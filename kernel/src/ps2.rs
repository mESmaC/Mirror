//! Sole i8042 controller owner for the single-BSP, explicitly selected q35 PC.
//!
//! Every status/data pair is routed by AUX, never by the IRQ that caused the
//! drain. Device decoders own separate state and never call this module while
//! borrowed. Initialization runs with both PIC input IRQs masked and IF clear;
//! runtime commands, hotplug, ACPI discovery and output-port/A20 writes are absent.
//!
//! Protocol references (no implementation copied):
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/input/pckbd.c
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/input/ps2.c
//! https://intel.github.io/ecfw-zephyr/reference/kbchost/index.html

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{arch, keyboard, mouse};

const DATA: u16 = 0x60;
const STATUS_COMMAND: u16 = 0x64;
const OUTPUT_FULL: u8 = 1;
const INPUT_FULL: u8 = 2;
const AUXILIARY: u8 = 0x20;
const RECEIVE_ERROR: u8 = 0xc0;
const POLL_LIMIT: usize = 100_000;
const INIT_DRAIN_LIMIT: usize = 256;
const DRAIN_LIMIT: usize = 32;
const RESEND_LIMIT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ps2Error {
    InterruptsEnabled,
    AlreadyInitialized,
    ControllerMissing,
    InputTimeout,
    OutputTimeout,
    OutputStuck,
    ControllerData,
    ControllerTest,
    KeyboardPortTest,
    MousePortTest,
    Configuration,
    DeviceRejected,
    ResendLimit,
    WrongScanSet,
    MouseSelfTest,
    UnsupportedMouse,
}

#[derive(Clone, Copy, Debug)]
pub struct InitResult {
    pub keyboard: Result<(), Ps2Error>,
    pub mouse: Result<(), Ps2Error>,
}

