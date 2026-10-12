//! Sole i8042 controller owner for the single-BSP, explicitly selected q35 PC.
//!
//! Every status/data pair is routed by AUX, never by the IRQ that caused the
//! drain. Device decoders own separate state and never call this module while
//! borrowed. Initialization runs with both PIC input IRQs masked and IF clear;
//! runtime commands, hotplug, ACPI discovery and output-port/A20 writes are
//! absent, except the one reset-line pulse that the restart path sends through
//! `reset_prepare` (spec 0058a, D4). Every wait is bounded by elapsed time
//! (spec 0017b-D5).
//!
//! Protocol references (no implementation copied):
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/input/pckbd.c
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/input/ps2.c
//! https://intel.github.io/ecfw-zephyr/reference/kbchost/index.html

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::timer::{RESET_POLL_LIMIT, Stopwatch};
use crate::{arch, keyboard, mouse, serial, vmmouse};

const DATA: u16 = 0x60;
const STATUS_COMMAND: u16 = 0x64;
const OUTPUT_FULL: u8 = 1;
const INPUT_FULL: u8 = 2;
const AUXILIARY: u8 = 0x20;
const RECEIVE_ERROR: u8 = 0xc0;
/// A backstop only: elapsed-time deadlines end every init wait. At 50 ns per
/// status read it cannot fire before the longest budget, the 4 s BAT wait.
const POLL_LIMIT: usize = 80_000_000;
const _: () = assert!(POLL_LIMIT as u64 * 50 >= Wait::Bat.budget_ms() * NANOS_PER_MS);
const NANOS_PER_MS: u64 = 1_000_000;
const INIT_DRAIN_LIMIT: usize = 256;
const DRAIN_LIMIT: usize = 32;
const RESEND_LIMIT: usize = 3;
/// The most items next_event can return without a new byte: each device's
/// full ring plus its one pending loss marker, which events queued after that
/// device's loss follow (spec 0017a-D3). Handoff drains use it as their bound.
pub(crate) const MAX_BACKLOG: usize = keyboard::QUEUE_CAPACITY + 1 + mouse::QUEUE_CAPACITY + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ps2Error {
    InterruptsEnabled,
    AlreadyInitialized,
    ControllerMissing,
    InputTimeout,
    OutputTimeout,
    OutputStuck,
    ControllerData,
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

/// One kind of bounded init wait, with its Linux budget (spec 0017b-D5): the
/// i8042 controller's IBF/OBF timeout, and libps2's ACK, reset ACK, response
/// and reset BAT waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Controller,
    Ack,
    ResetAck,
    Response,
    Bat,
}

impl Wait {
    const fn budget_ms(self) -> u64 {
        match self {
            Self::Controller => 500,
            Self::Ack => 200,
            Self::ResetAck => 1000,
            Self::Response => 500,
            Self::Bat => 4000,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Controller => "controller",
            Self::Ack | Self::ResetAck => "ack",
            Self::Response => "response",
            Self::Bat => "bat",
        }
    }
}

/// The start of one wait on the init clock. Each poll samples the clock before
/// its status read, so the read that follows an expired sample still accepts
/// a reply that arrived in time.
#[derive(Clone, Copy)]
struct Deadline {
    wait: Wait,
    start_ns: u64,
}

impl Deadline {
    fn start(clock: &mut Stopwatch, wait: Wait) -> Self {
        Self {
            wait,
            start_ns: clock.elapsed_ns(),
        }
    }

    fn waited_ns(self, clock: &mut Stopwatch) -> u64 {
        clock.elapsed_ns().saturating_sub(self.start_ns)
    }

    fn expired(self, clock: &mut Stopwatch) -> bool {
        self.waited_ns(clock) >= self.wait.budget_ms() * NANOS_PER_MS
    }

