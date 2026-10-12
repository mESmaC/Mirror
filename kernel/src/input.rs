//! Versioned userspace input records over an exclusive completed-event lease.
//!
//! The session is owned by the scheduler, never by IRQ handlers. A pending
//! record is copied by value and committed only after successful user copyout;
//! failed requests cannot consume it or reorder later device events.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::framebuffer::SurfaceRegion;
use crate::{keyboard::Key, mouse, ps2};

pub const EVENT_SIZE: usize = 32;
pub const KIND_KEY: u32 = 1;
pub const KIND_MOUSE: u32 = 2;
pub const KIND_LOST: u32 = 3;
pub const KIND_POSITION: u32 = 4;
const BUTTONS: u8 = mouse::LEFT | mouse::RIGHT | mouse::MIDDLE;

/// Set by a PS/2 IRQ that completed an input record and cleared by the
/// scheduler before it rechecks device-input waits (spec 0030a, 0030-D2).
/// It needs no session pointer: the IRQ works during any slice and between
/// sessions, and touches no scheduler state beyond these atomics.
static ARRIVED: AtomicBool = AtomicBool::new(false);
/// Threads blocked on device input, mirrored by the scheduler. A CPL3 input
/// IRQ reschedules only when this is nonzero.
static WAITERS: AtomicUsize = AtomicUsize::new(0);

/// Record that a device produced an input record.
pub(crate) fn note_arrival() {
    ARRIVED.store(true, Ordering::Release);
}

/// Whether input arrived since the last `take_arrival`.
pub(crate) fn arrival_pending() -> bool {
    ARRIVED.load(Ordering::Acquire)
}

/// Clear the arrival flag, returning whether it was set.
pub(crate) fn take_arrival() -> bool {
    ARRIVED.swap(false, Ordering::AcqRel)
}

/// The scheduler's count of threads blocked on device input.
pub(crate) fn set_waiters(count: usize) {
    WAITERS.store(count, Ordering::Release);
}

pub(crate) fn waiters() -> usize {
    WAITERS.load(Ordering::Acquire)
}

/// Where kind 4 positions land (spec 0017-D14, D17): absolute device units
/// span the framebuffer, and records carry pixels local to the desktop
/// rectangle inside it, clamped to that rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placement {
    last_x: usize,
    last_y: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Placement {
    /// Fails closed unless the desktop lies inside the framebuffer and its
    /// local coordinates fit the record's u32 words.
    fn new(framebuffer: (usize, usize), desktop: SurfaceRegion) -> Self {
        let (x, y) = desktop.origin();
        let (width, height) = desktop.dimensions();
        assert!(
            width != 0
                && height != 0
                && u32::try_from(width).is_ok()
                && u32::try_from(height).is_ok()
                && x.checked_add(width).is_some_and(|end| end <= framebuffer.0)
                && y.checked_add(height)
                    .is_some_and(|end| end <= framebuffer.1),
            "desktop outside the framebuffer"
        );
        Self {
            last_x: framebuffer.0 - 1,
            last_y: framebuffer.1 - 1,
            x,
            y,
            width,
            height,
        }
    }
}

pub struct Session<'a> {
    consumer: &'a mut ps2::Consumer,
    placement: Placement,
    pending: Option<[u8; EVENT_SIZE]>,
}

impl<'a> Session<'a> {
    /// `framebuffer` is the framebuffer's width and height and `desktop` the
    /// surface rectangle inside it, from `surface::Session::desktop`.
    pub fn new(
        consumer: &'a mut ps2::Consumer,
        framebuffer: (usize, usize),
        desktop: SurfaceRegion,
    ) -> Self {
        let placement = Placement::new(framebuffer, desktop);
        // Discard the backlog left by boot: up to 64 events per device, each
        // behind one pending loss marker. Bound ownership handoff work even if
        // deferred controller polling produces more events. Do not force
        // decoder break-recovery here: the first fresh key must remain usable.
        // Lost resets the client's state.
        for _ in 0..ps2::MAX_BACKLOG {
            if consumer.next_event().is_none() {
                break;
            }
        }
        Self {
            consumer,
            placement,
            // The initial record alone names both devices (spec 0017a-D4).
            pending: Some(encode(
                ps2::InputEvent::Lost {
                    keyboard: true,
                    pointer: true,
                },
                placement,
            )),
        }
    }

