//! Bounded i8042 / PS/2 Set 2 input for Mirror's single-CPU q35 platform.
//!
//! This is an explicitly selected platform driver, not physical-PC discovery.
//! IRQ1 only moves bytes into a fixed event queue. Main-context consumers own
//! printing and rendering; the periodic PIT also services deferred drain work.
//!
//! Register/command references (no source code copied):
//! - Intel EC keyboard-controller host documentation:
//!   https://intel.github.io/ecfw-zephyr/reference/kbchost/index.html
//! - QEMU's target controller and keyboard protocol implementations:
//!   https://github.com/qemu/qemu/blob/v10.1.0/hw/input/pckbd.c
//!   https://github.com/qemu/qemu/blob/v10.1.0/hw/input/ps2.c

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;

const DATA: u16 = 0x60;
const STATUS_COMMAND: u16 = 0x64;
const OUTPUT_FULL: u8 = 1;
const INPUT_FULL: u8 = 2;
const AUXILIARY: u8 = 0x20;
const RECEIVE_ERROR: u8 = 0xc0;
const POLL_LIMIT: usize = 100_000;
const INIT_DRAIN_LIMIT: usize = 256;
const DRAIN_LIMIT: usize = 32;
const QUEUE_CAPACITY: usize = 64;
const RESEND_LIMIT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Unshifted US-layout ASCII; modifiers belong to the event separately.
    Character(u8),
    Enter,
    Backspace,
    Tab,
    Escape,
    Left,
    Right,
    Home,
    End,
    Delete,
    Shift,
    Control,
    Alt,
    CapsLock,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub caps_lock: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub pressed: bool,
    pub repeat: bool,
    pub modifiers: Modifiers,
}