    /// Report this wait's own duration before the device's SKIPPED line. It is
    /// a content-free device marker, so every build flavor prints it.
    fn timed_out(self, clock: &mut Stopwatch, error: Ps2Error) -> Ps2Error {
        serial::println!(
            "MIRROR_PS2_TIMEOUT wait={} elapsed_ms={} clock={}",
            self.wait.name(),
            self.waited_ns(clock) / NANOS_PER_MS,
            clock.name()
        );
        error
    }
}

/// Smoke fault Ps2StaleScancode (spec 0017b): armed once 0xAE enables the
/// keyboard port, so the next keyboard-source read returns a buffered make
/// code before any real controller data.
#[cfg(feature = "boot-smoke")]
static STALE_SCANCODE: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "boot-smoke")]
const STALE_MAKE: u8 = 0x1c;

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
static CONSUMER_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Exclusive ownership of completed input delivery on the BSP. Interrupts may
/// still append device events, but only this non-cloneable token can consume
/// them. Borrow it into a userspace input session or the kernel fallback, never
/// both. The raw-pointer marker also forbids moving/sharing it across threads.
pub struct Consumer {
    _bsp_only: PhantomData<*mut ()>,
}

impl Consumer {
    pub fn claim() -> Option<Self> {
        CONSUMER_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Self {
            _bsp_only: PhantomData,
        })
    }

    /// Preserve entry IF; no controller or decoder borrow escapes this call.
    pub fn next_event(&mut self) -> Option<InputEvent> {
        next_event()
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        CONSUMER_CLAIMED.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key(keyboard::KeyEvent),
    Mouse(mouse::MouseEvent),
    /// A vmmouse absolute position in device units (spec 0017-D14).
    Position(mouse::PositionEvent),
    /// One device's loss marker (spec 0017a-D3). next_event sets exactly one
    /// flag; only a session's initial record names both devices.
    Lost {
        keyboard: bool,
        pointer: bool,
    },
}

/// A device stamps only a completed, queued or merged event, or its first
/// unacknowledged loss. This atomic has no controller borrow, so it is safe
/// from either decoder callback under IF exclusion.
pub(crate) fn next_sequence() -> u64 {
    NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn keyboard_first(keyboard: u64, mouse: u64) -> bool {
    // Each device holds at most 64 events and one loss marker. A full mouse
    // ring can keep an old head while merged packets take new sequences, but
    // every allocation needs a received byte or a drained vmmouse packet, so
    // live sequence numbers stay far closer than half the u64 range,
    // including across the natural wrap.
    (keyboard.wrapping_sub(mouse) as i64) < 0
}

/// Drain once, then consume the oldest keyboard/mouse item under one IF guard.
/// Each device reports a pending loss marker ahead of its queue head, so both
/// heads, markers included, merge by sequence. A loss never discards the other
/// device's queued events (spec 0017a-D3). Device functions perform no I/O.
fn next_event() -> Option<InputEvent> {
    let enabled = arch::save_and_disable_interrupts();
    // poll_pending ends its controller borrow before any device queue is read.
    poll_pending();
    let keyboard_next = match (keyboard::peek_sequence(), mouse::peek_sequence()) {
        (Some(key), Some(mouse)) => Some(keyboard_first(key, mouse)),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    };
    let result = match keyboard_next {
        Some(true) if keyboard::take_lost() => Some(InputEvent::Lost {
            keyboard: true,
            pointer: false,
        }),
        Some(true) => keyboard::pop_event().map(InputEvent::Key),
        Some(false) if mouse::take_lost() => Some(InputEvent::Lost {
            keyboard: false,
            pointer: true,
        }),
        Some(false) => mouse::pop_event().map(|event| match event {
            mouse::Event::Relative(motion) => InputEvent::Mouse(motion),
            mouse::Event::Absolute(position) => InputEvent::Position(position),
        }),
        None => None,
    };
    // SAFETY: Controller and both device borrows ended; initialized handlers
    // see consistent queues if the caller previously enabled interrupts.
    unsafe { arch::restore_interrupts(enabled) };
    // The IRQ side only records a vmmouse runtime fallback; print it once here
    // (spec 0017-D16).
    vmmouse::report_fallback();
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
        let mut clock = Stopwatch::start();
        let mut quiet_config = None;
        let result = match initialize(state, &mut clock, &mut quiet_config) {
            Ok(result) => result,
            Err(error) => {
                // A final mouse-enable failure can reveal a broken controller
                // after keyboard activation. Revoke both routes/queues before
                // best-effort quieting; neither PIC input IRQ may be unmasked.
                state.stop_routing();
                let _ = command(&mut clock, 0xad);
                let _ = command(&mut clock, 0xa7);
                if let Some(quiet) = quiet_config {
                    let _ = write_config(&mut clock, quiet);
                }
                InitResult::failed(error)
            }
        };
        // A content-free lifecycle marker in every build flavor (spec 0017b).
        // Init no longer runs the controller self-test (0017b-D8).
        serial::println!(
            "MIRROR_PS2_INIT elapsed_ms={} clock={} selftest=0",
            clock.elapsed_ns() / NANOS_PER_MS,
            clock.name()
        );
        result
    })
}

