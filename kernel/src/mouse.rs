//! Standard three-byte PS/2 mouse packets and a bounded event queue.
//!
//! The shared `ps2` controller owns all I/O, initialization, command replies,
//! and IRQ routing. It verifies device ID 0 before activating this decoder;
//! wheel/IntelliMouse packets are intentionally unsupported here.
//!
//! Protocol reference: QEMU's PS/2 implementation, especially
//! `ps2_mouse_send_packet`, `ps2_mouse_event`, and `ps2_write_mouse`:
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/input/ps2.c
//! No source code is copied. The header carries the ninth motion/sign bits;
//! PS/2 positive Y is upward, so decoded events invert Y for screen coordinates.

use core::cell::UnsafeCell;

use crate::{arch, ps2};

pub const LEFT: u8 = 1;
pub const RIGHT: u8 = 2;
pub const MIDDLE: u8 = 4;
const BUTTON_MASK: u8 = LEFT | RIGHT | MIDDLE;
const OUTPUT_FULL: u8 = 1;
const AUXILIARY: u8 = 0x20;
const RECEIVE_ERROR: u8 = 0xc0;
const HEADER_SYNC: u8 = 0x08;
const MOTION_OVERFLOW: u8 = 0xc0;
const QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub dx: i16,
    /// Positive values move downward on screen, opposite to raw PS/2 Y.
    pub dy: i16,
    /// Current button levels, not edges; LEFT | RIGHT | MIDDLE only.
    pub buttons: u8,
}

struct Packet {
    event: MouseEvent,
    overflow: bool,
}

#[derive(Clone, Copy)]
struct QueuedEvent {
    sequence: u64,
    event: MouseEvent,
}

struct Decoder {
    bytes: [u8; 3],
    length: u8,
    discard: u8,
}

impl Decoder {
    const fn new() -> Self {
        Self {
            bytes: [0; 3],
            length: 0,
            discard: 0,
        }
    }

    fn feed(&mut self, byte: u8) -> Option<Packet> {
        if self.discard != 0 {
            self.discard -= 1;
            return None;
        }
        if self.length == 0 && byte & HEADER_SYNC == 0 {
            return None;
        }
        self.bytes[usize::from(self.length)] = byte;
        self.length += 1;
        if self.length != 3 {
            return None;
        }
        self.length = 0;
        let header = self.bytes[0];
        let overflow = header & MOTION_OVERFLOW != 0;
        // Sign belongs to header bit 4/5, not to payload bit 7: this handles
        // the complete nine-bit range, including -256 and positive +255.
        let dx = i16::from(self.bytes[1]) - if header & 0x10 != 0 { 256 } else { 0 };
        let raw_dy = i16::from(self.bytes[2]) - if header & 0x20 != 0 { 256 } else { 0 };
        Some(Packet {
            event: MouseEvent {
                // Saturated/overflowed motion is not trustworthy. Preserve
                // button levels while rejecting both motion components.
                dx: if overflow { 0 } else { dx },
                dy: if overflow { 0 } else { -raw_dy },
                buttons: header & BUTTON_MASK,
            },
            overflow,
        })
    }

    fn corrupt_byte(&mut self) {
        // The controller consumed this corrupt byte. Discard the remaining
        // positions of its known three-byte packet, then seek a new header.
        // Repeated corrupt bytes still advance this bounded recovery count.
        self.discard = if self.discard != 0 {
            self.discard - 1
        } else {
            2 - self.length
        };
        self.length = 0;
        self.bytes = [0; 3];
    }

    fn discard_partial(&mut self) {
        // A global input barrier can run between bytes of a packet. Drop its
        // remaining tail as transport framing, not as new input/header bytes.
        if self.length != 0 {
            self.discard = 3 - self.length;
        }
        self.length = 0;
        self.bytes = [0; 3];
    }
}

struct State {
    active: bool,
    decoder: Decoder,
    queue: [Option<QueuedEvent>; QUEUE_CAPACITY],
    head: usize,
    length: usize,
    overflow: bool,
}

impl State {
    const fn new() -> Self {
        Self {
            active: false,
            decoder: Decoder::new(),
            queue: [None; QUEUE_CAPACITY],
            head: 0,
            length: 0,
            overflow: false,
        }
    }

    fn activate(&mut self) {
        *self = Self::new();
        self.active = true;
    }

    fn lose_input(&mut self) {
        self.queue.fill(None);
        self.head = 0;
        self.length = 0;
        self.overflow = true;
        // This decoder keeps no previous button levels or click history.
        // The consumer must reset its own history upon acknowledging loss.
    }

