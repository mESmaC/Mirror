//! Bounded PS/2 Set 2 keyboard decoding and typed events.
//!
//! This is an explicitly selected platform driver, not physical-PC discovery.
//! The shared ps2 controller owns all port I/O and routes keyboard bytes here.
//! IRQ handlers only decode into a fixed queue; main owns printing/rendering.
//!
//! Register/command references (no source code copied):
//! - Intel EC keyboard-controller host documentation:
//!   https://intel.github.io/ecfw-zephyr/reference/kbchost/index.html
//! - QEMU's target controller and keyboard protocol implementations:
//!   https://github.com/qemu/qemu/blob/v10.1.0/hw/input/pckbd.c
//!   https://github.com/qemu/qemu/blob/v10.1.0/hw/input/ps2.c

use core::cell::UnsafeCell;

use crate::{arch, ps2};

#[cfg(feature = "boot-smoke")]
const OUTPUT_FULL: u8 = 1;
const AUXILIARY: u8 = 0x20;
const RECEIVE_ERROR: u8 = 0xc0;
pub(crate) const QUEUE_CAPACITY: usize = 64;

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
    queue: [Option<(u64, KeyEvent)>; QUEUE_CAPACITY],
    head: usize,
    len: usize,
    /// Sequence of the first keyboard loss since the consumer last took this
    /// device's loss marker (spec 0017a-D3). Every queued event is newer.
    lost: Option<u64>,
}

impl State {
    const fn new() -> Self {
        Self {
            initialized: false,
            decoder: Decoder::new(),
            queue: [None; QUEUE_CAPACITY],
            head: 0,
            len: 0,
            lost: None,
        }
    }

    fn clear_queue(&mut self) {
        self.queue.fill(None);
        self.head = 0;
        self.len = 0;
    }

    /// A keyboard fault or ring overflow drops only this device's queue and
    /// key state. Its marker takes the next shared sequence unless an
    /// unacknowledged loss already holds one; mouse events are untouched.
    fn lose_input(&mut self) {
        self.clear_queue();
        if self.lost.is_none() {
            self.lost = Some(ps2::next_sequence());
        }
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
                    self.queue[tail] = Some((ps2::next_sequence(), event));
                    self.len += 1;
                }
            }
        }
    }

    fn pop(&mut self) -> Option<KeyEvent> {
        // The console must observe loss before it can consume subsequent input.
        if self.lost.is_some() || self.len == 0 {
            return None;
        }
        let event = self.queue[self.head].take().map(|(_, event)| event);
        self.head = (self.head + 1) % QUEUE_CAPACITY;
        self.len -= 1;
        event
    }

    /// The pending loss marker sorts ahead of the queue head: every queued
    /// event was published after the loss that cleared the queue.
    fn peek_sequence(&self) -> Option<u64> {
        if self.lost.is_some() {
            self.lost
        } else if self.len == 0 {
            None
        } else {
            self.queue[self.head].map(|(sequence, _)| sequence)
        }
    }

    fn take_lost(&mut self) -> bool {
        self.lost.take().is_some()
    }

    fn discard_input(&mut self) {
        self.clear_queue();
        self.decoder.lose_input();
        self.decoder.extended = false;
        self.decoder.released = false;
        self.lost = None;
    }
}

struct SharedState(UnsafeCell<State>);

// SAFETY: Mirror currently starts only its BSP. Every access has IF disabled,
// and no reference escapes that critical section or is used by a nested IRQ.
unsafe impl Sync for SharedState {}

static STATE: SharedState = SharedState(UnsafeCell::new(State::new()));

fn with_state<R>(operation: impl FnOnce(&mut State) -> R) -> R {
    let enabled = arch::save_and_disable_interrupts();
    // SAFETY: BSP-only IF exclusion grants exclusive access for this closure.
    let result = operation(unsafe { &mut *STATE.0.get() });
    // SAFETY: The borrow ended above, and previously active handlers stay valid.
    unsafe { arch::restore_interrupts(enabled) };
    result
}

/// Publish a fresh decoder after the controller consumed the final enable ACK.
/// The shared controller calls this once with IF clear, before PIC unmasking.
pub(crate) fn activate() {
    assert!(!arch::interrupts_enabled());
    with_state(|state| {
        assert!(!state.initialized, "keyboard already active");
        *state = State::new();
        state.initialized = true;
    });
}

/// The controller owns the status/data transaction and calls this with IF clear.
/// Never call back into ps2 while the keyboard state is borrowed.
pub(crate) fn accept_byte(status: u8, byte: u8) {
    debug_assert!(!arch::interrupts_enabled());
    // SAFETY: The single-BSP controller excludes interrupts and every consumer
    // ends its device-state borrow before calling into the controller again.
    let state = unsafe { &mut *STATE.0.get() };
    if state.initialized {
        state.accept(status, byte);
    }
}

/// Consume one typed event without touching controller ports, preserving IF.
/// The shared consumer handles deferred polling before borrowing device state.
pub(crate) fn pop_event() -> Option<KeyEvent> {
    with_state(State::pop)
}

pub(crate) fn peek_sequence() -> Option<u64> {
    with_state(|state| state.peek_sequence())
}

/// Controller handoff only (ps2 stop_routing): drop queued events and any
/// pending loss. Preserve an opaque Pause tail, but discard prefixes/held keys
/// and require a new release boundary.
pub(crate) fn discard_input() {
    with_state(State::discard_input);
}

