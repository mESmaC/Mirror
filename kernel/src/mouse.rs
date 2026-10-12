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
//!
//! While the VMware vmmouse is active (spec 0017c), this decoder keeps PS/2
//! framing but a completed packet is only a doorbell for a vmmouse drain, and
//! the ring also carries absolute positions (0017-D13, D15).

use core::cell::UnsafeCell;

use crate::{arch, ps2, timer, vmmouse};

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

/// A vmmouse absolute position in device units (spec 0017-D14): 0..=0xFFFF
/// spans the framebuffer. Consumers convert at use with `to_pixel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionEvent {
    pub x: u16,
    pub y: u16,
    /// Current button levels, not edges; LEFT | RIGHT | MIDDLE only.
    pub buttons: u8,
}

/// One entry of the shared mouse ring: relative motion, from PS/2 or a
/// relative vmmouse packet, or an absolute vmmouse position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Relative(MouseEvent),
    Absolute(PositionEvent),
}

impl Event {
    /// Fold a newer event into this newest queued one on a full ring: same-
    /// button relative motion adds with checked i16 sums (spec 0017a-D6), and
    /// a same-button absolute position replaces this one (0017-D15). A button
    /// change, a relative/absolute switch or an overflow refuses.
    fn absorb(&mut self, newer: Event) -> bool {
        match (self, newer) {
            (Self::Relative(queued), Self::Relative(motion))
                if queued.buttons == motion.buttons =>
            {
                let (Some(dx), Some(dy)) = (
                    queued.dx.checked_add(motion.dx),
                    queued.dy.checked_add(motion.dy),
                ) else {
                    return false;
                };
                queued.dx = dx;
                queued.dy = dy;
                true
            }
            (Self::Absolute(queued), Self::Absolute(position))
                if queued.buttons == position.buttons =>
            {
                *queued = position;
                true
            }
            _ => false,
        }
    }
}

/// Convert a device unit to a pixel index in `0..=last` (spec 0017-D14):
/// `(value * last + 0x7FFF) / 0xFFFF`, so 0 and 0xFFFF are the first and last
/// pixel. `last` is the extent minus one; the result never exceeds it.
pub fn to_pixel(value: u16, last: usize) -> usize {
    let scaled = (u128::from(value) * last as u128 + 0x7fff) / 0xffff;
    usize::try_from(scaled).expect("pixel within the extent")
}

struct Packet {
    event: MouseEvent,
    overflow: bool,
}

#[derive(Clone, Copy)]
struct QueuedEvent {
    sequence: u64,
    event: Event,
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
    /// vmmouse doorbell mode (spec 0017-D13). Only `vmmouse::init` sets it,
    /// after its probe and enable pass; a runtime fallback clears it until the
    /// next boot (0017-D16).
    doorbell: bool,
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
            doorbell: false,
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

    /// Decode one AUX byte. Returns true when it rang the vmmouse doorbell: in
    /// doorbell mode a completed packet, a motion-overflow packet or a receive
    /// error queues nothing and is no pointer loss, and the caller then runs
    /// one drain (spec 0017-D13). Framing (0017a-D7) is the same in both modes.
    fn accept(
        &mut self,
        status: u8,
        byte: u8,
        now: u64,
        mut next_sequence: impl FnMut() -> u64,
    ) -> bool {
        if !self.active || status & (OUTPUT_FULL | AUXILIARY) != (OUTPUT_FULL | AUXILIARY) {
            return false;
        }
        if status & RECEIVE_ERROR != 0 {
            self.decoder.corrupt_byte(now);
            if !self.doorbell {
                self.lose_input(&mut next_sequence);
            }
            return self.doorbell;
        }
        let Some(packet) = self.decoder.feed(byte, now) else {
            return false;
        };
        if self.doorbell {
            // The packet's motion and buttons are discarded: the vmmouse
            // queue carries the input this packet announces.
            return true;
        }
        if packet.overflow {
            // Signal lost motion before publishing its zero-motion button
            // snapshot. The consumer disarms click edges until release.
            self.lose_input(&mut next_sequence);
        }
        self.publish(Event::Relative(packet.event), &mut next_sequence);
        false
    }