    fn accept(&mut self, status: u8, byte: u8, next_sequence: impl FnOnce() -> u64) {
        if !self.active || status & (OUTPUT_FULL | AUXILIARY) != (OUTPUT_FULL | AUXILIARY) {
            return;
        }
        if status & RECEIVE_ERROR != 0 {
            self.decoder.corrupt_byte();
            self.lose_input();
            return;
        }
        let Some(packet) = self.decoder.feed(byte) else {
            return;
        };
        if packet.overflow {
            // Signal lost motion before publishing its zero-motion button
            // snapshot. The consumer disarms click edges until release.
            self.lose_input();
        } else if self.length == QUEUE_CAPACITY {
            // Decoder is at a complete packet boundary. Drop the whole queue
            // and this packet; the next complete packet supplies fresh levels.
            self.lose_input();
            return;
        }
        let tail = (self.head + self.length) % QUEUE_CAPACITY;
        self.queue[tail] = Some(QueuedEvent {
            sequence: next_sequence(),
            event: packet.event,
        });
        self.length += 1;
    }

    fn pop(&mut self) -> Option<MouseEvent> {
        if self.overflow || self.length == 0 {
            return None;
        }
        let event = self.queue[self.head].take();
        self.head = (self.head + 1) % QUEUE_CAPACITY;
        self.length -= 1;
        event.map(|queued| queued.event)
    }

    fn peek_sequence(&self) -> Option<u64> {
        if self.overflow || self.length == 0 {
            return None;
        }
        self.queue[self.head].map(|queued| queued.sequence)
    }

    fn take_overflow(&mut self) -> bool {
        core::mem::take(&mut self.overflow)
    }

    fn discard_input(&mut self) {
        self.queue.fill(None);
        self.head = 0;
        self.length = 0;
        self.overflow = false;
        self.decoder.discard_partial();
    }
}

struct SharedState(UnsafeCell<State>);

// SAFETY: Mirror currently starts only its BSP. All accesses exclude IRQs,
// and the shared controller never retains a decoder borrow across callbacks.
unsafe impl Sync for SharedState {}

static STATE: SharedState = SharedState(UnsafeCell::new(State::new()));

fn with_state<R>(operation: impl FnOnce(&mut State) -> R) -> R {
    let enabled = arch::save_and_disable_interrupts();
    // SAFETY: BSP-only interrupt exclusion provides this exclusive borrow.
    let result = operation(unsafe { &mut *STATE.0.get() });
    // SAFETY: The state borrow ended; previously active handlers remain valid.
    unsafe { arch::restore_interrupts(enabled) };
    result
}

/// Called by the sole controller owner with IF clear, after validating mouse
/// ID 0 and consuming the final enable ACK. No hardware access occurs here.
pub(crate) fn activate() {
    // SAFETY: The controller owner holds BSP interrupt exclusion for activation.
    unsafe { &mut *STATE.0.get() }.activate();
}

/// Accept one controller-routed byte with IF clear. This function never polls
/// the controller, allocates, prints, enables interrupts, or retains a borrow.
pub(crate) fn accept_byte(status: u8, byte: u8) {
    // SAFETY: The sole controller owner calls with IF clear on the boot CPU.
    unsafe { &mut *STATE.0.get() }.accept(status, byte, ps2::next_sequence);
}

/// Internal ordered-dispatch consumer. Preserves IF and never polls hardware;
/// ps2::next_event performs the shared drain before borrowing either queue.
pub(crate) fn pop_event() -> Option<MouseEvent> {
    with_state(State::pop)
}

/// Inspect the oldest publication sequence for the controller's ordered merge.
pub(crate) fn peek_sequence() -> Option<u64> {
    with_state(|state| state.peek_sequence())
}

/// Acknowledge lost input, preserving IF. The shared dispatcher then discards
/// both queues and returns a global loss event before any subsequent input.
pub(crate) fn take_overflow() -> bool {
    with_state(State::take_overflow)
}

/// Global loss barrier: clear queued events and the loss flag, preserving only
/// a bounded partial-packet tail discard so stale payload cannot become a header.
/// The pointer consumer clears button history and waits for a released snapshot.
pub(crate) fn discard_input() {
    with_state(State::discard_input);
}

/// Delivered IRQ12 count, including possible empty/command-ACK deliveries.
/// Decoded input evidence must also include actual motion or button events.
/// Only trace markers report it (spec 0010-D4).
#[cfg(feature = "test-trace")]
pub fn irq_count() -> u64 {
    ps2::mouse_irq_count()
}