impl KeyEvent {
    /// Printable US ASCII only; control keys remain typed events.
    pub fn ascii(self) -> Option<u8> {
        if !self.pressed || self.modifiers.control || self.modifiers.alt {
            return None;
        }
        let Key::Character(mut byte) = self.key else {
            return None;
        };
        if byte.is_ascii_lowercase() {
            if self.modifiers.shift ^ self.modifiers.caps_lock {
                byte = byte.to_ascii_uppercase();
            }
        } else if self.modifiers.shift {
            byte = match byte {
                b'1' => b'!',
                b'2' => b'@',
                b'3' => b'#',
                b'4' => b'$',
                b'5' => b'%',
                b'6' => b'^',
                b'7' => b'&',
                b'8' => b'*',
                b'9' => b'(',
                b'0' => b')',
                b'-' => b'_',
                b'=' => b'+',
                b'[' => b'{',
                b']' => b'}',
                b'\\' => b'|',
                b';' => b':',
                b'\'' => b'"',
                b',' => b'<',
                b'.' => b'>',
                b'/' => b'?',
                b'`' => b'~',
                other => other,
            };
        }
        (b' '..=b'~').contains(&byte).then_some(byte)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardError {
    InterruptsEnabled,
    AlreadyInitialized,
    ControllerMissing,
    InputTimeout,
    OutputTimeout,
    OutputStuck,
    ControllerData,
    ControllerTest,
    KeyboardPortTest,
    Configuration,
    DeviceRejected,
    ResendLimit,
    WrongScanSet,
}

struct Decoder {
    extended: bool,
    released: bool,
    pause_remaining: u8,
    held: [u64; 8],
    caps_lock: bool,
    recovering: bool,
}

impl Decoder {
    const fn new() -> Self {
        Self {
            extended: false,
            released: false,
            pause_remaining: 0,
            held: [0; 8],
            caps_lock: false,
            recovering: false,
        }
    }

    fn is_held(&self, index: usize) -> bool {
        self.held[index / 64] & (1_u64 << (index % 64)) != 0
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers {
            shift: self.is_held(0x12) || self.is_held(0x59),
            control: self.is_held(0x14) || self.is_held(0x114),
            alt: self.is_held(0x11) || self.is_held(0x111),
            caps_lock: self.caps_lock,
        }
    }

    /// Loss discards held state, including lock state. Until a complete known
    /// key release arrives, makes are ignored: release and press again to resume.
    /// Keep any opaque Pause tail so it cannot manufacture a Control press.
    fn lose_input(&mut self) {
        self.held.fill(0);
        self.caps_lock = false;
        self.recovering = true;
    }

    fn corrupt_byte(&mut self) {
        self.extended = false;
        self.released = false;
        self.pause_remaining = self.pause_remaining.saturating_sub(1);
    }

    fn feed(&mut self, byte: u8) -> Decoded {
        if self.pause_remaining != 0 {
            // Pause has no ordinary make/break pair. Consume its complete
            // seven-byte tail even when a damaged sequence fails to match it.
            self.pause_remaining -= 1;
            return Decoded::None;
        }
        match byte {
            0xe1 => {
                self.extended = false;
                self.released = false;
                self.pause_remaining = 7;
                return Decoded::None;
            }
            0xe0 if !self.extended && !self.released => {
                self.extended = true;
                return Decoded::None;
            }
            0xf0 if !self.released => {
                self.released = true;
                return Decoded::None;
            }
            // Device replies, BAT/error bytes, and impossible doubled prefixes
            // are never keys. They require the same conservative loss recovery.
            0x00 | 0xff | 0xaa | 0xfa | 0xfc | 0xfd | 0xfe | 0xe0 | 0xf0 => {
                self.extended = false;
                self.released = false;
                return Decoded::Lost;
            }
            _ => {}
        }
        let extended = core::mem::take(&mut self.extended);
        let released = core::mem::take(&mut self.released);
        let Some(key) = decode_key(byte, extended) else {
            // This includes every PrintScreen fragment (E0 12 / E0 7C), so
            // its fake shifts cannot change the real left/right Shift state.
            return Decoded::None;
        };
        if self.recovering {
            if released {
                self.recovering = false;
            }
            return Decoded::None;
        }
        let index = usize::from(byte) + if extended { 256 } else { 0 };
        let was_held = self.is_held(index);
        if released && !was_held {
            return Decoded::None;
        }
        let bit = 1_u64 << (index % 64);
        if released {
            self.held[index / 64] &= !bit;
        } else {
            self.held[index / 64] |= bit;
            if key == Key::CapsLock && !was_held {
                self.caps_lock = !self.caps_lock;
            }
        }
        Decoded::Event(KeyEvent {
            key,
            pressed: !released,
            repeat: !released && was_held,
            modifiers: self.modifiers(),
        })
    }
}

enum Decoded {
    None,
    Lost,
    Event(KeyEvent),
}

fn decode_key(byte: u8, extended: bool) -> Option<Key> {
    if extended {
        return Some(match byte {
            0x14 => Key::Control,
            0x11 => Key::Alt,
            0x5a => Key::Enter,
            0x6b => Key::Left,
            0x74 => Key::Right,
            0x6c => Key::Home,
            0x69 => Key::End,
            0x71 => Key::Delete,
            _ => return None,
        });
    }
    let character = match byte {
        0x1c => b'a',
        0x32 => b'b',
        0x21 => b'c',
        0x23 => b'd',
        0x24 => b'e',
        0x2b => b'f',
        0x34 => b'g',
        0x33 => b'h',
        0x43 => b'i',
        0x3b => b'j',
        0x42 => b'k',
        0x4b => b'l',
        0x3a => b'm',
        0x31 => b'n',
        0x44 => b'o',
        0x4d => b'p',
        0x15 => b'q',
        0x2d => b'r',
        0x1b => b's',
        0x2c => b't',
        0x3c => b'u',
        0x2a => b'v',
        0x1d => b'w',
        0x22 => b'x',
        0x35 => b'y',
        0x1a => b'z',
        0x16 => b'1',
        0x1e => b'2',
        0x26 => b'3',
        0x25 => b'4',
        0x2e => b'5',
        0x36 => b'6',
        0x3d => b'7',
        0x3e => b'8',
        0x46 => b'9',
        0x45 => b'0',
        0x0e => b'`',
        0x4e => b'-',
        0x55 => b'=',
        0x54 => b'[',
        0x5b => b']',
        0x5d => b'\\',
        0x4c => b';',
        0x52 => b'\'',
        0x41 => b',',
        0x49 => b'.',
        0x4a => b'/',
        0x29 => b' ',
        _ => {
            return Some(match byte {
                0x5a => Key::Enter,
                0x66 => Key::Backspace,
                0x0d => Key::Tab,
                0x76 => Key::Escape,
                0x12 | 0x59 => Key::Shift,
                0x14 => Key::Control,
                0x11 => Key::Alt,
                0x58 => Key::CapsLock,
                _ => return None,
            });
        }
    };
    Some(Key::Character(character))
}

struct State {
    initialized: bool,
    decoder: Decoder,
    queue: [Option<KeyEvent>; QUEUE_CAPACITY],
    head: usize,
    len: usize,
    overflow: bool,
    pending_drain: bool,
}

impl State {
    const fn new() -> Self {
        Self {
            initialized: false,
            decoder: Decoder::new(),
            queue: [None; QUEUE_CAPACITY],
            head: 0,
            len: 0,
            overflow: false,
            pending_drain: false,
        }
    }

    fn lose_input(&mut self) {
        self.queue.fill(None);
        self.head = 0;
        self.len = 0;
        self.overflow = true;
        self.decoder.lose_input();
    }

    fn accept(&mut self, status: u8, byte: u8) {
        if status & AUXILIARY != 0 {
            return;
        }
        if status & RECEIVE_ERROR != 0 {
            self.decoder.corrupt_byte();
            self.lose_input();
            return;
        }
        match self.decoder.feed(byte) {
            Decoded::None => {}
            Decoded::Lost => self.lose_input(),
            Decoded::Event(event) => {
                if self.len == QUEUE_CAPACITY {
                    self.lose_input();
                } else {
                    let tail = (self.head + self.len) % QUEUE_CAPACITY;
                    self.queue[tail] = Some(event);
                    self.len += 1;
                }
            }
        }
    }

    fn pop(&mut self) -> Option<KeyEvent> {
        // The console must observe loss before it can consume subsequent input.
        if self.overflow || self.len == 0 {
            return None;
        }
        let event = self.queue[self.head].take();
        self.head = (self.head + 1) % QUEUE_CAPACITY;
        self.len -= 1;
        event
    }
}

struct SharedState(UnsafeCell<State>);

// SAFETY: Mirror currently starts only its BSP. Every access has IF disabled,
// and no reference escapes that critical section or is used by a nested IRQ.
unsafe impl Sync for SharedState {}

static STATE: SharedState = SharedState(UnsafeCell::new(State::new()));
static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

fn with_state<R>(operation: impl FnOnce(&mut State) -> R) -> R {
    let enabled = arch::save_and_disable_interrupts();
    // SAFETY: BSP-only IF exclusion grants exclusive access for this closure.
    let result = operation(unsafe { &mut *STATE.0.get() });
    // SAFETY: The borrow ended above, and previously active handlers stay valid.
    unsafe { arch::restore_interrupts(enabled) };
    result
}

/// Initialize the selected q35 controller with IF clear and PIC IRQ1 masked.
/// Call after timer initialization; only the caller unmasks IRQ1 after success.
/// No controller output-port, A20, CPU-reset, or auxiliary-device command is used.
pub fn init() -> Result<(), KeyboardError> {
    if arch::interrupts_enabled() {
        return Err(KeyboardError::InterruptsEnabled);
    }
    with_state(|state| {
        if state.initialized {
            return Err(KeyboardError::AlreadyInitialized);
        }
        let mut quiet_config = None;
        let result = initialize_controller(&mut quiet_config);
        if result.is_ok() {
            *state = State::new();
            state.initialized = true;
            IRQ_COUNT.store(0, Ordering::Relaxed);
        } else {
            // Best-effort bounded quieting. IRQ1 stays masked at the PIC on
            // every failed init, even if an absent/broken controller ignores us.
            let _ = command(0xad);
            let _ = command(0xa7);
            if let Some(config) = quiet_config {
                let _ = write_config(config);
            }
        }
        result
    })
}

fn initialize_controller(quiet_config: &mut Option<u8>) -> Result<(), KeyboardError> {
    command(0xad)?;
    command(0xa7)?;
    drain_initial_output()?;
    let original = read_config()?;
    // Preserve the platform's system/keylock bits, but clear reserved bits,
    // translation, and both IRQ enables, while disabling both interfaces.
    let quiet = (original & 0x0c) | 0x30;
    *quiet_config = Some(quiet);
    write_config(quiet)?;
    command(0xaa)?;
    if read_response()? != 0x55 {
        return Err(KeyboardError::ControllerTest);
    }
    // A controller self-test may reset its configuration. Reapply all policy.
    command(0xad)?;
    command(0xa7)?;
    write_config(quiet)?;
    drain_initial_output()?;
    command(0xab)?;
    if read_response()? != 0 {
        return Err(KeyboardError::KeyboardPortTest);
    }
    command(0xae)?;
    write_config(quiet & !0x10)?;
    verify_config(quiet & !0x10)?;
    keyboard_command(0xf5)?;
    keyboard_command(0xf0)?;
    keyboard_command(0x02)?;
    keyboard_command(0xf0)?;
    keyboard_command(0x00)?;
    if read_response()? != 2 {
        return Err(KeyboardError::WrongScanSet);
    }
    // Configure and verify IRQ routing while scanning is still disabled. IF
    // and the PIC mask exclude IRQ1 throughout these command transactions.
    let active = (quiet & !0x10) | 1;
    write_config(active)?;
    verify_config(active)?;
    keyboard_command(0xf4)?;
    // The enable ACK has been consumed. No further polling before IRQ handoff.
    Ok(())
}

fn status() -> u8 {
    // SAFETY: This BSP platform driver owns the i8042 status/data ports.
    unsafe { arch::in8(STATUS_COMMAND) }
}

fn data() -> u8 {
    // SAFETY: The caller observed OBF and this driver owns port 0x60.
    unsafe { arch::in8(DATA) }
}

fn wait_input_empty() -> Result<(), KeyboardError> {
    for _ in 0..POLL_LIMIT {
        let current = status();
        if current == 0xff {
            return Err(KeyboardError::ControllerMissing);
        }
        if current & INPUT_FULL == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(KeyboardError::InputTimeout)
}

fn command(value: u8) -> Result<(), KeyboardError> {
    wait_input_empty()?;
    // SAFETY: Owned controller command port with an empty input buffer.
    unsafe { arch::out8(STATUS_COMMAND, value) };
    Ok(())
}

fn write_data(value: u8) -> Result<(), KeyboardError> {
    wait_input_empty()?;
    // SAFETY: Owned controller data port with an empty input buffer.
    unsafe { arch::out8(DATA, value) };
    Ok(())
}

fn read_response() -> Result<u8, KeyboardError> {
    for _ in 0..POLL_LIMIT {
        let current = status();
        if current == 0xff {
            return Err(KeyboardError::ControllerMissing);
        }
        if current & OUTPUT_FULL != 0 {
            let byte = data();
            if current & AUXILIARY != 0 {
                continue;
            }
            if current & RECEIVE_ERROR != 0 {
                return Err(KeyboardError::ControllerData);
            }
            return Ok(byte);
        }
        core::hint::spin_loop();
    }
    Err(KeyboardError::OutputTimeout)
}

fn drain_initial_output() -> Result<(), KeyboardError> {
    for _ in 0..INIT_DRAIN_LIMIT {
        if status() & OUTPUT_FULL == 0 {
            return Ok(());
        }
        let _ = data();
    }
    if status() & OUTPUT_FULL == 0 {
        Ok(())
    } else {
        Err(KeyboardError::OutputStuck)
    }
}

fn read_config() -> Result<u8, KeyboardError> {
    command(0x20)?;
    read_response()
}

fn write_config(config: u8) -> Result<(), KeyboardError> {
    command(0x60)?;
    write_data(config)
}

fn verify_config(expected: u8) -> Result<(), KeyboardError> {
    if read_config()? & 0x73 != expected & 0x73 {
        return Err(KeyboardError::Configuration);
    }
    Ok(())
}

fn keyboard_command(value: u8) -> Result<(), KeyboardError> {
    for _ in 0..=RESEND_LIMIT {
        write_data(value)?;
        // A last scan byte can race F5. Discard a bounded number of such bytes;
        // FA/FE alone control this transaction. No command responses are queued.
        let mut resend = false;
        for _ in 0..DRAIN_LIMIT {
            match read_response()? {
                0xfa => return Ok(()),
                0xfe => {
                    resend = true;
                    break;
                }
                0xfc | 0xfd | 0xff => return Err(KeyboardError::DeviceRejected),
                _ => {}
            }
        }
        if !resend {
            return Err(KeyboardError::DeviceRejected);
        }
    }
    Err(KeyboardError::ResendLimit)
}

fn drain_controller(state: &mut State) {
    for _ in 0..DRAIN_LIMIT {
        let current = status();
        if current & OUTPUT_FULL == 0 {
            state.pending_drain = false;
            return;
        }
        let byte = data();
        if state.initialized {
            state.accept(current, byte);
        }
    }
    // Do not depend on another edge while OBF remains asserted. pop_event
    // continues bounded work on each PIT wake of the normal main loop.
    state.pending_drain = status() & OUTPUT_FULL != 0;
}

/// Called by the IRQ1 interrupt gate with IF clear, before the PIC EOI.
pub(crate) fn handle_irq() {
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    // SAFETY: The interrupt gate cleared IF on the sole BSP, so no other access
    // to STATE can run until this bounded handler returns.
    drain_controller(unsafe { &mut *STATE.0.get() });
}

/// Consume one typed event, preserving IF. A pending hardware drain processes
/// at most 32 more bytes. The PIT must keep waking main even when no event exists.
pub fn pop_event() -> Option<KeyEvent> {
    with_state(|state| {
        if state.initialized && state.pending_drain {
            drain_controller(state);
        }
        state.pop()
    })
}

/// Report and acknowledge queue overflow or damaged input. The consumer must
/// discard its unfinished input line. pop_event withholds events until this ack.
pub fn take_overflow() -> bool {
    with_state(|state| core::mem::take(&mut state.overflow))
}

/// Delivered IRQ1 handlers, including empty or command-ACK deliveries; this is
/// not a key count. Real input evidence also requires decoded key events.
pub fn irq_count() -> u64 {
    IRQ_COUNT.load(Ordering::Relaxed)
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    fn feed(state: &mut State, bytes: &[u8]) {
        for &byte in bytes {
            state.accept(OUTPUT_FULL, byte);
        }
    }
    fn event(state: &mut State) -> KeyEvent {
        state.pop().expect("missing keyboard test event")
    }
    fn drain(state: &mut State) {
        while state.pop().is_some() {}
    }

    // Boot calls this before hardware initialization, so these public consumer
    // paths must leave an empty queue and the caller's IF unchanged, without
    // touching controller ports. Deferred polling requires initialized state.
    let original_if = arch::interrupts_enabled();
    assert!(pop_event().is_none());
    assert_eq!(arch::interrupts_enabled(), original_if);
    assert!(!take_overflow());
    assert_eq!(arch::interrupts_enabled(), original_if);

    let mut state = State::new();
    feed(&mut state, &[0x1c, 0x1c, 0xf0, 0x1c]);
    let first = event(&mut state);
    assert_eq!(first.ascii(), Some(b'a'));
    assert!(!first.repeat);
    assert!(event(&mut state).repeat);
    let released = event(&mut state);
    assert!(!released.pressed);
    assert_eq!(released.ascii(), None);
    assert!(state.pop().is_none());

    // Per-side state, split prefixes, shifted punctuation, and independent
    // left/right Control and Alt all use the same decoder as real IRQ bytes.
    feed(&mut state, &[0x12, 0x59, 0xf0]);
    feed(&mut state, &[0x12, 0x1c, 0x16]);
    drain(&mut state);
    assert!(state.decoder.modifiers().shift);
    feed(&mut state, &[0x1c, 0x16]);
    assert_eq!(event(&mut state).ascii(), Some(b'A'));
    assert_eq!(event(&mut state).ascii(), Some(b'!'));
    feed(&mut state, &[0xf0, 0x59, 0x14, 0xe0]);
    feed(&mut state, &[0x14, 0xf0, 0x14, 0x1c]);
    drain(&mut state);
    assert!(state.decoder.modifiers().control);
    feed(&mut state, &[0x1c]);
    assert_eq!(event(&mut state).ascii(), None);
    feed(
        &mut state,
        &[0xe0, 0xf0, 0x14, 0x11, 0xe0, 0x11, 0xf0, 0x11],
    );
    drain(&mut state);
    assert!(state.decoder.modifiers().alt);
    feed(&mut state, &[0x1c]);
    assert_eq!(event(&mut state).ascii(), None);
    feed(&mut state, &[0xe0, 0xf0, 0x11]);
    drain(&mut state);
    assert_eq!(state.decoder.modifiers(), Modifiers::default());

    feed(&mut state, &[0x58, 0x58, 0x1c]);
    assert!(event(&mut state).modifiers.caps_lock);
    let caps_repeat = event(&mut state);
    assert!(caps_repeat.repeat && caps_repeat.modifiers.caps_lock);
    assert_eq!(event(&mut state).ascii(), Some(b'A'));
    feed(&mut state, &[0x12, 0x1c, 0xf0, 0x12, 0xf0, 0x58, 0x58]);
    let _ = event(&mut state);
    assert_eq!(event(&mut state).ascii(), Some(b'a'));
    drain(&mut state);
    assert!(!state.decoder.modifiers().caps_lock);

    // Pause and PrintScreen must not generate events or alter held modifiers.
    let before = state.decoder.modifiers();
    feed(&mut state, &[0xe1, 0x14, 0x77, 0xe1]);
    feed(&mut state, &[0xf0, 0x14, 0xf0, 0x77]);
    feed(&mut state, &[0xe0, 0x12, 0xe0, 0x7c, 0xe0, 0xf0, 0x7c]);
    feed(&mut state, &[0xe0, 0xf0, 0x12, 0xe0, 0x59, 0x7f]);
    assert!(state.pop().is_none());
    assert_eq!(state.decoder.modifiers(), before);
    feed(&mut state, &[0xe0, 0x6b, 0xe0, 0xf0]);
    feed(&mut state, &[0x6b]);
    assert_eq!(event(&mut state).key, Key::Left);
    assert!(!event(&mut state).pressed);

    // AUX bytes, including errored AUX bytes, never enter keyboard decoding.
    state.accept(OUTPUT_FULL | AUXILIARY, 0x12);
    state.accept(OUTPUT_FULL | AUXILIARY | RECEIVE_ERROR, 0x14);
    assert!(state.pop().is_none());
    assert!(!state.overflow);

    // Wrap the queue several times, then prove overflow removes all stale
    // events and modifiers and gates new input on an observed release boundary.
    state = State::new();
    for _ in 0..QUEUE_CAPACITY * 3 {
        feed(&mut state, &[0x1c]);
        assert_eq!(event(&mut state).ascii(), Some(b'a'));
    }
    feed(&mut state, &[0x12]);
    for _ in 0..QUEUE_CAPACITY {
        feed(&mut state, &[0x1c]);
    }
    assert!(state.overflow && state.len == 0);
    assert_eq!(state.decoder.modifiers(), Modifiers::default());
    feed(&mut state, &[0x12, 0x1c]);
    assert_eq!(state.len, 0);
    feed(&mut state, &[0xf0, 0x12, 0x1c]);
    assert!(state.pop().is_none());
    state.overflow = false;
    assert_eq!(event(&mut state).ascii(), Some(b'a'));

    // Corruption halfway through Pause still consumes its opaque tail; no fake
    // Control make or suffix text can escape the recovery gate.
    feed(&mut state, &[0xe1, 0x14]);
    state.accept(OUTPUT_FULL | RECEIVE_ERROR, 0x77);
    feed(&mut state, &[0xe1, 0xf0, 0x14, 0xf0, 0x77, 0x12, 0x1c]);
    assert_eq!(state.decoder.modifiers(), Modifiers::default());
    assert!(state.overflow && state.len == 0);
    feed(&mut state, &[0xf0, 0x1c, 0x1c]);
    state.overflow = false;
    assert_eq!(event(&mut state).ascii(), Some(b'a'));
    for response in [0x00, 0xff, 0xaa, 0xfa, 0xfc, 0xfd, 0xfe] {
        feed(&mut state, &[response, 0x12, 0x1c]);
        assert!(state.overflow && state.len == 0);
        assert_eq!(state.decoder.modifiers(), Modifiers::default());
        feed(&mut state, &[0xf0, 0x1c]);
        state.overflow = false;
    }
    crate::serial::println!("MIRROR_KEYBOARD_TESTS_OK");
}