fn configuration(original: u8, keyboard: bool, mouse: bool, irqs: bool) -> u8 {
    // Preserve only system/keylock policy. Clear translation/reserved bits;
    // each failed interface stays disabled with its interrupt bit clear.
    (original & 0x0c)
        | if keyboard { u8::from(irqs) } else { 0x10 }
        | if mouse { u8::from(irqs) << 1 } else { 0x20 }
}

fn initialize(
    state: &mut State,
    clock: &mut Stopwatch,
    quiet_config: &mut Option<u8>,
) -> Result<InitResult, Ps2Error> {
    command(clock, 0xad)?;
    command(clock, 0xa7)?;
    drain_initial_output()?;
    let quiet = configuration(read_config(state, clock)?, false, false, false);
    *quiet_config = Some(quiet);
    write_config(clock, quiet)?;
    // No controller self-test (0xaa, spec 0017b-D8): it can only abort init or
    // reset the configuration. The port tests below still run.

    let mut keyboard = prepare_keyboard(state, clock, quiet);
    if keyboard.is_err() {
        command(clock, 0xad)?;
        write_config(clock, quiet)?;
    }
    let mut mouse = prepare_mouse(state, clock, quiet, keyboard.is_ok());
    if mouse.is_err() {
        command(clock, 0xa7)?;
    }
    let mut active = configuration(quiet, keyboard.is_ok(), mouse.is_ok(), true);
    write_config(clock, active)?;
    verify_config(state, clock, active)?;

    // No config reads after keyboard scanning begins: controller replies have
    // the same source tag as scan bytes. Mouse commands can still distinguish
    // and deliver every intervening keyboard byte to its now-active decoder.
    if keyboard.is_ok() {
        keyboard = device_command(state, clock, Source::Keyboard, 0xf4);
        if keyboard.is_ok() {
            keyboard::activate();
            state.keyboard_ready = true;
        } else {
            command(clock, 0xad)?;
            active = configuration(quiet, false, mouse.is_ok(), true);
            write_config(clock, active)?;
            verify_config(state, clock, active)?;
        }
    }
    if mouse.is_ok() {
        mouse = device_command(state, clock, Source::Mouse, 0xf4);
        if mouse.is_ok() {
            mouse::activate();
            state.mouse_ready = true;
        } else {
            // Preserve the working keyboard even when the mouse does not ACK.
            // If quieting fails, revoke the whole controller: live AUX data
            // with IRQ12 masked could occupy OBF and prevent future IRQ1 input.
            command(clock, 0xa7)?;
            write_config(
                clock,
                configuration(active, state.keyboard_ready, false, true),
            )?;
        }
    }
    // Drain even if an enable ACK consumed the IRQ's edge. Future IRQs and PIT
    // wakeups continue bounded work until the shared output buffer is empty.
    state.pending_drain = state.keyboard_ready || state.mouse_ready;
    Ok(InitResult { keyboard, mouse })
}