    /// Queue one completed event under the next shared sequence. On a full
    /// ring (spec 0017a-D6, 0017-D15) the newest queued event absorbs it and
    /// takes its sequence, so it still sorts after every key published before
    /// it; when it cannot, that is a pointer loss, and the next event supplies
    /// fresh levels.
    fn publish(&mut self, event: Event, next_sequence: &mut impl FnMut() -> u64) {
        if self.length == QUEUE_CAPACITY {
            let newest = (self.head + QUEUE_CAPACITY - 1) % QUEUE_CAPACITY;
            if let Some(queued) = &mut self.queue[newest]
                && queued.event.absorb(event)
            {
                queued.sequence = next_sequence();
            } else {
                self.lose_input(next_sequence);
            }
            return;
        }
        let tail = (self.head + self.length) % QUEUE_CAPACITY;
        self.queue[tail] = Some(QueuedEvent {
            sequence: next_sequence(),
            event,
        });
        self.length += 1;
    }

    /// One vmmouse drain into this ring (spec 0017-D13). A protocol error has
    /// already disabled the device; it ends doorbell mode for good and is a
    /// pointer loss (0017-D16).
    fn drain(
        &mut self,
        backdoor: &mut impl vmmouse::Backdoor,
        mut next_sequence: impl FnMut() -> u64,
    ) -> vmmouse::Drain {
        debug_assert!(self.active && self.doorbell);
        let result = vmmouse::drain(backdoor, |event| self.publish(event, &mut next_sequence));
        if let vmmouse::Drain::Failed(_) = result {
            self.doorbell = false;
            self.lose_input(&mut next_sequence);
        }
        result
    }