    /// Return an owned snapshot. Until consume(), no additional event is taken
    /// from the controller and every retry observes these exact same bytes.
    pub fn peek(&mut self) -> Option<[u8; EVENT_SIZE]> {
        if self.pending.is_none() {
            let placement = self.placement;
            self.pending = self
                .consumer
                .next_event()
                .map(|event| encode(event, placement));
        }
        self.pending
    }

    /// The caller must have copied the complete peeked record successfully.
    /// Invalid user buffers must return before invoking this commit operation.
    pub fn consume(&mut self) {
        assert!(
            self.pending.take().is_some(),
            "consume without a pending input record"
        );
    }
}

fn encode(event: ps2::InputEvent, placement: Placement) -> [u8; EVENT_SIZE] {
    // Eight LE u32s: version, kind, flags, key kind, unshifted character,
    // printable ASCII (zero if none), signed dx, signed dy. Unused words are zero.
    // A loss record's flags are a nonzero device mask: bit 0 keyboard, bit 1
    // pointer (spec 0017a-D4). A position record (spec 0017-D17) carries
    // button levels and unsigned surface-local x and y in the last two words.
    let mut words = [0u32; 8];
    words[0] = 1;
    match event {
        ps2::InputEvent::Key(event) => {
            words[1] = KIND_KEY;
            words[2] = u32::from(event.pressed)
                | u32::from(event.repeat) << 1
                | u32::from(event.modifiers.shift) << 2
                | u32::from(event.modifiers.control) << 3
                | u32::from(event.modifiers.alt) << 4
                | u32::from(event.modifiers.caps_lock) << 5;
            words[3] = match event.key {
                Key::Character(character) => {
                    words[4] = u32::from(character);
                    1
                }
                Key::Enter => 2,
                Key::Backspace => 3,
                Key::Tab => 4,
                Key::Escape => 5,
                Key::Left => 6,
                Key::Right => 7,
                Key::Home => 8,
                Key::End => 9,
                Key::Delete => 10,
                Key::Shift => 11,
                Key::Control => 12,
                Key::Alt => 13,
                Key::CapsLock => 14,
            };
            words[5] = u32::from(event.ascii().unwrap_or(0));
        }
        ps2::InputEvent::Mouse(event) => {
            words[1] = KIND_MOUSE;
            words[2] = u32::from(event.buttons & BUTTONS);
            words[6] = i32::from(event.dx) as u32;
            words[7] = i32::from(event.dy) as u32;
        }
        ps2::InputEvent::Position(event) => {
            // Device units to framebuffer pixels, then to the desktop's
            // centered origin; positions in the border clamp to its edge.
            let local = |value, last, origin: usize, extent: usize| {
                let pixel: usize = mouse::to_pixel(value, last);
                pixel.saturating_sub(origin).min(extent - 1) as u32
            };
            words[1] = KIND_POSITION;
            words[2] = u32::from(event.buttons & BUTTONS);
            words[6] = local(event.x, placement.last_x, placement.x, placement.width);
            words[7] = local(event.y, placement.last_y, placement.y, placement.height);
        }
        ps2::InputEvent::Lost { keyboard, pointer } => {
            assert!(keyboard || pointer, "loss record without a device");
            words[1] = KIND_LOST;
            words[2] = u32::from(keyboard) | u32::from(pointer) << 1;
        }
    }
    let mut record = [0u8; EVENT_SIZE];
    for (destination, word) in record.as_chunks_mut::<4>().0.iter_mut().zip(words) {
        destination.copy_from_slice(&word.to_le_bytes());
    }
    record
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    use crate::keyboard::{KeyEvent, Modifiers};
    fn words(record: [u8; EVENT_SIZE]) -> [u32; 8] {
        let mut result = [0; 8];
        for (word, bytes) in result.iter_mut().zip(record.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*bytes);
        }
        result
    }
    fn key(key: Key) -> KeyEvent {
        KeyEvent {
            key,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        }
    }
    /// A 1280x800 desktop centered in a framebuffer, as surface::Session
    /// grants it.
    fn desktop(width: usize, height: usize) -> SurfaceRegion {
        let info = crate::framebuffer::Info {
            width,
            height,
            pitch: width * 4,
            bpp: 32,
            byte_len: width * 4 * height,
        };
        SurfaceRegion::new(info, (width - 1280) / 2, (height - 800) / 2, 1280, 800)
            .expect("test desktop")
    }
    const FRAMEBUFFER: (usize, usize) = (1920, 1080);
    let placement = Placement::new(FRAMEBUFFER, desktop(FRAMEBUFFER.0, FRAMEBUFFER.1));
    let encode = |event| self::encode(event, placement);
    // Keyboard, pointer and initial loss records carry their device mask.
    for (keyboard, pointer, mask) in [(true, false, 1), (false, true, 2), (true, true, 3)] {
        assert_eq!(
            words(encode(ps2::InputEvent::Lost { keyboard, pointer })),
            [1, 3, mask, 0, 0, 0, 0, 0]
        );
    }
    for (index, key_kind) in [
        Key::Character(b'a'),
        Key::Enter,
        Key::Backspace,
        Key::Tab,
        Key::Escape,
        Key::Left,
        Key::Right,
        Key::Home,
        Key::End,
        Key::Delete,
        Key::Shift,
        Key::Control,
        Key::Alt,
        Key::CapsLock,
    ]
    .into_iter()
    .enumerate()
    {
        let encoded = words(encode(ps2::InputEvent::Key(key(key_kind))));
        assert_eq!(&encoded[..4], &[1, 1, 1, index as u32 + 1]);
        assert_eq!(&encoded[6..], &[0, 0]);
        assert_eq!(&encoded[4..6], if index == 0 { &[97, 97] } else { &[0, 0] });
    }
    let mut event = key(Key::Character(b'a'));
    event.modifiers.shift = true;
    event.repeat = true;
    assert_eq!(
        words(encode(ps2::InputEvent::Key(event))),
        [1, 1, 7, 1, 97, 65, 0, 0]
    );
    event.modifiers.caps_lock = true;
    assert_eq!(words(encode(ps2::InputEvent::Key(event)))[5], 97);
    event.modifiers.control = true;
    event.modifiers.alt = true;
    assert_eq!(
        words(encode(ps2::InputEvent::Key(event))),
        [1, 1, 63, 1, 97, 0, 0, 0]
    );
    event.pressed = false;
    event.repeat = false;
    assert_eq!(
        words(encode(ps2::InputEvent::Key(event))),
        [1, 1, 60, 1, 97, 0, 0, 0]
    );
    let motion = encode(ps2::InputEvent::Mouse(crate::mouse::MouseEvent {
        dx: -256,
        dy: 256,
        buttons: 0xff,
    }));
    assert_eq!(words(motion), [1, 2, 7, 0, 0, 0, (-256i32) as u32, 256]);
    assert_eq!(&motion[24..28], &[0, 0xff, 0xff, 0xff]);

    // Position records (spec 0017-D17): with the 1280x800 desktop centered in
    // 1920x1080 (origin 320,140), device unit 0 is local (0, 0) and 0xFFFF is
    // (1279, 799); framebuffer pixels in the border clamp to the edge.
    let position = |x, y, buttons| {
        words(encode(ps2::InputEvent::Position(mouse::PositionEvent {
            x,
            y,
            buttons,
        })))
    };
    assert_eq!(position(0, 0, 0), [1, 4, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        position(0xffff, 0xffff, 0xff),
        [1, 4, 7, 0, 0, 0, 1279, 799]
    );
    for buttons in [mouse::LEFT, mouse::RIGHT, mouse::MIDDLE] {
        assert_eq!(position(0, 0, buttons)[2], u32::from(buttons));
    }
    // The device unit that maps to framebuffer pixel `pixel` of `extent`.
    let unit = |pixel: u64, extent: u64| {
        u16::try_from((2 * pixel * 0xffff + extent - 1) / (2 * (extent - 1))).expect("unit")
    };
    for (pixel, local) in [
        (0, 0),
        (319, 0),
        (320, 0),
        (321, 1),
        (960, 640),
        (1599, 1279),
        (1600, 1279),
        (1919, 1279),
    ] {
        assert_eq!(position(unit(pixel, 1920), 0, 0)[6..], [local, 0]);
    }
    for (pixel, local) in [
        (0, 0),
        (139, 0),
        (140, 0),
        (141, 1),
        (540, 400),
        (939, 799),
        (940, 799),
        (1079, 799),
    ] {
        assert_eq!(position(0, unit(pixel, 1080), 0)[6..], [0, local]);
    }
    // A framebuffer the desktop fills has no border to subtract.
    let filled = Placement::new((1280, 800), desktop(1280, 800));
    let corner = self::encode(
        ps2::InputEvent::Position(mouse::PositionEvent {
            x: 0xffff,
            y: 0xffff,
            buttons: 0,
        }),
        filled,
    );
    assert_eq!(words(corner), [1, 4, 0, 0, 0, 0, 1279, 799]);

    // Fixtures activate only software decoders. Controller state is still
    // uninitialized with no pending drain, so these paths perform no port I/O.
    crate::keyboard::with_test_input(|| {
        crate::mouse::with_test_input(|| {
            let mut consumer = ps2::Consumer::claim().expect("first input consumer");
            assert!(
                ps2::Consumer::claim().is_none(),
                "input lease was duplicated"
            );
            crate::keyboard::accept_byte(1, 0x1c); // Old 'a' is discarded at handoff.
            crate::keyboard::accept_byte(1, 0xf0);
            crate::keyboard::accept_byte(1, 0x1c);
            {
                let mut session = Session::new(&mut consumer, FRAMEBUFFER, desktop(1920, 1080));
                let initial = session.peek().unwrap();
                assert_eq!(words(initial), [1, KIND_LOST, 3, 0, 0, 0, 0, 0]);
                assert_eq!(session.peek(), Some(initial));
                session.consume();
                assert_eq!(session.peek(), None);
                // A fresh make works immediately: new() did not force a break.
                crate::keyboard::accept_byte(1, 0x32); // b
                for byte in [0x09, 3, 0] {
                    crate::mouse::accept_byte(0x21, byte);
                }
                crate::keyboard::accept_byte(1, 0xf0);
                crate::keyboard::accept_byte(1, 0x32);
                let first = session.peek().unwrap();
                assert_eq!(words(first), [1, 1, 1, 1, 98, 98, 0, 0]);
                // Simulate failed copies by retaining the staged record. Later
                // input/loss cannot replace it until the successful commit.
                assert_eq!(session.peek(), Some(first));
                assert_eq!(session.peek(), Some(first));
                session.consume();
                assert_eq!(words(session.peek().unwrap()), [1, 2, 1, 0, 0, 0, 3, 0]);
                session.consume();
                assert_eq!(words(session.peek().unwrap()), [1, 1, 0, 1, 98, 0, 0, 0]);
                session.consume();
                assert_eq!(session.peek(), None);

                crate::keyboard::accept_byte(1, 0x21); // c staged before loss
                let retained = session.peek().unwrap();
                crate::keyboard::accept_byte(1, 0); // Corrupt stream -> keyboard loss
                assert_eq!(session.peek(), Some(retained));
                session.consume();
                let loss = session.peek().unwrap();
                assert_eq!(words(loss), [1, 3, 1, 0, 0, 0, 0, 0]);
                assert_eq!(session.peek(), Some(loss));
                session.consume();
                assert_eq!(session.peek(), None);
                crate::keyboard::accept_byte(1, 0xf0);
                crate::keyboard::accept_byte(1, 0x21); // Recovery release is consumed.
                crate::keyboard::accept_byte(1, 0x23); // Fresh d
                assert_eq!(words(session.peek().unwrap())[5], 100);
                session.consume();

                // A pointer loss is its own record and keeps the key that
                // was queued before it (spec 0017a-D3).
                crate::keyboard::accept_byte(1, 0x24); // e queued before loss
                crate::mouse::accept_byte(0x21 | 0xc0, 0x08); // Corrupt AUX header
                for byte in [0x09, 0x09, 0x08, 4, 0] {
                    crate::mouse::accept_byte(0x21, byte);
                }
                assert_eq!(words(session.peek().unwrap())[5], 101);
                session.consume();
                assert_eq!(words(session.peek().unwrap()), [1, 3, 2, 0, 0, 0, 0, 0]);
                session.consume();
                assert_eq!(words(session.peek().unwrap()), [1, 2, 0, 0, 0, 0, 4, 0]);
                session.consume();
                assert_eq!(session.peek(), None);

                // A drained position reaches the session in order with keys,
                // as a kind 4 record in surface-local pixels (spec 0017-D17).
                crate::keyboard::accept_byte(1, 0x2b); // f
                mouse::publish_test_event(mouse::Event::Absolute(mouse::PositionEvent {
                    x: 0xffff,
                    y: 0,
                    buttons: mouse::LEFT,
                }));
                crate::keyboard::accept_byte(1, 0xf0);
                crate::keyboard::accept_byte(1, 0x2b);
                assert_eq!(words(session.peek().unwrap())[5], 102);
                session.consume();
                assert_eq!(words(session.peek().unwrap()), [1, 4, 1, 0, 0, 0, 1279, 0]);
                session.consume();
                assert_eq!(words(session.peek().unwrap()), [1, 1, 0, 1, 102, 0, 0, 0]);
                session.consume();
                assert_eq!(session.peek(), None);
            }
            assert_eq!(consumer.next_event(), None);

            // The worst-case handoff backlog: each device holds one loss
            // marker plus a full ring of events queued after it (0017a-D3).
            fn fill_backlog() {
                crate::keyboard::accept_byte(1, 0); // Keyboard loss
                crate::keyboard::accept_byte(1, 0xf0);
                crate::keyboard::accept_byte(1, 0x1c); // Recovery release
                for _ in 0..crate::keyboard::QUEUE_CAPACITY {
                    crate::keyboard::accept_byte(1, 0x1c); // a, then repeats
                }
                // A corrupt AUX header is a pointer loss. Its two-byte tail is
                // discarded before the packets that refill the ring.
                for (status, byte) in [(0x21 | 0xc0, 0x08), (0x21, 0), (0x21, 0)] {
                    crate::mouse::accept_byte(status, byte);
                }
                for _ in 0..crate::mouse::QUEUE_CAPACITY {
                    for byte in [0x08, 1, 0] {
                        crate::mouse::accept_byte(0x21, byte);
                    }
                }
            }
            fill_backlog();
            let backlog = (0..=ps2::MAX_BACKLOG)
                .take_while(|_| consumer.next_event().is_some())
                .count();
            assert_eq!(backlog, ps2::MAX_BACKLOG);
            fill_backlog();
            {
                let mut session = Session::new(&mut consumer, FRAMEBUFFER, desktop(1920, 1080));
                let initial = session.peek().unwrap();
                assert_eq!(words(initial), [1, KIND_LOST, 3, 0, 0, 0, 0, 0]);
                session.consume();
                assert_eq!(session.peek(), None, "boot backlog survived handoff");
            }
            assert_eq!(consumer.next_event(), None);
            drop(consumer);
            let _next = ps2::Consumer::claim().expect("released input lease was not reusable");
        })
    });
    crate::serial::println!("MIRROR_INPUT_TESTS_OK");
}