fn prepare_keyboard(state: &State, clock: &mut Stopwatch, quiet: u8) -> Result<(), Ps2Error> {
    command(clock, 0xab)?;
    if read_reply(state, clock, Source::Keyboard, Wait::Controller)? != 0 {
        return Err(Ps2Error::KeyboardPortTest);
    }
    command(clock, 0xae)?;
    #[cfg(feature = "boot-smoke")]
    if matches!(
        crate::boot::smoke_mode(),
        crate::boot::SmokeMode::Ps2StaleScancode
    ) {
        STALE_SCANCODE.store(true, Ordering::Relaxed);
    }
    let config = configuration(quiet, true, false, false);
    write_config(clock, config)?;
    // Stop scanning before the config readback (spec 0017b). A keyboard that
    // firmware left scanning can buffer a scancode at boot, and the readback
    // would take it as the config byte: replies share the keyboard source tag.
    // F5's ACK wait discards up to 32 such bytes first.
    device_command(state, clock, Source::Keyboard, 0xf5)?;
    verify_config(state, clock, config)?;
    device_command(state, clock, Source::Keyboard, 0xf0)?;
    device_command(state, clock, Source::Keyboard, 0x02)?;
    device_command(state, clock, Source::Keyboard, 0xf0)?;
    device_command(state, clock, Source::Keyboard, 0x00)?;
    // Set 2 reads back as 0x02, or as 0x41, its translated form, which VMware
    // returns even with translation off while it sends raw Set 2 scancodes
    // (0017b-D9, amended 2026-10-11). Any other reply is a wrong set.
    if !matches!(
        read_reply(state, clock, Source::Keyboard, Wait::Response)?,
        0x02 | 0x41
    ) {
        return Err(Ps2Error::WrongScanSet);
    }
    Ok(())
}

