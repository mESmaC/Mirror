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
//! The 500 ms framing-recovery gap matches linux `psmouse-base.c`, which uses
//! HZ/2 (spec 0017a-D7); no source code is copied.

use core::cell::UnsafeCell;

use crate::{arch, ps2, timer};

pub const LEFT: u8 = 1;
pub const RIGHT: u8 = 2;
pub const MIDDLE: u8 = 4;
const BUTTON_MASK: u8 = LEFT | RIGHT | MIDDLE;
const OUTPUT_FULL: u8 = 1;
const AUXILIARY: u8 = 0x20;
const RECEIVE_ERROR: u8 = 0xc0;
const HEADER_SYNC: u8 = 0x08;
const MOTION_OVERFLOW: u8 = 0xc0;
pub(crate) const QUEUE_CAPACITY: usize = 64;
/// 500 ms at the 100 Hz timer. A partial packet whose previous byte is older
/// than this is dropped, and the next byte becomes a candidate header (D7).
const RESYNC_TICKS: u64 = 50;

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
    /// Timer tick of the previous byte, consulted only mid-packet.
    last: u64,
}

impl Decoder {
    const fn new() -> Self {
        Self {
            bytes: [0; 3],
            length: 0,
            discard: 0,
            last: 0,
        }
    }

    /// Framing recovery (spec 0017a-D7). A partial packet, including the
    /// remaining tail of a corrupt one, whose previous byte arrived more than
    /// 500 ms before `now` is dropped silently. The byte arriving at `now`
    /// then starts afresh and must still pass the header-bit-3 check.
    fn expire(&mut self, now: u64) {
        if (self.length != 0 || self.discard != 0) && now.wrapping_sub(self.last) > RESYNC_TICKS {
            self.length = 0;
            self.discard = 0;
            self.bytes = [0; 3];
        }
        self.last = now;
    }