impl InitResult {
    fn failed(error: Ps2Error) -> Self {
        Self {
            keyboard: Err(error),
            mouse: Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Keyboard,
    Mouse,
}

impl Source {
    fn from_status(status: u8) -> Self {
        if status & AUXILIARY == 0 {
            Self::Keyboard
        } else {
            Self::Mouse
        }
    }
}

struct State {
    attempted: bool,
    keyboard_ready: bool,
    mouse_ready: bool,
    pending_drain: bool,
}

impl State {
    const fn new() -> Self {
        Self {
            attempted: false,
            keyboard_ready: false,
            mouse_ready: false,
            pending_drain: false,
        }
    }

    fn route(&self, status: u8, byte: u8) {
        match Source::from_status(status) {
            Source::Keyboard if self.keyboard_ready => keyboard::accept_byte(status, byte),
            Source::Mouse if self.mouse_ready => mouse::accept_byte(status, byte),
            _ => {}
        }
    }

    fn stop_routing(&mut self) {
        self.keyboard_ready = false;
        self.mouse_ready = false;
        self.pending_drain = false;
        keyboard::discard_input();
        mouse::discard_input();
    }
}

struct SharedState(UnsafeCell<State>);

// SAFETY: Only the BSP runs; every borrow excludes maskable interrupts and
// ends before restoring IF. Device callbacks never reenter controller state.
unsafe impl Sync for SharedState {}

static STATE: SharedState = SharedState(UnsafeCell::new(State::new()));
static KEYBOARD_IRQS: AtomicU64 = AtomicU64::new(0);
static MOUSE_IRQS: AtomicU64 = AtomicU64::new(0);
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key(keyboard::KeyEvent),
    Mouse(mouse::MouseEvent),
    Lost,
}

/// A device stamps only a completed, queued event. This atomic has no controller
/// borrow, so it is safe from either decoder callback under IF exclusion.
pub(crate) fn next_sequence() -> u64 {
    NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn keyboard_first(keyboard: u64, mouse: u64) -> bool {
    // At most 128 events can be queued. No two live sequence numbers can differ
    // by half the u64 range, including across the counter's natural wrap.
    (keyboard.wrapping_sub(mouse) as i64) < 0
}

/// Drain once, then consume the oldest keyboard/mouse event under one IF guard.
/// Loss is a shared ordering barrier: acknowledge both flags and discard both
/// queues before delivering any later event. Device pop functions perform no I/O.
pub fn next_event() -> Option<InputEvent> {
    let enabled = arch::save_and_disable_interrupts();
    // poll_pending ends its controller borrow before any device queue is read.
    poll_pending();
    let keyboard_lost = keyboard::take_overflow();
    let mouse_lost = mouse::take_overflow();
    let result = if keyboard_lost || mouse_lost {
        keyboard::discard_input();
        mouse::discard_input();
        Some(InputEvent::Lost)
    } else {
        match (keyboard::peek_sequence(), mouse::peek_sequence()) {
            (Some(key), Some(mouse)) if keyboard_first(key, mouse) => {
                keyboard::pop_event().map(InputEvent::Key)
            }
            (Some(_), Some(_)) | (None, Some(_)) => mouse::pop_event().map(InputEvent::Mouse),
            (Some(_), None) => keyboard::pop_event().map(InputEvent::Key),
            (None, None) => None,
        }
    };
    // SAFETY: Controller and both device borrows ended; initialized handlers
    // see consistent queues if the caller previously enabled interrupts.
    unsafe { arch::restore_interrupts(enabled) };
    result
}

fn with_state<R>(operation: impl FnOnce(&mut State) -> R) -> R {
    let enabled = arch::save_and_disable_interrupts();
    // SAFETY: Single-BSP IF exclusion grants this closure exclusive access.
    let result = operation(unsafe { &mut *STATE.0.get() });
    // SAFETY: All controller/device borrows ended; installed IRQ handlers remain valid.
    unsafe { arch::restore_interrupts(enabled) };
    result
}

/// Initialize once with IF clear and PIC IRQ1/IRQ12 masked. The caller unmasks
/// only successful devices afterward. Mouse failure leaves a prepared keyboard
/// available, and failed devices remain inactive in runtime byte routing.
pub fn init() -> InitResult {
    if arch::interrupts_enabled() {
        return InitResult::failed(Ps2Error::InterruptsEnabled);
    }
    with_state(|state| {
        if state.attempted {
            return InitResult::failed(Ps2Error::AlreadyInitialized);
        }
        state.attempted = true;
        let mut quiet_config = None;
        match initialize(state, &mut quiet_config) {
            Ok(result) => result,
            Err(error) => {
                // A final mouse-enable failure can reveal a broken controller
                // after keyboard activation. Revoke both routes/queues before
                // best-effort quieting; neither PIC input IRQ may be unmasked.
                state.stop_routing();
                let _ = command(0xad);
                let _ = command(0xa7);
                if let Some(quiet) = quiet_config {
                    let _ = write_config(quiet);
                }
                InitResult::failed(error)
            }
        }
    })
}

fn configuration(original: u8, keyboard: bool, mouse: bool, irqs: bool) -> u8 {
    // Preserve only system/keylock policy. Clear translation/reserved bits;
    // each failed interface stays disabled with its interrupt bit clear.
    (original & 0x0c)
        | if keyboard { u8::from(irqs) } else { 0x10 }
        | if mouse { u8::from(irqs) << 1 } else { 0x20 }
}

fn initialize(state: &mut State, quiet_config: &mut Option<u8>) -> Result<InitResult, Ps2Error> {
    command(0xad)?;
    command(0xa7)?;
    drain_initial_output()?;
    let quiet = configuration(read_config(state)?, false, false, false);
    *quiet_config = Some(quiet);
    write_config(quiet)?;
    command(0xaa)?;
    if read_response(state, Source::Keyboard)? != 0x55 {
        return Err(Ps2Error::ControllerTest);
    }
    // Self-test may reset configuration. Reapply all routing policy first.
    command(0xad)?;
    command(0xa7)?;
    write_config(quiet)?;
    drain_initial_output()?;

    let mut keyboard = prepare_keyboard(state, quiet);
    if keyboard.is_err() {
        command(0xad)?;
        write_config(quiet)?;
    }
    let mut mouse = prepare_mouse(state, quiet, keyboard.is_ok());
    if mouse.is_err() {
        command(0xa7)?;
    }
    let mut active = configuration(quiet, keyboard.is_ok(), mouse.is_ok(), true);
    write_config(active)?;
    verify_config(state, active)?;

    // No config reads after keyboard scanning begins: controller replies have
    // the same source tag as scan bytes. Mouse commands can still distinguish
    // and deliver every intervening keyboard byte to its now-active decoder.
    if keyboard.is_ok() {
        keyboard = device_command(state, Source::Keyboard, 0xf4);
        if keyboard.is_ok() {
            keyboard::activate();
            state.keyboard_ready = true;
        } else {
            command(0xad)?;
            active = configuration(quiet, false, mouse.is_ok(), true);
            write_config(active)?;
            verify_config(state, active)?;
        }
    }
    if mouse.is_ok() {
        mouse = device_command(state, Source::Mouse, 0xf4);
        if mouse.is_ok() {
            mouse::activate();
            state.mouse_ready = true;
        } else {
            // Preserve the working keyboard even when the mouse does not ACK.
            // If quieting fails, revoke the whole controller: live AUX data
            // with IRQ12 masked could occupy OBF and prevent future IRQ1 input.
            command(0xa7)?;
            write_config(configuration(active, state.keyboard_ready, false, true))?;
        }
    }
    // Drain even if an enable ACK consumed the IRQ's edge. Future IRQs and PIT
    // wakeups continue bounded work until the shared output buffer is empty.
    state.pending_drain = state.keyboard_ready || state.mouse_ready;
    Ok(InitResult { keyboard, mouse })
}

fn prepare_keyboard(state: &State, quiet: u8) -> Result<(), Ps2Error> {
    command(0xab)?;
    if read_response(state, Source::Keyboard)? != 0 {
        return Err(Ps2Error::KeyboardPortTest);
    }
    command(0xae)?;
    let config = configuration(quiet, true, false, false);
    write_config(config)?;
    verify_config(state, config)?;
    device_command(state, Source::Keyboard, 0xf5)?;
    device_command(state, Source::Keyboard, 0xf0)?;
    device_command(state, Source::Keyboard, 0x02)?;
    device_command(state, Source::Keyboard, 0xf0)?;
    device_command(state, Source::Keyboard, 0x00)?;
    if read_response(state, Source::Keyboard)? != 2 {
        return Err(Ps2Error::WrongScanSet);
    }
    Ok(())
}

fn prepare_mouse(state: &State, quiet: u8, keyboard: bool) -> Result<(), Ps2Error> {
    command(0xa9)?;
    if read_response(state, Source::Keyboard)? != 0 {
        return Err(Ps2Error::MousePortTest);
    }
    command(0xa8)?;
    let config = configuration(quiet, keyboard, true, false);
    write_config(config)?;
    verify_config(state, config)?;
    device_command(state, Source::Mouse, 0xf5)?;
    // Defaults alone do not undo IntelliMouse mode. Reset establishes ID 0 and
    // disables reporting; then verify both BAT and the standard three-byte ID.
    device_command(state, Source::Mouse, 0xff)?;
    if read_response(state, Source::Mouse)? != 0xaa {
        return Err(Ps2Error::MouseSelfTest);
    }
    if read_response(state, Source::Mouse)? != 0 {
        return Err(Ps2Error::UnsupportedMouse);
    }
    device_command(state, Source::Mouse, 0xf6)?;
    device_command(state, Source::Mouse, 0xf2)?;
    if read_response(state, Source::Mouse)? != 0 {
        return Err(Ps2Error::UnsupportedMouse);
    }
    Ok(())
}

fn status() -> u8 {
    // SAFETY: This single-BSP module alone owns the i8042 ports, with IF clear.
    unsafe { arch::in8(STATUS_COMMAND) }
}

fn data() -> u8 {
    // SAFETY: The caller observed OBF and this module exclusively owns port 0x60.
    unsafe { arch::in8(DATA) }
}

fn wait_input_empty() -> Result<(), Ps2Error> {
    for _ in 0..POLL_LIMIT {
        let current = status();
        if current == 0xff {
            return Err(Ps2Error::ControllerMissing);
        }
        if current & INPUT_FULL == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(Ps2Error::InputTimeout)
}

fn command(value: u8) -> Result<(), Ps2Error> {
    wait_input_empty()?;
    // SAFETY: Exclusively owned command port and an empty input buffer.
    unsafe { arch::out8(STATUS_COMMAND, value) };
    Ok(())
}

fn write_data(value: u8) -> Result<(), Ps2Error> {
    wait_input_empty()?;
    // SAFETY: Exclusively owned data port and an empty input buffer.
    unsafe { arch::out8(DATA, value) };
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Response {
    Waiting,
    OtherSource,
    Byte(u8),
}

fn classify_response(expected: Source, status: u8, byte: u8) -> Result<Response, Ps2Error> {
    if status == 0xff {
        return Err(Ps2Error::ControllerMissing);
    }
    if status & OUTPUT_FULL == 0 {
        return Ok(Response::Waiting);
    }
    if Source::from_status(status) != expected {
        return Ok(Response::OtherSource);
    }
    if status & RECEIVE_ERROR != 0 {
        return Err(Ps2Error::ControllerData);
    }
    Ok(Response::Byte(byte))
}

fn read_response(state: &State, expected: Source) -> Result<u8, Ps2Error> {
    for _ in 0..POLL_LIMIT {
        let current = status();
        // Do not touch the data port for an absent controller or empty buffer.
        let byte = if current != 0xff && current & OUTPUT_FULL != 0 {
            data()
        } else {
            0
        };
        match classify_response(expected, current, byte)? {
            Response::Byte(byte) => return Ok(byte),
            Response::OtherSource => state.route(current, byte),
            Response::Waiting => core::hint::spin_loop(),
        }
    }
    Err(Ps2Error::OutputTimeout)
}

fn drain_initial_output() -> Result<(), Ps2Error> {
    for _ in 0..INIT_DRAIN_LIMIT {
        let current = status();
        if current == 0xff {
            return Err(Ps2Error::ControllerMissing);
        }
        if current & OUTPUT_FULL == 0 {
            return Ok(());
        }
        let _ = data();
    }
    if status() & OUTPUT_FULL == 0 {
        Ok(())
    } else {
        Err(Ps2Error::OutputStuck)
    }
}

fn read_config(state: &State) -> Result<u8, Ps2Error> {
    command(0x20)?;
    read_response(state, Source::Keyboard)
}

fn write_config(config: u8) -> Result<(), Ps2Error> {
    command(0x60)?;
    write_data(config)
}

fn verify_config(state: &State, expected: u8) -> Result<(), Ps2Error> {
    if read_config(state)? & 0x73 != expected & 0x73 {
        return Err(Ps2Error::Configuration);
    }
    Ok(())
}

fn device_command(state: &State, target: Source, value: u8) -> Result<(), Ps2Error> {
    for _ in 0..=RESEND_LIMIT {
        if target == Source::Mouse {
            command(0xd4)?;
        }
        write_data(value)?;
        // A final packet/scan byte may race disable. Match ACK only from the
        // addressed device, discarding at most 32 stale bytes from that device.
        let mut resend = false;
        for _ in 0..DRAIN_LIMIT {
            match read_response(state, target)? {
                0xfa => return Ok(()),
                0xfe => {
                    resend = true;
                    break;
                }
                0xfc | 0xfd | 0xff => return Err(Ps2Error::DeviceRejected),
                _ => {}
            }
        }
        if !resend {
            return Err(Ps2Error::DeviceRejected);
        }
    }
    Err(Ps2Error::ResendLimit)
}

fn drain_controller(state: &mut State) {
    for _ in 0..DRAIN_LIMIT {
        let current = status();
        if current == 0xff || current & OUTPUT_FULL == 0 {
            state.pending_drain = false;
            return;
        }
        state.route(current, data());
    }
    let current = status();
    state.pending_drain = current != 0xff && current & OUTPUT_FULL != 0;
}

/// IRQ1/vector 33 and IRQ12/vector 44 both drain the shared output port. The
/// dispatcher acknowledges the proper PIC(s) after this bounded handler returns.
pub(crate) fn handle_irq(vector: u8) {
    debug_assert!(!arch::interrupts_enabled());
    match vector {
        33 => &KEYBOARD_IRQS,
        44 => &MOUSE_IRQS,
        _ => panic!("invalid PS/2 IRQ vector"),
    }
    .fetch_add(1, Ordering::Relaxed);
    with_state(drain_controller);
}

/// Continue at most 32 deferred bytes, preserving IF. Call before borrowing any
/// keyboard/mouse state: the drain may call both device decoders synchronously.
/// The periodic PIT must keep waking main even when no input event is queued.
pub fn poll_pending() {
    with_state(|state| {
        if state.pending_drain {
            drain_controller(state);
        }
    });
}

/// Delivered handlers, possibly empty or raised by an initialization ACK.
pub fn keyboard_irq_count() -> u64 {
    KEYBOARD_IRQS.load(Ordering::Relaxed)
}

/// Delivered handlers, possibly empty or raised by an initialization ACK.
pub fn mouse_irq_count() -> u64 {
    MOUSE_IRQS.load(Ordering::Relaxed)
}

#[cfg(feature = "boot-smoke")]
fn dispatch_self_test() {
    fn feed(router: &State, source: Source, bytes: &[u8]) {
        let status = OUTPUT_FULL
            | if source == Source::Mouse {
                AUXILIARY
            } else {
                0
            };
        for &byte in bytes {
            router.route(status, byte);
        }
    }

    fn expect_key(ascii: u8) {
        let Some(InputEvent::Key(event)) = next_event() else {
            panic!("expected merged keyboard input");
        };
        assert_eq!(event.ascii(), Some(ascii));
        assert_eq!(event.modifiers, keyboard::Modifiers::default());
        assert!(!arch::interrupts_enabled());
    }

    fn expect_mouse(dx: i16) {
        assert_eq!(
            next_event(),
            Some(InputEvent::Mouse(mouse::MouseEvent {
                dx,
                dy: 0,
                buttons: 0
            }))
        );
        assert!(!arch::interrupts_enabled());
    }

    assert!(!arch::interrupts_enabled());
    with_state(|state| {
        assert!(!state.attempted && !state.pending_drain);
        assert!(!state.keyboard_ready && !state.mouse_ready);
    });
    let original_sequence = NEXT_SEQUENCE.swap(u64::MAX - 1, Ordering::Relaxed);
    keyboard::with_test_input(|| {
        mouse::with_test_input(|| {
            // The real route/decoders/queues/merge run without touching ports:
            // global controller initialization and pending-drain remain false.
            let mut router = State {
                attempted: false,
                keyboard_ready: true,
                mouse_ready: true,
                pending_drain: false,
            };
            // A key between mouse packet bytes must be delivered first. Actual
            // publications span MAX-1, MAX, 0, 1, exercising both queue heads.
            feed(&router, Source::Mouse, &[0x08, 3]);
            feed(&router, Source::Keyboard, &[0x1c]);
            feed(&router, Source::Mouse, &[0]);
            feed(&router, Source::Keyboard, &[0x32]);
            feed(&router, Source::Mouse, &[0x08, 7, 0]);
            expect_key(b'a');
            expect_mouse(3);
            expect_key(b'b');
            expect_mouse(7);
            assert_eq!(next_event(), None);

            // Loss from either or both sources invalidates even the other
            // source's already-queued events. Acknowledge exactly one barrier,
            // then require fresh keyboard recovery and fresh mouse input.
            for (key_loss, mouse_loss) in [(true, false), (false, true), (true, true)] {
                feed(&router, Source::Keyboard, &[0x12, 0x21]);
                feed(&router, Source::Mouse, &[0x09, 4, 0]);
                if key_loss {
                    router.route(OUTPUT_FULL | RECEIVE_ERROR, 0x00);
                    // Recovery can complete before the loss is consumed. This
                    // newly queued text must still be discarded by the barrier.
                    feed(&router, Source::Keyboard, &[0xf0, 0x1c, 0x1c]);
                }
                if mouse_loss {
                    router.route(OUTPUT_FULL | AUXILIARY | RECEIVE_ERROR, 0x08);
                    // Consume the corrupt packet's tail, then queue another
                    // valid packet behind the pending loss notification.
                    feed(&router, Source::Mouse, &[0x09, 0x09, 0x08, 5, 0]);
                }
                assert_eq!(next_event(), Some(InputEvent::Lost));
                assert_eq!(next_event(), None);
                assert!(!keyboard::take_overflow() && !mouse::take_overflow());
                assert!(!arch::interrupts_enabled());
                feed(&router, Source::Keyboard, &[0x1c]);
                assert_eq!(next_event(), None); // Makes alone cannot recover.
                feed(&router, Source::Keyboard, &[0xf0, 0x1c, 0x1c]);
                feed(&router, Source::Mouse, &[0x08, 2, 0]);
                expect_key(b'a'); // Stale Shift must not survive global loss.
                expect_mouse(2);
                assert_eq!(next_event(), None);
            }

            // A keyboard fault can cut a mouse packet after its header. The
            // global barrier must discard its remaining two header-looking
            // payload bytes instead of manufacturing a click from stale data.
            feed(&router, Source::Mouse, &[0x09]);
            router.route(OUTPUT_FULL | RECEIVE_ERROR, 0x00);
            assert_eq!(next_event(), Some(InputEvent::Lost));
            feed(&router, Source::Mouse, &[0x09, 0x09]);
            assert_eq!(next_event(), None);
            feed(&router, Source::Mouse, &[0x08, 1, 0]);
            expect_mouse(1);
            assert_eq!(next_event(), None);

            // Fatal controller handoff revokes even a previously activated
            // keyboard, discards both queues, and cancels deferred work. Later
            // bytes cannot become input through either revoked device route.
            feed(&router, Source::Keyboard, &[0xf0, 0x1c, 0x1c]);
            feed(&router, Source::Mouse, &[0x08, 1, 0]);
            assert!(keyboard::peek_sequence().is_some());
            assert!(mouse::peek_sequence().is_some());
            router.pending_drain = true;
            router.stop_routing();
            assert!(!router.keyboard_ready && !router.mouse_ready && !router.pending_drain);
            feed(&router, Source::Keyboard, &[0xf0, 0x1c, 0x1c]);
            feed(&router, Source::Mouse, &[0x08, 1, 0]);
            assert_eq!(next_event(), None);
        });
    });
    // Both temporary device states ended before returning the sequence counter
    // to its pre-test value, so no duplicate publication can remain queued.
    NEXT_SEQUENCE.store(original_sequence, Ordering::Relaxed);
    assert_eq!(next_event(), None);
    assert!(!arch::interrupts_enabled());
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    // AUX, not which IRQ happened to run, determines who owns every byte.
    for target in [Source::Keyboard, Source::Mouse] {
        for source in [Source::Keyboard, Source::Mouse] {
            let status = OUTPUT_FULL
                | if source == Source::Mouse {
                    AUXILIARY
                } else {
                    0
                };
            for byte in [0x00, 0x1c, 0xaa, 0xfa, 0xfe] {
                assert_eq!(
                    classify_response(target, status, byte),
                    Ok(if source == target {
                        Response::Byte(byte)
                    } else {
                        Response::OtherSource
                    })
                );
                assert_eq!(
                    classify_response(target, status | RECEIVE_ERROR, byte),
                    if source == target {
                        Err(Ps2Error::ControllerData)
                    } else {
                        Ok(Response::OtherSource)
                    }
                );
            }
        }
        assert_eq!(classify_response(target, 0, 0xfa), Ok(Response::Waiting));
        assert_eq!(
            classify_response(target, 0xff, 0xfa),
            Err(Ps2Error::ControllerMissing)
        );
    }
    // All combinations preserve system/keylock, forbid translation/reserved
    // bits, and never enable an IRQ for a disabled or failed device.
    for original in 0..=u8::MAX {
        for keyboard in [false, true] {
            for mouse in [false, true] {
                for irqs in [false, true] {
                    let config = configuration(original, keyboard, mouse, irqs);
                    assert_eq!(config & 0xcc, original & 0x0c);
                    assert_eq!(config & 0x10 == 0, keyboard);
                    assert_eq!(config & 0x20 == 0, mouse);
                    assert_eq!(config & 1 != 0, keyboard && irqs);
                    assert_eq!(config & 2 != 0, mouse && irqs);
                }
            }
        }
    }
    let original_if = arch::interrupts_enabled();
    poll_pending();
    assert_eq!(arch::interrupts_enabled(), original_if);
    assert_eq!(next_event(), None);
    assert_eq!(arch::interrupts_enabled(), original_if);
    dispatch_self_test();
    crate::serial::println!("MIRROR_PS2_TESTS_OK");
}