/// Exercise the real merged consumer before controller initialization. No
/// device-state borrow spans the callback, which may use all consumer paths.
#[cfg(feature = "boot-smoke")]
pub(crate) fn with_test_input(operation: impl FnOnce()) {
    assert!(!arch::interrupts_enabled());
    with_state(|state| assert!(!state.active));
    activate();
    operation();
    assert!(!arch::interrupts_enabled());
    with_state(|state| *state = State::new());
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    const STATUS: u8 = OUTPUT_FULL | AUXILIARY;

    fn state() -> State {
        let mut state = State::new();
        state.activate();
        state
    }
    fn feed(state: &mut State, bytes: &[u8]) {
        for &byte in bytes {
            state.accept(STATUS, byte, || 0);
        }
    }
    fn event(state: &mut State) -> MouseEvent {
        state.pop().expect("missing mouse test event")
    }

    // Boot invokes this before controller init. These internal empty-queue
    // consumers must perform no I/O and preserve caller IF.
    let original_if = arch::interrupts_enabled();
    assert!(pop_event().is_none());
    assert_eq!(arch::interrupts_enabled(), original_if);
    assert_eq!(peek_sequence(), None);
    assert_eq!(arch::interrupts_enabled(), original_if);
    discard_input();
    assert!(!take_overflow());
    assert_eq!(arch::interrupts_enabled(), original_if);
    assert!(!take_overflow());
    assert_eq!(arch::interrupts_enabled(), original_if);
    assert_eq!(irq_count(), 0);

    let mut queue = state();
    // Header resynchronization ignores bytes without mandatory bit 3. Payload
    // bytes do not have that restriction and may themselves look like headers.
    feed(&mut queue, &[0x00, 0x07, 0x80, 0x08, 0xff]);
    assert!(queue.pop().is_none());
    feed(&mut queue, &[0xff]);
    assert_eq!(
        event(&mut queue),
        MouseEvent {
            dx: 255,
            dy: -255,
            buttons: 0
        }
    );
    feed(&mut queue, &[0x38, 0x00, 0x00]);
    assert_eq!(
        event(&mut queue),
        MouseEvent {
            dx: -256,
            dy: 256,
            buttons: 0
        }
    );
    feed(&mut queue, &[0x38, 0xff, 0xff]);
    assert_eq!(
        event(&mut queue),
        MouseEvent {
            dx: -1,
            dy: 1,
            buttons: 0
        }
    );
    feed(&mut queue, &[0x18, 0x80, 0x80]);
    assert_eq!(
        event(&mut queue),
        MouseEvent {
            dx: -128,
            dy: -128,
            buttons: 0
        }
    );
    feed(&mut queue, &[0x28, 0x80, 0x80]);
    assert_eq!(
        event(&mut queue),
        MouseEvent {
            dx: 128,
            dy: 128,
            buttons: 0
        }
    );

    for buttons in 0..=BUTTON_MASK {
        feed(&mut queue, &[HEADER_SYNC | buttons, 0, 0]);
        assert_eq!(
            event(&mut queue),
            MouseEvent {
                dx: 0,
                dy: 0,
                buttons
            }
        );
    }

    // Either motion overflow bit invalidates both deltas, but not the button
    // levels. Loss must be acknowledged before the retained snapshot appears.
    for overflow in [0x40, 0x80, 0xc0] {
        feed(&mut queue, &[0x08, 1, 1]);
        feed(
            &mut queue,
            &[HEADER_SYNC | overflow | LEFT | MIDDLE, 255, 255],
        );
        assert!(queue.pop().is_none());
        assert!(queue.take_overflow());
        assert!(!queue.take_overflow());
        assert_eq!(
            event(&mut queue),
            MouseEvent {
                dx: 0,
                dy: 0,
                buttons: LEFT | MIDDLE
            }
        );
        assert!(queue.pop().is_none());
    }

    // Wrong-origin bytes and inactive input must not alter a partial packet.
    let mut inactive = State::new();
    feed(&mut inactive, &[0x09, 5, 6]);
    assert!(inactive.pop().is_none());
    feed(&mut queue, &[0x08]);
    queue.accept(OUTPUT_FULL, 0xff, || 0);
    queue.accept(OUTPUT_FULL | RECEIVE_ERROR, 0xff, || 0);
    queue.accept(AUXILIARY, 0xff, || 0);
    feed(&mut queue, &[2, 3]);
    assert_eq!(
        event(&mut queue),
        MouseEvent {
            dx: 2,
            dy: -3,
            buttons: 0
        }
    );
    assert!(!queue.take_overflow());

    // Every corrupt position discards the known remainder of its packet;
    // header-looking tail bytes must not invent a new button press.
    for error in [0x40, 0x80, RECEIVE_ERROR] {
        for position in 0..3 {
            let mut damaged = state();
            let packet = [0x0f, 0x0f, 0x0f];
            feed(&mut damaged, &packet[..position]);
            damaged.accept(STATUS | error, packet[position], || 0);
            feed(&mut damaged, &packet[position + 1..]);
            assert!(damaged.pop().is_none());
            assert!(damaged.take_overflow());
            assert_eq!(damaged.decoder.length, 0);
            assert_eq!(damaged.decoder.discard, 0);
            feed(&mut damaged, &[0x08, 4, 0]);
            assert_eq!(
                event(&mut damaged),
                MouseEvent {
                    dx: 4,
                    dy: 0,
                    buttons: 0
                }
            );
        }
    }
    queue.accept(STATUS | RECEIVE_ERROR, 0x0f, || 0);
    queue.accept(STATUS | RECEIVE_ERROR, 0x0f, || 0);
    queue.accept(STATUS | RECEIVE_ERROR, 0x0f, || 0);
    assert!(queue.take_overflow());
    feed(&mut queue, &[0x08, 0, 0]);
    assert_eq!(event(&mut queue).buttons, 0);

    // Reuse ring slots several times and prove FIFO ordering through wrap.
    for round in 0..3 {
        for index in 0..QUEUE_CAPACITY {
            feed(
                &mut queue,
                &[0x08, (round * QUEUE_CAPACITY + index) as u8, 0],
            );
        }
        for index in 0..QUEUE_CAPACITY {
            assert_eq!(
                event(&mut queue).dx,
                (round * QUEUE_CAPACITY + index) as i16
            );
        }
        assert!(queue.pop().is_none());
    }

    for _ in 0..=QUEUE_CAPACITY {
        feed(&mut queue, &[HEADER_SYNC | LEFT, 1, 0]);
    }
    assert!(queue.overflow && queue.length == 0);
    feed(&mut queue, &[HEADER_SYNC | LEFT, 0, 0]);
    assert!(queue.pop().is_none());
    assert!(queue.take_overflow());
    // Button levels are supplied without interpreting a fresh click. Pointer
    // tests cover suppressing this held press until the following release.
    assert_eq!(event(&mut queue).buttons, LEFT);
    feed(&mut queue, &[HEADER_SYNC, 0, 0, HEADER_SYNC | LEFT, 0, 0]);
    assert_eq!(event(&mut queue).buttons, 0);
    assert_eq!(event(&mut queue).buttons, LEFT);
    assert!(queue.pop().is_none());

    // Publication sequences are allocated only for completed, accepted events;
    // peek and pop retain ordering even across the global u64 sequence wrap.
    let mut ordered = state();
    let mut sequence = u64::MAX - 1;
    for byte in [0x08, 1, 0, 0x08, 2, 0, 0x08, 3, 0] {
        ordered.accept(STATUS, byte, || {
            let current = sequence;
            sequence = sequence.wrapping_add(1);
            current
        });
    }
    assert_eq!(sequence, 1);
    for (expected_sequence, expected_dx) in [(u64::MAX - 1, 1), (u64::MAX, 2), (0, 3)] {
        assert_eq!(ordered.peek_sequence(), Some(expected_sequence));
        assert_eq!(event(&mut ordered).dx, expected_dx);
    }
    assert_eq!(ordered.peek_sequence(), None);

    // A cross-device loss barrier drops complete queued events and partial
    // bytes, but preserves just enough framing to discard a header-looking tail.
    for prefix in 1..3 {
        let mut barrier = state();
        feed(&mut barrier, &[0x09, 1, 1]);
        feed(&mut barrier, &[0x0f, 0x0f][..prefix]);
        barrier.overflow = true;
        barrier.discard_input();
        assert!(!barrier.take_overflow());
        assert!(barrier.active);
        assert_eq!(barrier.peek_sequence(), None);
        feed(&mut barrier, &[0x0f, 0x0f][..3 - prefix]);
        assert!(barrier.pop().is_none());
        feed(&mut barrier, &[0x08, 7, 0]);
        assert_eq!(
            event(&mut barrier),
            MouseEvent {
                dx: 7,
                dy: 0,
                buttons: 0
            }
        );
    }
    let mut barrier = state();
    barrier.accept(STATUS | RECEIVE_ERROR, 0x0f, || 0);
    barrier.discard_input();
    assert!(!barrier.take_overflow());
    feed(&mut barrier, &[0x0f, 0x0f, 0x08, 8, 0]);
    assert_eq!(event(&mut barrier).dx, 8);

    crate::serial::println!("MIRROR_MOUSE_TESTS_OK");
}