fn prepare_mouse(
    state: &State,
    clock: &mut Stopwatch,
    quiet: u8,
    keyboard: bool,
) -> Result<(), Ps2Error> {
    command(clock, 0xa9)?;
    if read_reply(state, clock, Source::Keyboard, Wait::Controller)? != 0 {
        return Err(Ps2Error::MousePortTest);
    }
    command(clock, 0xa8)?;
    let config = configuration(quiet, keyboard, true, false);
    write_config(clock, config)?;
    verify_config(state, clock, config)?;
    device_command(state, clock, Source::Mouse, 0xf5)?;
    // Defaults alone do not undo IntelliMouse mode. Reset establishes ID 0 and
    // disables reporting; then verify both BAT and the standard three-byte ID.
    send_device(state, clock, Source::Mouse, 0xff, Wait::ResetAck)?;
    if read_bat(state, clock)? != 0xaa {
        return Err(Ps2Error::MouseSelfTest);
    }
    if read_reply(state, clock, Source::Mouse, Wait::Response)? != 0 {
        return Err(Ps2Error::UnsupportedMouse);
    }
    device_command(state, clock, Source::Mouse, 0xf6)?;
    device_command(state, clock, Source::Mouse, 0xf2)?;
    if read_reply(state, clock, Source::Mouse, Wait::Response)? != 0 {
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

fn wait_input_empty(clock: &mut Stopwatch) -> Result<(), Ps2Error> {
    let deadline = Deadline::start(clock, Wait::Controller);
    for _ in 0..POLL_LIMIT {
        let expired = deadline.expired(clock);
        let current = status();
        if current == 0xff {
            return Err(Ps2Error::ControllerMissing);
        }
        if current & INPUT_FULL == 0 {
            return Ok(());
        }
        if expired {
            break;
        }
        core::hint::spin_loop();
    }
    Err(deadline.timed_out(clock, Ps2Error::InputTimeout))
}

fn command(clock: &mut Stopwatch, value: u8) -> Result<(), Ps2Error> {
    wait_input_empty(clock)?;
    // SAFETY: Exclusively owned command port and an empty input buffer.
    unsafe { arch::out8(STATUS_COMMAND, value) };
    Ok(())
}

fn write_data(clock: &mut Stopwatch, value: u8) -> Result<(), Ps2Error> {
    wait_input_empty(clock)?;
    // SAFETY: Exclusively owned data port and an empty input buffer.
    unsafe { arch::out8(DATA, value) };
    Ok(())
}

/// The 8042 command that pulses output-port bit 0, the CPU reset line.
const PULSE_RESET_LINE: u8 = 0xfe;
const _: () = assert!(
    PULSE_RESET_LINE == mirror_acpi::KBC_RESET_PULSE
        && STATUS_COMMAND == mirror_acpi::KBC_COMMAND_PORT
);

/// Why the restart path skips the 8042 pulse (spec 0058a, D4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResetSkip {
    /// The status port reads 0xFF: no controller decodes it (`-NoKeyboard`).
    Absent,
    /// The input buffer stayed full for the whole controller budget.
    Busy,
}

/// Proof that the controller's input buffer was empty, which the one reset
/// pulse needs. Dropping it writes nothing.
pub(crate) struct ResetPulse {
    _empty: (),
}

impl ResetPulse {
    /// Write the pulse command once. If the machine survives, the restart
    /// path's own timed wait follows.
    pub(crate) fn pulse(self) {
        // SAFETY: This module owns port 0x64. `reset_prepare` observed an empty
        // input buffer on the BSP with IF clear, and the restart path that
        // holds this token never returns to input handling.
        unsafe { arch::out8(STATUS_COMMAND, PULSE_RESET_LINE) };
    }
}

/// D4's choice as a pure function of one status read and the time waited so
/// far: `None` keeps waiting. An absent controller and an empty buffer are
/// decided by the read itself, so the read after an expired sample still
/// accepts a buffer that emptied in time.
pub(crate) fn reset_choice(status: u8, waited_ms: u64) -> Option<Result<(), ResetSkip>> {
    if status == 0xff {
        Some(Err(ResetSkip::Absent))
    } else if status & INPUT_FULL == 0 {
        Some(Ok(()))
    } else if waited_ms >= Wait::Controller.budget_ms() {
        Some(Err(ResetSkip::Busy))
    } else {
        None
    }
}

/// Wait up to the controller budget (500 ms, 0017b-D5) for the input buffer
/// to empty, then hand out the single pulse. Prints nothing: the restart path
/// reports the outcome. The shared poll limit ends a wait whose stopwatch
/// stalls, as `busy`. Call only from the restart path, on the BSP, IF clear.
pub(crate) fn reset_prepare(stopwatch: &mut Stopwatch) -> Result<ResetPulse, ResetSkip> {
    assert!(!arch::interrupts_enabled(), "reset pulse requires IF clear");
    let start_ns = stopwatch.elapsed_ns();
    for _ in 0..RESET_POLL_LIMIT {
        let waited_ms = stopwatch.elapsed_ns().saturating_sub(start_ns) / NANOS_PER_MS;
        match reset_choice(status(), waited_ms) {
            Some(Ok(())) => return Ok(ResetPulse { _empty: () }),
            Some(Err(skip)) => return Err(skip),
            None => core::hint::spin_loop(),
        }
    }
    Err(ResetSkip::Busy)
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

/// Wait for one byte from `expected` until `deadline`, which several reads
/// can share. Bytes from the other source go to its decoder, if active.
fn read_response(
    state: &State,
    clock: &mut Stopwatch,
    expected: Source,
    deadline: Deadline,
) -> Result<u8, Ps2Error> {
    #[cfg(feature = "boot-smoke")]
    if expected == Source::Keyboard && STALE_SCANCODE.swap(false, Ordering::Relaxed) {
        serial::println!("MIRROR_PS2_INJECTED stale=1");
        return Ok(STALE_MAKE);
    }
    for _ in 0..POLL_LIMIT {
        let expired = deadline.expired(clock);
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
        if expired {
            break;
        }
    }
    Err(deadline.timed_out(clock, Ps2Error::OutputTimeout))
}

/// One reply with its own deadline.
fn read_reply(
    state: &State,
    clock: &mut Stopwatch,
    expected: Source,
    wait: Wait,
) -> Result<u8, Ps2Error> {
    let deadline = Deadline::start(clock, wait);
    read_response(state, clock, expected, deadline)
}

/// The mouse's reset result. Smoke fault Ps2BatTimeout (spec 0017b) reads and
/// drops every mouse byte during this wait only, so it ends at its deadline.
fn read_bat(state: &State, clock: &mut Stopwatch) -> Result<u8, Ps2Error> {
    let deadline = Deadline::start(clock, Wait::Bat);
    #[cfg(feature = "boot-smoke")]
    if matches!(
        crate::boot::smoke_mode(),
        crate::boot::SmokeMode::Ps2BatTimeout
    ) {
        // Every dropped byte shares the one BAT deadline; the explicit check
        // also ends a byte stream that never lets a single read time out.
        for _ in 0..POLL_LIMIT {
            if deadline.expired(clock) {
                break;
            }
            read_response(state, clock, Source::Mouse, deadline)?;
        }
        return Err(deadline.timed_out(clock, Ps2Error::OutputTimeout));
    }
    read_response(state, clock, Source::Mouse, deadline)
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

fn read_config(state: &State, clock: &mut Stopwatch) -> Result<u8, Ps2Error> {
    command(clock, 0x20)?;
    read_reply(state, clock, Source::Keyboard, Wait::Controller)
}

fn write_config(clock: &mut Stopwatch, config: u8) -> Result<(), Ps2Error> {
    command(clock, 0x60)?;
    write_data(clock, config)
}

fn verify_config(state: &State, clock: &mut Stopwatch, expected: u8) -> Result<(), Ps2Error> {
    if read_config(state, clock)? & 0x73 != expected & 0x73 {
        return Err(Ps2Error::Configuration);
    }
    Ok(())
}

fn device_command(
    state: &State,
    clock: &mut Stopwatch,
    target: Source,
    value: u8,
) -> Result<(), Ps2Error> {
    send_device(state, clock, target, value, Wait::Ack)
}

fn send_device(
    state: &State,
    clock: &mut Stopwatch,
    target: Source,
    value: u8,
    ack: Wait,
) -> Result<(), Ps2Error> {
    for _ in 0..=RESEND_LIMIT {
        if target == Source::Mouse {
            command(clock, 0xd4)?;
        }
        write_data(clock, value)?;
        // A final packet/scan byte may race disable. Match ACK only from the
        // addressed device, discarding at most 32 stale bytes from that device.
        // Each attempt has one ACK deadline, which those discards share.
        let deadline = Deadline::start(clock, ack);
        let mut resend = false;
        for _ in 0..DRAIN_LIMIT {
            match read_response(state, clock, target, deadline)? {
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
/// A drain that stamped a record, a vmmouse drain's included, flags input
/// arrival for the scheduler (spec 0030a, 0030-D2).
pub(crate) fn handle_irq(vector: u8) {
    debug_assert!(!arch::interrupts_enabled());
    match vector {
        33 => &KEYBOARD_IRQS,
        44 => &MOUSE_IRQS,
        _ => panic!("invalid PS/2 IRQ vector"),
    }
    .fetch_add(1, Ordering::Relaxed);
    let before = NEXT_SEQUENCE.load(Ordering::Relaxed);
    with_state(drain_controller);
    if NEXT_SEQUENCE.load(Ordering::Relaxed) != before {
        crate::input::note_arrival();
    }
}

/// Whether a bounded drain left work for the next consumer poll: controller
/// bytes past the IRQ's limit, or vmmouse packets past its 64-packet bound.
pub(crate) fn drain_pending() -> bool {
    with_state(|state| state.pending_drain) || vmmouse::pending()
}

/// Continue at most 32 deferred bytes, preserving IF. Call before borrowing any
/// keyboard/mouse state: the drain may call both device decoders synchronously.
/// The periodic PIT must keep waking main even when no input event is queued.
/// A vmmouse drain that stopped at its 64-packet bound also continues here,
/// independently of the controller's pending drain (spec 0017-D13).
pub fn poll_pending() {
    with_state(|state| {
        if state.pending_drain {
            drain_controller(state);
        }
    });
    if vmmouse::pending() {
        mouse::drain_vmmouse();
    }
}

/// Delivered handlers, possibly empty or raised by an initialization ACK.
pub fn keyboard_irq_count() -> u64 {
    KEYBOARD_IRQS.load(Ordering::Relaxed)
}

/// Delivered handlers, possibly empty or raised by an initialization ACK.
#[cfg(feature = "test-trace")]
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

    fn expect_key_with(ascii: u8, modifiers: keyboard::Modifiers) -> keyboard::KeyEvent {
        let Some(InputEvent::Key(event)) = next_event() else {
            panic!("expected merged keyboard input");
        };
        assert_eq!(event.ascii(), Some(ascii));
        assert_eq!(event.modifiers, modifiers);
        assert!(!arch::interrupts_enabled());
        event
    }

    fn expect_key(ascii: u8) {
        let event = expect_key_with(ascii, keyboard::Modifiers::default());
        assert!(event.pressed && !event.repeat);
    }

    fn expect_release() {
        let Some(InputEvent::Key(event)) = next_event() else {
            panic!("expected merged keyboard release");
        };
        assert!(!event.pressed);
        assert!(!arch::interrupts_enabled());
    }

    fn expect_pointer(dx: i16, buttons: u8) {
        assert_eq!(
            next_event(),
            Some(InputEvent::Mouse(mouse::MouseEvent { dx, dy: 0, buttons }))
        );
        assert!(!arch::interrupts_enabled());
    }

    fn expect_mouse(dx: i16) {
        expect_pointer(dx, 0);
    }

    fn expect_lost(keyboard: bool) {
        assert_eq!(
            next_event(),
            Some(InputEvent::Lost {
                keyboard,
                pointer: !keyboard
            })
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

            // Mouse-only losses (spec 0017a-D3) never change keyboard state:
            // held Shift, Caps Lock and an already queued key all survive,
            // and the pointer marker follows that key by sequence.
            feed(
                &router,
                Source::Keyboard,
                &[0xf0, 0x1c, 0xf0, 0x32, 0x12, 0x58, 0xf0, 0x58],
            );
            for _ in 0..5 {
                assert!(matches!(next_event(), Some(InputEvent::Key(_))));
            }
            assert_eq!(next_event(), None);
            let held = keyboard::Modifiers {
                shift: true,
                caps_lock: true,
                ..keyboard::Modifiers::default()
            };
            for case in 0..3 {
                feed(&router, Source::Keyboard, &[0x1c]);
                match case {
                    // An AUX RECEIVE_ERROR byte in a packet's header position.
                    0 => router.route(OUTPUT_FULL | AUXILIARY | RECEIVE_ERROR, 0x08),
                    // A motion-overflow packet with the left button held.
                    1 => feed(&router, Source::Mouse, &[0x48 | mouse::LEFT, 0xff, 0]),
                    // A button change on a full ring.
                    _ => {
                        for _ in 0..mouse::QUEUE_CAPACITY {
                            feed(&router, Source::Mouse, &[0x08, 1, 0]);
                        }
                        feed(&router, Source::Mouse, &[0x08 | mouse::LEFT, 1, 0]);
                    }
                }
                let event = expect_key_with(b'a', held);
                assert!(event.pressed && !event.repeat);
                expect_lost(false);
                if case == 1 {
                    // The overflow packet's zero-motion button snapshot.
                    expect_pointer(0, mouse::LEFT);
                }
                assert_eq!(next_event(), None);
                if case == 0 {
                    // The corrupt header's two remaining positions are
                    // discarded as framing, not decoded as a click.
                    feed(&router, Source::Mouse, &[0x09, 0x09]);
                    assert_eq!(next_event(), None);
                }
                // The next make is delivered from its first byte, with no
                // keyboard recovery gate and both modifiers intact.
                feed(&router, Source::Keyboard, &[0x1c]);
                let event = expect_key_with(b'a', held);
                assert!(event.pressed && event.repeat);
                feed(&router, Source::Keyboard, &[0xf0, 0x1c]);
                expect_release();
                assert_eq!(next_event(), None);
                assert!(!keyboard::take_lost() && !mouse::take_lost());
            }

            // A keyboard fault between two mouse packets drops neither: its
            // marker sorts between them. Keyboard recovery still withholds
            // makes until a complete release, and Shift and Caps Lock reset.
            feed(&router, Source::Mouse, &[0x08, 3, 0]);
            router.route(OUTPUT_FULL | RECEIVE_ERROR, 0x00);
            feed(&router, Source::Mouse, &[0x08, 4, 0]);
            expect_mouse(3);
            expect_lost(true);
            expect_mouse(4);
            assert_eq!(next_event(), None);
            feed(&router, Source::Keyboard, &[0x1c]);
            assert_eq!(next_event(), None); // Makes alone cannot recover.
            feed(&router, Source::Keyboard, &[0xf0, 0x1c, 0x1c]);
            expect_key(b'a');
            assert_eq!(next_event(), None);

            // A keyboard fault can cut a mouse packet after its header. The
            // mouse decoder is untouched, so the packet completes and is
            // delivered after the keyboard marker.
            feed(&router, Source::Mouse, &[0x09]);
            router.route(OUTPUT_FULL | RECEIVE_ERROR, 0x00);
            feed(&router, Source::Mouse, &[5, 0]);
            expect_lost(true);
            expect_pointer(5, mouse::LEFT);
            assert_eq!(next_event(), None);

            // Both devices lost: two markers, in loss-sequence order. Input
            // that each device queues after its own loss, here including text
            // after keyboard recovery completes, follows its marker.
            for keyboard_loss_first in [true, false] {
                for keyboard_turn in [keyboard_loss_first, !keyboard_loss_first] {
                    if keyboard_turn {
                        router.route(OUTPUT_FULL | RECEIVE_ERROR, 0x00);
                    } else {
                        router.route(OUTPUT_FULL | AUXILIARY | RECEIVE_ERROR, 0x08);
                        feed(&router, Source::Mouse, &[0x09, 0x09]);
                    }
                }
                feed(&router, Source::Keyboard, &[0xf0, 0x1c, 0x1c]);
                feed(&router, Source::Mouse, &[0x08, 2, 0]);
                expect_lost(keyboard_loss_first);
                expect_lost(!keyboard_loss_first);
                expect_key(b'a');
                expect_mouse(2);
                assert_eq!(next_event(), None);
                assert!(!keyboard::take_lost() && !mouse::take_lost());
            }

            // A starved full mouse ring merges same-button motion into its
            // newest event, which takes the newest packet's sequence (spec
            // 0017a-D6): a key published between two merged packets is
            // delivered before the merged event, and no motion is lost.
            for _ in 0..mouse::QUEUE_CAPACITY {
                feed(&router, Source::Mouse, &[0x08, 1, 0]);
            }
            feed(&router, Source::Mouse, &[0x08, 2, 0]);
            feed(&router, Source::Keyboard, &[0xf0, 0x1c]);
            feed(&router, Source::Mouse, &[0x08, 3, 0]);
            for _ in 0..mouse::QUEUE_CAPACITY - 1 {
                expect_mouse(1);
            }
            expect_release();
            expect_mouse(6);
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
