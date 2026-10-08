//! Two allocation-free channel queues. The syscall layer resolves capabilities,
//! validates/copies user buffers, and handles blocking; queues contain no user
//! pointers. Endpoint references include every duplicate with send/receive
//! authority; inert capabilities have neither endpoint reference.

use crate::capability::{Capability, Object, RECEIVE, SEND};

pub(crate) const MESSAGE_SIZE: usize = 64;
pub(crate) const QUEUE_CAPACITY: usize = 4;
const CHANNEL_COUNT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Message {
    pub bytes: [u8; MESSAGE_SIZE],
    pub length: usize,
}

impl Message {
    const EMPTY: Self = Self {
        bytes: [0; MESSAGE_SIZE],
        length: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IpcError {
    WouldBlock,
    Closed,
    MessageSize,
}

struct Channel {
    messages: [Message; QUEUE_CAPACITY],
    head: usize,
    length: usize,
    senders: usize,
    receivers: usize,
}

impl Channel {
    const fn new() -> Self {
        Self {
            messages: [Message::EMPTY; QUEUE_CAPACITY],
            head: 0,
            length: 0,
            senders: 0,
            receivers: 0,
        }
    }
}

pub(crate) struct Ipc {
    channels: [Channel; CHANNEL_COUNT],
}

impl Ipc {
    pub(crate) const fn new() -> Self {
        Self {
            channels: [Channel::new(), Channel::new()],
        }
    }

    pub(crate) fn grant(&mut self, capability: Capability) {
        let Object::Channel(index) = capability.object else {
            return;
        };
        assert!(capability.rights & !(SEND | RECEIVE) == 0);
        let channel = self.channel(index);
        if capability.rights & SEND != 0 {
            channel.senders = channel
                .senders
                .checked_add(1)
                .expect("sender reference overflow");
        }
        if capability.rights & RECEIVE != 0 {
            channel.receivers = channel
                .receivers
                .checked_add(1)
                .expect("receiver reference overflow");
        }
    }

    pub(crate) fn release(&mut self, capability: Capability) {
        let Object::Channel(index) = capability.object else {
            return;
        };
        assert!(capability.rights & !(SEND | RECEIVE) == 0);
        let channel = self.channel(index);
        if capability.rights & SEND != 0 {
            channel.senders = channel
                .senders
                .checked_sub(1)
                .expect("sender reference underflow");
        }
        if capability.rights & RECEIVE != 0 {
            channel.receivers = channel
                .receivers
                .checked_sub(1)
                .expect("receiver reference underflow");
            if channel.receivers == 0 {
                // Nobody can consume the queued data. Discard and scrub it now.
                channel.messages = [Message::EMPTY; QUEUE_CAPACITY];
                channel.head = 0;
                channel.length = 0;
            }
        }
    }

    pub(crate) fn send(&mut self, index: usize, bytes: &[u8]) -> Result<usize, IpcError> {
        let channel = self.channel(index);
        if bytes.is_empty() || bytes.len() > MESSAGE_SIZE {
            return Err(IpcError::MessageSize);
        }
        if channel.receivers == 0 {
            return Err(IpcError::Closed);
        }
        if channel.length == QUEUE_CAPACITY {
            return Err(IpcError::WouldBlock);
        }
        let mut message = Message::EMPTY;
        message.bytes[..bytes.len()].copy_from_slice(bytes);
        message.length = bytes.len();
        let tail = (channel.head + channel.length) % QUEUE_CAPACITY;
        channel.messages[tail] = message;
        channel.length += 1;
        Ok(bytes.len())
    }

    /// Observe the oldest complete message without consuming it. A failed user
    /// copy must leave the same message available for a subsequent receive.
    pub(crate) fn peek(&self, index: usize) -> Result<Message, IpcError> {
        let channel = self.channels.get(index).expect("invalid channel index");
        if channel.length != 0 {
            return Ok(channel.messages[channel.head]);
        }
        if channel.senders == 0 {
            Err(IpcError::Closed)
        } else {
            Err(IpcError::WouldBlock)
        }
    }

    /// Call only after peek and a successful complete user copy, while holding
    /// exclusive IPC access. This must not run on copy failure or short buffers.
    pub(crate) fn consume(&mut self, index: usize) {
        let channel = self.channel(index);
        assert_ne!(channel.length, 0, "consuming an empty channel");
        channel.messages[channel.head] = Message::EMPTY;
        channel.head = (channel.head + 1) % QUEUE_CAPACITY;
        channel.length -= 1;
    }

    pub(crate) fn is_drained(&self) -> bool {
        self.channels
            .iter()
            .all(|channel| channel.senders == 0 && channel.receivers == 0 && channel.length == 0)
    }

    fn channel(&mut self, index: usize) -> &mut Channel {
        self.channels.get_mut(index).expect("invalid channel index")
    }
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn self_test() {
    let mut ipc = Ipc::new();
    let send = Capability {
        object: Object::Channel(0),
        rights: SEND,
    };
    let receive = Capability {
        object: Object::Channel(0),
        rights: RECEIVE,
    };
    let both = Capability {
        object: Object::Channel(1),
        rights: SEND | RECEIVE,
    };
    let console = Capability {
        object: Object::Console,
        rights: crate::capability::WRITE,
    };
    assert!(ipc.is_drained());
    let inert = Capability {
        object: Object::Channel(0),
        rights: 0,
    };
    ipc.grant(inert);
    ipc.release(inert);
    assert!(ipc.is_drained(), "inert capabilities retained an endpoint");
    ipc.grant(console);
    ipc.release(console);
    assert!(ipc.is_drained());
    assert_eq!(ipc.peek(0), Err(IpcError::Closed));
    assert_eq!(ipc.send(0, b"x"), Err(IpcError::Closed));
    ipc.grant(send);
    ipc.grant(receive);
    assert!(!ipc.is_drained());
    assert_eq!(ipc.peek(0), Err(IpcError::WouldBlock));
    assert_eq!(ipc.send(0, &[]), Err(IpcError::MessageSize));
    assert_eq!(
        ipc.send(0, &[0; MESSAGE_SIZE + 1]),
        Err(IpcError::MessageSize)
    );
    for value in 0..QUEUE_CAPACITY as u8 {
        assert_eq!(ipc.send(0, &[value; MESSAGE_SIZE]), Ok(MESSAGE_SIZE));
    }
    assert_eq!(ipc.send(0, b"full"), Err(IpcError::WouldBlock));
    for value in 0..2_u8 {
        let message = ipc.peek(0).unwrap();
        assert_eq!(message.length, MESSAGE_SIZE);
        assert_eq!(message.bytes, [value; MESSAGE_SIZE]);
        assert_eq!(ipc.peek(0), Ok(message), "peek consumed a message");
        ipc.consume(0);
    }
    // Fill the freed slots to force the FIFO tail to wrap around the ring.
    assert_eq!(ipc.send(0, &[4]), Ok(1));
    assert_eq!(ipc.send(0, &[5]), Ok(1));
    for value in 2..6_u8 {
        let message = ipc.peek(0).unwrap();
        assert_eq!(message.bytes[0], value);
        if value >= 4 {
            assert_eq!(message.length, 1);
            assert!(message.bytes[1..].iter().all(|byte| *byte == 0));
        }
        ipc.consume(0);
    }
    assert_eq!(ipc.peek(0), Err(IpcError::WouldBlock));
    ipc.grant(send); // A duplicate keeps the endpoint open after one close.
    ipc.release(send);
    assert_eq!(ipc.peek(0), Err(IpcError::WouldBlock));
    ipc.send(0, b"queued before last sender closes").unwrap();
    ipc.release(send);
    assert!(ipc.peek(0).is_ok(), "sender close discarded queued data");
    ipc.consume(0);
    assert_eq!(ipc.peek(0), Err(IpcError::Closed));
    ipc.release(receive);
    assert!(ipc.is_drained());

    ipc.grant(both);
    ipc.grant(both);
    ipc.send(1, b"preserved until last receiver closes")
        .unwrap();
    ipc.release(both);
    assert!(ipc.peek(1).is_ok());
    ipc.release(both);
    assert_eq!(ipc.peek(1), Err(IpcError::Closed));
    assert!(ipc.is_drained());

    ipc.grant(send);
    ipc.grant(receive);
    ipc.grant(receive);
    ipc.send(0, b"discard on last receiver close").unwrap();
    ipc.release(receive);
    assert!(ipc.peek(0).is_ok());
    ipc.release(receive);
    assert_eq!(ipc.send(0, b"closed"), Err(IpcError::Closed));
    assert_eq!(ipc.peek(0), Err(IpcError::WouldBlock));
    assert!(
        ipc.channels[0]
            .messages
            .iter()
            .all(|message| *message == Message::EMPTY)
    );
    ipc.release(send);
    assert!(ipc.is_drained());
    crate::serial::println!("MIRROR_IPC_TESTS_OK");
}