    fn pop(&mut self) -> Option<Event> {
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
/// A rung doorbell drains the vmmouse here (spec 0017-D13).
pub(crate) fn accept_byte(status: u8, byte: u8) {
    let now = timer::ticks();
    // SAFETY: The sole controller owner calls with IF clear on the boot CPU.
    let state = unsafe { &mut *STATE.0.get() };
    if state.accept(status, byte, now, ps2::next_sequence) {
        drain_device(state);
    }
}

/// One drain on the real backdoor with IF clear, recording its outcome. It
/// never prints; ps2::next_event reports a fallback.
fn drain_device(state: &mut State) {
    if !state.doorbell {
        return;
    }
    let mut port = {
        // SAFETY: Doorbell mode is set only by vmmouse::init after GETVERSION
        // found the backdoor on a hypervisor, and every caller holds this
        // state with IF clear on the BSP, at CPL0.
        unsafe { vmmouse::Port::new() }
    };
    vmmouse::record(state.drain(&mut port, ps2::next_sequence));
}

/// Continue a vmmouse drain that stopped at its 64-packet bound (called by
/// ps2::poll_pending), or run the VmmouseError fault's one drain. Preserves
/// IF; does nothing outside doorbell mode.
pub(crate) fn drain_vmmouse() {
    with_state(drain_device);
}

/// Called only by vmmouse::init with IF clear, after a successful enable and
/// before IRQ12 is unmasked: completed PS/2 packets become doorbells.
pub(crate) fn enter_doorbell() {
    with_state(|state| {
        assert!(state.active, "vmmouse without an active PS/2 mouse");
        state.doorbell = true;
    });
}

/// Internal ordered-dispatch consumer. Preserves IF and never polls hardware;
/// ps2::next_event performs the shared drain before borrowing either queue.
pub(crate) fn pop_event() -> Option<Event> {
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

/// Publish one event into the real ring as a drain would, under the shared
/// sequence. Input-session tests only, inside `with_test_input`.
#[cfg(feature = "boot-smoke")]
pub(crate) fn publish_test_event(event: Event) {
    with_state(|state| {
        assert!(state.active && !state.doorbell);
        let mut sequence = ps2::next_sequence;
        state.publish(event, &mut sequence);
    });
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

/// A queued relative event's motion; an absolute position is a test failure.
#[cfg(feature = "boot-smoke")]
fn relative(event: Option<Event>) -> Option<MouseEvent> {
    event.map(|event| match event {
        Event::Relative(motion) => motion,
        Event::Absolute(_) => panic!("unexpected absolute mouse event"),
    })
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
        relative(state.pop()).expect("missing mouse test event")
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
    while let Some(motion) = relative(starved.pop()) {
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
        let newest = relative(saturated.queue[QUEUE_CAPACITY - 1].map(|queued| queued.event));
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
        assert_eq!(relative(framing.pop()), expected);
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

/// Doorbell mode, absolute events in the shared ring and the runtime fallback
/// (spec 0017c acceptance test 1), on private decoder states fed by fake
/// vmmouse queues. Called by vmmouse::self_test; performs no port I/O.
#[cfg(feature = "boot-smoke")]
pub(crate) fn doorbell_self_test() {
    const STATUS: u8 = OUTPUT_FULL | AUXILIARY;

    fn doorbell() -> State {
        let mut state = State::new();
        state.activate();
        state.doorbell = true;
        state
    }
    fn counter(sequence: &mut u64) -> impl FnMut() -> u64 + '_ {
        move || {
            let current = *sequence;
            *sequence = current.wrapping_add(1);
            current
        }
    }
    /// Drain until the fake queue is empty; returns the number of drains.
    fn drain_all(state: &mut State, device: &mut vmmouse::Stream, sequence: &mut u64) -> usize {
        for drains in 1..=64 {
            match state.drain(device, counter(sequence)) {
                vmmouse::Drain::More => {}
                vmmouse::Drain::Done => return drains,
                vmmouse::Drain::Failed(reason) => panic!("unexpected vmmouse fallback {reason:?}"),
            }
        }
        panic!("vmmouse drains did not empty the queue");
    }
    fn position(state: &mut State) -> PositionEvent {
        match state.pop() {
            Some(Event::Absolute(position)) => position,
            other => panic!("expected an absolute mouse event, got {other:?}"),
        }
    }
    fn at(index: u16, buttons: u8) -> PositionEvent {
        PositionEvent {
            x: index,
            y: index * 2,
            buttons,
        }
    }

    // A complete PS/2 packet, one with motion overflow included, queues
    // nothing, allocates no sequence and rings once, on its last byte.
    let mut state = doorbell();
    for packet in [[0x09, 5, 5], [0x48 | LEFT, 255, 0], [0x08, 0, 0]] {
        let rung = packet.map(|byte| state.accept(STATUS, byte, 0, || panic!("doorbell sequence")));
        assert_eq!(rung, [false, false, true]);
    }
    assert!(state.pop().is_none() && state.lost.is_none() && state.length == 0);
    // A receive error rings and is no pointer loss. Framing still discards
    // the rest of its packet, so the header-looking tail does not ring.
    assert!(state.accept(STATUS | RECEIVE_ERROR, 0x09, 0, || panic!("loss sequence")));
    assert!(!state.accept(STATUS, 0x09, 0, || panic!("tail sequence")));
    assert!(!state.accept(STATUS, 0x09, 0, || panic!("tail sequence")));
    assert!(state.lost.is_none() && state.peek_sequence().is_none());
    // Wrong-origin bytes never ring.
    assert!(!state.accept(OUTPUT_FULL, 0x08, 0, || 0));
    let mut sequence = 0;
    let mut device = vmmouse::Stream::new(0, 0, false);
    assert_eq!(drain_all(&mut state, &mut device, &mut sequence), 1);
    assert!(state.pop().is_none() && sequence == 0);

    // Drained positions keep device units and take shared sequences in order;
    // relative packets carry dx = x, dy = -y.
    let mut device = vmmouse::Stream::new(3, LEFT, false);
    assert_eq!(drain_all(&mut state, &mut device, &mut sequence), 1);
    let mut device = vmmouse::Stream::new(2, RIGHT, true);
    assert_eq!(drain_all(&mut state, &mut device, &mut sequence), 1);
    for index in 0..3 {
        assert_eq!(state.peek_sequence(), Some(u64::from(index)));
        assert_eq!(position(&mut state), at(index, LEFT));
    }
    for dx in 0..2 {
        assert_eq!(
            relative(state.pop()),
            Some(MouseEvent {
                dx,
                dy: -2 * dx,
                buttons: RIGHT
            })
        );
    }
    assert!(state.pop().is_none() && !state.take_lost());

    // 64 + 1000 same-button absolute events with no consumer lose nothing:
    // sixteen drains stop at the 64-packet bound, the seventeenth empties the
    // queue, and the newest position replaces the newest queued one and takes
    // its sequence (0017-D15).
    let mut starved = doorbell();
    let mut sequence = 0;
    let total = QUEUE_CAPACITY as u16 + 1000;
    let mut device = vmmouse::Stream::new(u32::from(total), MIDDLE, false);
    assert_eq!(drain_all(&mut starved, &mut device, &mut sequence), 17);
    assert_eq!((device.remaining, device.drains), (0, 17));
    assert!(starved.lost.is_none() && starved.length == QUEUE_CAPACITY);
    assert_eq!(sequence, u64::from(total));
    for index in 0..QUEUE_CAPACITY as u16 - 1 {
        assert_eq!(position(&mut starved), at(index, MIDDLE));
    }
    assert_eq!(starved.peek_sequence(), Some(u64::from(total) - 1));
    assert_eq!(position(&mut starved), at(total - 1, MIDDLE));
    assert!(starved.pop().is_none() && !starved.take_lost());

    // On a full ring a button change, or a relative/absolute switch, is a
    // pointer loss; same-button relative motion still merges (0017a-D6).
    for (full_relative, buttons, next_relative, lost) in [
        (false, LEFT, false, true),
        (false, MIDDLE, true, true),
        (true, MIDDLE, false, true),
        (true, LEFT, true, true),
        (true, MIDDLE, true, false),
    ] {
        let mut full = doorbell();
        let mut sequence = 0;
        let capacity = QUEUE_CAPACITY as u32;
        let mut device = vmmouse::Stream::new(capacity, MIDDLE, full_relative);
        drain_all(&mut full, &mut device, &mut sequence);
        assert!(full.length == QUEUE_CAPACITY && full.lost.is_none());
        let mut device = vmmouse::Stream::new(1, buttons, next_relative);
        drain_all(&mut full, &mut device, &mut sequence);
        assert_eq!(full.lost, lost.then_some(QUEUE_CAPACITY as u64));
        assert_eq!(full.length, if lost { 0 } else { QUEUE_CAPACITY });
        if lost {
            assert!(full.pop().is_none());
            assert!(full.take_lost());
            // The next event supplies fresh levels.
            let mut device = vmmouse::Stream::new(1, LEFT, false);
            drain_all(&mut full, &mut device, &mut sequence);
            assert_eq!(position(&mut full), at(0, LEFT));
            assert!(full.pop().is_none());
        }
    }

    // A runtime error (0017-D16), a STATUS error or a queue length of 6,
    // disables the device, drops the ring for a pointer loss and leaves
    // doorbell mode for good: PS/2 packets decode as relative motion again.
    for (stray, error, reason) in [
        (0, true, vmmouse::Fallback::Status),
        (2, false, vmmouse::Fallback::QueueLength),
    ] {
        let mut failed = doorbell();
        let mut sequence = 0;
        let mut device = vmmouse::Stream::new(2, LEFT, false);
        drain_all(&mut failed, &mut device, &mut sequence);
        let mut device = vmmouse::Stream::new(1, LEFT, false);
        device.stray = stray;
        device.error = error;
        assert_eq!(
            failed.drain(&mut device, counter(&mut sequence)),
            vmmouse::Drain::Failed(reason)
        );
        assert!(device.disabled && device.remaining == 1);
        assert!(!failed.doorbell && failed.length == 0);
        assert_eq!(failed.peek_sequence(), Some(2));
        assert!(failed.pop().is_none() && failed.take_lost());
        for byte in [0x08 | LEFT, 4, 0] {
            assert!(!failed.accept(STATUS, byte, 0, counter(&mut sequence)));
        }
        assert_eq!(
            relative(failed.pop()),
            Some(MouseEvent {
                dx: 4,
                dy: 0,
                buttons: LEFT
            })
        );
        // A receive error is a pointer loss again, and rings nothing.
        assert!(!failed.accept(STATUS | RECEIVE_ERROR, 0, 0, || 9));
        assert_eq!(failed.lost, Some(9));
    }
}