/// Report and acknowledge queue overflow or damaged keyboard input. The
/// consumer must discard its unfinished input line; pointer state is
/// unaffected. pop_event withholds events until this ack.
pub(crate) fn take_lost() -> bool {
    with_state(State::take_lost)
}

/// Delivered IRQ1 handlers, including empty or command-ACK deliveries; this is
/// not a key count. Real input evidence also requires decoded key events.
pub fn irq_count() -> u64 {
    ps2::keyboard_irq_count()
}

/// Exercise the real merged consumer before controller initialization. No
/// device-state borrow spans the callback, which may use all consumer paths.
#[cfg(feature = "boot-smoke")]
pub(crate) fn with_test_input(operation: impl FnOnce()) {
    assert!(!arch::interrupts_enabled());
    activate(); // Also asserts the real device has not already been activated.
    operation();
    assert!(!arch::interrupts_enabled());
    with_state(|state| *state = State::new());
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
    // touching controller ports. Only ps2::next_event performs deferred polling.
    let original_if = arch::interrupts_enabled();
    assert!(pop_event().is_none());
    assert_eq!(arch::interrupts_enabled(), original_if);
    assert!(!take_lost());
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
    assert!(state.lost.is_none());

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
    assert!(state.lost.is_some() && state.len == 0);
    assert_eq!(state.decoder.modifiers(), Modifiers::default());
    feed(&mut state, &[0x12, 0x1c]);
    assert_eq!(state.len, 0);
    feed(&mut state, &[0xf0, 0x12, 0x1c]);
    assert!(state.pop().is_none());
    state.lost = None;
    assert_eq!(event(&mut state).ascii(), Some(b'a'));

    // Corruption halfway through Pause still consumes its opaque tail; no fake
    // Control make or suffix text can escape the recovery gate.
    feed(&mut state, &[0xe1, 0x14]);
    state.accept(OUTPUT_FULL | RECEIVE_ERROR, 0x77);
    feed(&mut state, &[0xe1, 0xf0, 0x14, 0xf0, 0x77, 0x12, 0x1c]);
    assert_eq!(state.decoder.modifiers(), Modifiers::default());
    assert!(state.lost.is_some() && state.len == 0);
    feed(&mut state, &[0xf0, 0x1c, 0x1c]);
    state.lost = None;
    assert_eq!(event(&mut state).ascii(), Some(b'a'));
    for response in [0x00, 0xff, 0xaa, 0xfa, 0xfc, 0xfd, 0xfe] {
        feed(&mut state, &[response, 0x12, 0x1c]);
        assert!(state.lost.is_some() && state.len == 0);
        assert_eq!(state.decoder.modifiers(), Modifiers::default());
        feed(&mut state, &[0xf0, 0x1c]);
        state.lost = None;
    }

    // A loss marker sorts ahead of the queue head with its own sequence. A
    // second loss before the consumer takes it keeps the first sequence, and
    // text queued after recovery is delivered behind the marker (0017a-D3).
    state = State::new();
    feed(&mut state, &[0x1c]);
    state.accept(OUTPUT_FULL | RECEIVE_ERROR, 0x00);
    let first = state.peek_sequence().expect("keyboard loss marker");
    feed(&mut state, &[0xf0, 0x1c, 0x1c]);
    assert_eq!(state.len, 1);
    feed(&mut state, &[0xff]);
    assert_eq!((state.peek_sequence(), state.len), (Some(first), 0));
    feed(&mut state, &[0xf0, 0x1c, 0x1c]);
    let after = state.queue[state.head].map(|(sequence, _)| sequence);
    assert!(after.is_some_and(|sequence| (first.wrapping_sub(sequence) as i64) < 0));
    assert!(state.pop().is_none());
    assert!(state.take_lost());
    assert!(!state.take_lost());
    assert_eq!(state.peek_sequence(), after);
    assert_eq!(event(&mut state).ascii(), Some(b'a'));
    assert!(state.peek_sequence().is_none());

    // Controller handoff (ps2 stop_routing) may interrupt any keyboard
    // prefix. It clears old modifiers/queued text and any pending loss, and
    // withholds makes until an ordinary release.
    state = State::new();
    feed(&mut state, &[0x12, 0x1c, 0xe0, 0xf0]);
    assert!(state.peek_sequence().is_some());
    state.discard_input();
    assert!(state.peek_sequence().is_none());
    assert!(state.lost.is_none());
    assert_eq!(state.decoder.modifiers(), Modifiers::default());
    feed(&mut state, &[0x14, 0x1c]);
    assert!(state.pop().is_none());
    feed(&mut state, &[0xf0, 0x1c, 0x1c]);
    assert_eq!(event(&mut state).ascii(), Some(b'a'));

    // Preserve the complete remaining Pause tail through controller handoff:
    // its embedded break must not prematurely release the recovery gate.
    feed(&mut state, &[0xe1, 0x14]);
    assert_eq!(state.decoder.pause_remaining, 6);
    state.discard_input();
    feed(
        &mut state,
        &[0x77, 0xe1, 0xf0, 0x14, 0xf0, 0x77, 0x12, 0x1c],
    );
    assert!(state.pop().is_none());
    assert_eq!(state.decoder.modifiers(), Modifiers::default());
    feed(&mut state, &[0xf0, 0x1c, 0x1c]);
    assert_eq!(event(&mut state).ascii(), Some(b'a'));
    crate::serial::println!("MIRROR_KEYBOARD_TESTS_OK");
}
