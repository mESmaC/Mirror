//! Versioned userspace input records over an exclusive completed-event lease.
//!
//! The session is owned by the scheduler, never by IRQ handlers. A pending
//! record is copied by value and committed only after successful user copyout;
//! failed requests cannot consume it or reorder later device events.

use crate::{keyboard::Key, ps2};

pub const EVENT_SIZE: usize = 32;
pub const KIND_KEY: u32 = 1;
pub const KIND_MOUSE: u32 = 2;
pub const KIND_LOST: u32 = 3;

pub struct Session<'a> {
    consumer: &'a mut ps2::Consumer,
    pending: Option<[u8; EVENT_SIZE]>,
}

impl<'a> Session<'a> {
    pub fn new(consumer: &'a mut ps2::Consumer) -> Self {
        // Bound ownership handoff work even if deferred controller polling
        // produces more events. Do not force decoder break-recovery here: the
        // first fresh key must remain usable. Lost resets the client's state.
        for _ in 0..128 {
            if consumer.next_event().is_none() {
                break;
            }
        }
        Self {
            consumer,
            // The initial record alone names both devices (spec 0017a-D4).
            pending: Some(encode(ps2::InputEvent::Lost {
                keyboard: true,
                pointer: true,
            })),
        }
    }

    /// Return an owned snapshot. Until consume(), no additional event is taken
    /// from the controller and every retry observes these exact same bytes.
    pub fn peek(&mut self) -> Option<[u8; EVENT_SIZE]> {
        if self.pending.is_none() {
            self.pending = self.consumer.next_event().map(encode);
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

fn encode(event: ps2::InputEvent) -> [u8; EVENT_SIZE] {
    // Eight LE u32s: version, kind, flags, key kind, unshifted character,
    // printable ASCII (zero if none), signed dx, signed dy. Unused words are zero.
    // A loss record's flags are a nonzero device mask: bit 0 keyboard, bit 1
    // pointer (spec 0017a-D4).
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
            words[2] = u32::from(
                event.buttons & (crate::mouse::LEFT | crate::mouse::RIGHT | crate::mouse::MIDDLE),
            );
            words[6] = i32::from(event.dx) as u32;
            words[7] = i32::from(event.dy) as u32;
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
                let mut session = Session::new(&mut consumer);
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
            }
            assert_eq!(consumer.next_event(), None);
            drop(consumer);
            let _next = ps2::Consumer::claim().expect("released input lease was not reusable");
        })
    });
    crate::serial::println!("MIRROR_INPUT_TESTS_OK");
}