    fn feed(&mut self, byte: u8, now: u64) -> Option<Packet> {
        self.expire(now);
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

    fn corrupt_byte(&mut self, now: u64) {
        self.expire(now);
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
        // Controller handoff (ps2 stop_routing) can run between bytes of a
        // packet. Drop its remaining tail as transport framing, not as new
        // input/header bytes.
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
    /// Sequence of the first pointer loss since the consumer last took this
    /// device's loss marker (spec 0017a-D3). Every queued event is newer.
    lost: Option<u64>,
}

impl State {
    const fn new() -> Self {
        Self {
            active: false,
            decoder: Decoder::new(),
            queue: [None; QUEUE_CAPACITY],
            head: 0,
            length: 0,
            lost: None,
        }
    }

    fn activate(&mut self) {
        *self = Self::new();
        self.active = true;
    }

    fn clear_queue(&mut self) {
        self.queue.fill(None);
        self.head = 0;
        self.length = 0;
    }

    /// A pointer loss drops only this device's queue. Its marker takes the
    /// next shared sequence unless an unacknowledged loss already holds one,
    /// so it orders against keyboard events without discarding any of them.
    fn lose_input(&mut self, next_sequence: impl FnOnce() -> u64) {
        self.clear_queue();
        if self.lost.is_none() {
            self.lost = Some(next_sequence());
        }
        // This decoder keeps no previous button levels or click history.
        // The consumer must reset its own history upon acknowledging loss.
    }

    fn accept(&mut self, status: u8, byte: u8, now: u64, mut next_sequence: impl FnMut() -> u64) {
        if !self.active || status & (OUTPUT_FULL | AUXILIARY) != (OUTPUT_FULL | AUXILIARY) {
            return;
        }
        if status & RECEIVE_ERROR != 0 {
            self.decoder.corrupt_byte(now);
            self.lose_input(&mut next_sequence);
            return;
        }
        let Some(packet) = self.decoder.feed(byte, now) else {
            return;
        };
        if packet.overflow {
            // Signal lost motion before publishing its zero-motion button
            // snapshot. The consumer disarms click edges until release.
            self.lose_input(&mut next_sequence);
        } else if self.length == QUEUE_CAPACITY {
            // Starvation (spec 0017a-D6): add same-button motion into the
            // newest queued event, which takes this packet's sequence and so
            // still sorts after every key published before this packet. A
            // button change or an i16 overflow is a pointer loss; the decoder
            // is at a packet boundary, so the next packet supplies fresh levels.
            let newest = (self.head + QUEUE_CAPACITY - 1) % QUEUE_CAPACITY;
            if let Some(queued) = &mut self.queue[newest]
                && queued.event.buttons == packet.event.buttons
                && let Some(dx) = queued.event.dx.checked_add(packet.event.dx)
                && let Some(dy) = queued.event.dy.checked_add(packet.event.dy)
            {
                queued.event.dx = dx;
                queued.event.dy = dy;
                queued.sequence = next_sequence();
            } else {
                self.lose_input(&mut next_sequence);
            }
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
        // The consumer must take the loss marker before any later event.
        if self.lost.is_some() || self.length == 0 {
            return None;
        }
        let event = self.queue[self.head].take();
        self.head = (self.head + 1) % QUEUE_CAPACITY;
        self.length -= 1;
        event.map(|queued| queued.event)
    }

    /// The pending loss marker sorts ahead of the queue head: every queued
    /// event was published after the loss that cleared the queue.
    fn peek_sequence(&self) -> Option<u64> {
        if self.lost.is_some() {
            return self.lost;
        }
        if self.length == 0 {
            return None;
        }
        self.queue[self.head].map(|queued| queued.sequence)
    }

    fn take_lost(&mut self) -> bool {
        self.lost.take().is_some()
    }

    fn discard_input(&mut self) {
        self.clear_queue();
        self.lost = None;
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
/// The delivered timer tick count dates the byte for framing recovery (D7).
pub(crate) fn accept_byte(status: u8, byte: u8) {
    let now = timer::ticks();
    // SAFETY: The sole controller owner calls with IF clear on the boot CPU.
    unsafe { &mut *STATE.0.get() }.accept(status, byte, now, ps2::next_sequence);
}

/// Internal ordered-dispatch consumer. Preserves IF and never polls hardware;
/// ps2::next_event performs the shared drain before borrowing either queue.
pub(crate) fn pop_event() -> Option<MouseEvent> {
    with_state(State::pop)
}

/// Inspect the oldest publication sequence for the controller's ordered merge:
/// a pending loss marker first, then the queue head.
pub(crate) fn peek_sequence() -> Option<u64> {
    with_state(|state| state.peek_sequence())
}

/// Acknowledge a pending pointer loss, preserving IF. The shared dispatcher
/// returns its marker in sequence order; keyboard events are never discarded.
pub(crate) fn take_lost() -> bool {
    with_state(State::take_lost)
}

/// Controller handoff only (ps2 stop_routing): clear queued events and any
/// pending loss, preserving a bounded partial-packet tail discard so stale
/// payload cannot become a header.
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
            state.accept(STATUS, byte, 0, || 0);
        }
    }
    /// Publish through an explicit shared counter, as ps2::next_sequence does.
    fn feed_sequenced(state: &mut State, bytes: &[u8], sequence: &mut u64) {
        for &byte in bytes {
            state.accept(STATUS, byte, 0, || {
                let current = *sequence;
                *sequence = sequence.wrapping_add(1);
                current
            });
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
    assert!(!take_lost());
    assert_eq!(arch::interrupts_enabled(), original_if);
    assert!(!take_lost());
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
        assert!(queue.take_lost());
        assert!(!queue.take_lost());
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

    // The loss marker takes a shared sequence of its own, after the dropped
    // event and before the snapshot. A second loss before the consumer takes
    // the marker keeps the first loss's sequence and allocates no other.
    let mut marked = state();
    let mut sequence = 10;
    feed_sequenced(&mut marked, &[0x08, 1, 1], &mut sequence);
    feed_sequenced(&mut marked, &[0x48 | LEFT, 255, 0], &mut sequence);
    assert_eq!(sequence, 13);
    assert_eq!(marked.peek_sequence(), Some(11));
    marked.accept(STATUS | RECEIVE_ERROR, 0x08, 0, || {
        panic!("second loss sequence")
    });
    feed(&mut marked, &[0x09, 0x09]);
    assert_eq!(marked.peek_sequence(), Some(11));
    assert!(marked.pop().is_none());
    assert!(marked.take_lost());
    assert_eq!(marked.peek_sequence(), None);

    // Wrong-origin bytes and inactive input must not alter a partial packet.
    let mut inactive = State::new();
    feed(&mut inactive, &[0x09, 5, 6]);
    assert!(inactive.pop().is_none());
    feed(&mut queue, &[0x08]);
    queue.accept(OUTPUT_FULL, 0xff, 0, || 0);
    queue.accept(OUTPUT_FULL | RECEIVE_ERROR, 0xff, 0, || 0);
    queue.accept(AUXILIARY, 0xff, 0, || 0);
    feed(&mut queue, &[2, 3]);
    assert_eq!(
        event(&mut queue),
        MouseEvent {
            dx: 2,
            dy: -3,
            buttons: 0
        }
    );
    assert!(!queue.take_lost());

    // Every corrupt position discards the known remainder of its packet;
    // header-looking tail bytes must not invent a new button press.
    for error in [0x40, 0x80, RECEIVE_ERROR] {
        for position in 0..3 {
            let mut damaged = state();
            let packet = [0x0f, 0x0f, 0x0f];
            feed(&mut damaged, &packet[..position]);
            damaged.accept(STATUS | error, packet[position], 0, || 0);
            feed(&mut damaged, &packet[position + 1..]);
            assert!(damaged.pop().is_none());
            assert!(damaged.take_lost());
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
    queue.accept(STATUS | RECEIVE_ERROR, 0x0f, 0, || 0);
    queue.accept(STATUS | RECEIVE_ERROR, 0x0f, 0, || 0);
    queue.accept(STATUS | RECEIVE_ERROR, 0x0f, 0, || 0);
    assert!(queue.take_lost());
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

    // A button change on a full ring is a pointer loss (spec 0017a-D6): the
    // ring and that packet are dropped, and the next packet supplies levels.
    for _ in 0..QUEUE_CAPACITY {
        feed(&mut queue, &[HEADER_SYNC | LEFT, 1, 0]);
    }
    assert!(queue.lost.is_none() && queue.length == QUEUE_CAPACITY);
    feed(&mut queue, &[HEADER_SYNC, 1, 0]);
    assert!(queue.lost.is_some() && queue.length == 0);
    feed(&mut queue, &[HEADER_SYNC | LEFT, 0, 0]);
    assert!(queue.pop().is_none());
    assert!(queue.take_lost());
    // Button levels are supplied without interpreting a fresh click. Pointer
    // tests cover suppressing this held press until the following release.
    assert_eq!(event(&mut queue).buttons, LEFT);
    feed(&mut queue, &[HEADER_SYNC, 0, 0, HEADER_SYNC | LEFT, 0, 0]);
    assert_eq!(event(&mut queue).buttons, 0);
    assert_eq!(event(&mut queue).buttons, LEFT);
    assert!(queue.pop().is_none());

    // Starvation without a consumer: 64 + 1000 same-button packets lose
    // nothing. The newest queued event absorbs the excess motion.
    let mut starved = state();
    for _ in 0..QUEUE_CAPACITY + 1000 {
        feed(&mut starved, &[HEADER_SYNC, 1, 0]);
    }
    assert!(starved.lost.is_none() && starved.length == QUEUE_CAPACITY);
    let (mut count, mut total) = (0, 0_i32);
    while let Some(motion) = starved.pop() {
        assert_eq!((motion.dy, motion.buttons), (0, 0));
        count += 1;
        total += i32::from(motion.dx);
    }
    assert_eq!((count, total), (QUEUE_CAPACITY, 1064));
    assert!(!starved.take_lost());

    // The merged event takes the newest packet's sequence, so a sequence
    // allocated between two merged packets (a key) sorts before it, here
    // across the u64 wrap.
    let mut merged = state();
    let mut sequence = u64::MAX - QUEUE_CAPACITY as u64;
    for _ in 0..QUEUE_CAPACITY {
        feed_sequenced(&mut merged, &[HEADER_SYNC, 1, 0], &mut sequence);
    }
    feed_sequenced(&mut merged, &[HEADER_SYNC, 2, 0], &mut sequence);
    let key = sequence;
    sequence = sequence.wrapping_add(1);
    feed_sequenced(&mut merged, &[HEADER_SYNC, 3, 0], &mut sequence);
    let newest = sequence.wrapping_sub(1);
    assert_eq!((key, newest), (0, 1));
    assert!(merged.lost.is_none() && merged.length == QUEUE_CAPACITY);
    for _ in 0..QUEUE_CAPACITY - 1 {
        assert_eq!(event(&mut merged).dx, 1);
    }
    assert_eq!(merged.peek_sequence(), Some(newest));
    assert!((key.wrapping_sub(newest) as i64) < 0);
    assert_eq!(
        event(&mut merged),
        MouseEvent {
            dx: 6,
            dy: 0,
            buttons: 0
        }
    );
    assert!(merged.pop().is_none());

    // Checked i16 sums: reaching i16::MIN exactly still merges, and one more
    // packet beyond either bound is a pointer loss that drops the ring.
    // Header 0x18 is dx -256; header 0x28 with payload 0 is dy +256.
    for (header, merges, sum) in [(0x18, 127, i16::MIN), (0x28, 126, 127 * 256)] {
        let mut saturated = state();
        for _ in 0..QUEUE_CAPACITY + merges {
            feed(&mut saturated, &[header, 0, 0]);
        }
        assert!(saturated.lost.is_none() && saturated.length == QUEUE_CAPACITY);
        let newest = saturated.queue[QUEUE_CAPACITY - 1].map(|queued| queued.event);
        let motion = newest.expect("newest saturated event");
        assert_eq!(if header == 0x18 { motion.dx } else { motion.dy }, sum);
        feed(&mut saturated, &[header, 0, 0]);
        assert!(saturated.lost.is_some() && saturated.length == 0);
        assert!(saturated.pop().is_none());
        assert!(saturated.take_lost());
        assert!(saturated.pop().is_none());
    }

    // Framing recovery (spec 0017a-D7): header 0x09, then a gap of more
    // than 50 ticks drops it silently and 08 05 00 decodes on its own. A gap
    // of 50 ticks or less completes the stale packet 09 08 05 instead, and
    // the trailing 00 fails the header-bit-3 check.
    let stale = MouseEvent {
        dx: 8,
        dy: -5,
        buttons: LEFT,
    };
    let fresh = MouseEvent {
        dx: 5,
        dy: 0,
        buttons: 0,
    };
    for (advance, expected) in [(51, fresh), (49, stale), (50, stale)] {
        let mut framing = state();
        let start = 1000;
        framing.accept(STATUS, 0x09, start, || 0);
        for byte in [0x08, 5, 0] {
            framing.accept(STATUS, byte, start + advance, || 0);
        }
        assert_eq!(event(&mut framing), expected);
        assert!(framing.pop().is_none());
        assert!(!framing.take_lost());
        assert_eq!(framing.decoder.length, 0);
    }
    // A corrupt packet's pending tail expires the same way: after the gap the
    // next byte is a candidate header rather than discarded payload.
    for (advance, expected) in [(51, Some(fresh)), (49, None)] {
        let mut framing = state();
        framing.accept(STATUS | RECEIVE_ERROR, 0x09, 1000, || 0);
        assert!(framing.take_lost());
        for byte in [0x08, 5, 0] {
            framing.accept(STATUS, byte, 1000 + advance, || 0);
        }
        assert_eq!(framing.pop(), expected);
        assert!(framing.pop().is_none());
    }

    // Publication sequences are allocated only for completed, accepted events;
    // peek and pop retain ordering even across the global u64 sequence wrap.
    let mut ordered = state();
    let mut sequence = u64::MAX - 1;
    feed_sequenced(
        &mut ordered,
        &[0x08, 1, 0, 0x08, 2, 0, 0x08, 3, 0],
        &mut sequence,
    );
    assert_eq!(sequence, 1);
    for (expected_sequence, expected_dx) in [(u64::MAX - 1, 1), (u64::MAX, 2), (0, 3)] {
        assert_eq!(ordered.peek_sequence(), Some(expected_sequence));
        assert_eq!(event(&mut ordered).dx, expected_dx);
    }
    assert_eq!(ordered.peek_sequence(), None);

    // Controller handoff (ps2 stop_routing) drops complete queued events,
    // any pending loss and partial bytes, but preserves just enough framing
    // to discard a header-looking tail.
    for prefix in 1..3 {
        let mut handoff = state();
        feed(&mut handoff, &[0x09, 1, 1]);
        feed(&mut handoff, &[0x0f, 0x0f][..prefix]);
        handoff.lost = Some(0);
        handoff.discard_input();
        assert!(!handoff.take_lost());
        assert!(handoff.active);
        assert_eq!(handoff.peek_sequence(), None);
        feed(&mut handoff, &[0x0f, 0x0f][..3 - prefix]);
        assert!(handoff.pop().is_none());
        feed(&mut handoff, &[0x08, 7, 0]);
        assert_eq!(
            event(&mut handoff),
            MouseEvent {
                dx: 7,
                dy: 0,
                buttons: 0
            }
        );
    }
    let mut handoff = state();
    handoff.accept(STATUS | RECEIVE_ERROR, 0x0f, 0, || 0);
    handoff.discard_input();
    assert!(!handoff.take_lost());
    feed(&mut handoff, &[0x0f, 0x0f, 0x08, 8, 0]);
    assert_eq!(event(&mut handoff).dx, 8);

    crate::serial::println!("MIRROR_MOUSE_TESTS_OK");
}
