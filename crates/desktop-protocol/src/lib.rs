#![no_std]

//! Native scratchpad operations and their bounded, canonical IPC encoding.
//!
//! Requests and responses are exactly 64 bytes. Integers are little endian.
//! Version/opcode occupy bytes 0/1, correlation ID 4..8 and revision 8..16.
//! Request parameters occupy 16..19; printable text occupies 32..64. Replies
//! set opcode bit 7 and carry status at 2, has-applied at 3, draft/applied/caret
//! at 16/17/18, advertised capacity/chunk size at 19/20, read offset/count at
//! 21/22 and supported-operation bits at 23. Every unused byte must be zero.
//!
//! Capabilities grant transport authority; this protocol never grants rights.
//! Agents compare revisions before mutation. Human commands use the same
//! dispatcher without an expected revision. Reads report their revision so a
//! client can detect changes between chunks instead of joining mixed snapshots.

pub const MESSAGE_BYTES: usize = 64;
pub const CAPACITY: usize = 96;
pub const CHUNK_BYTES: usize = 32;
pub const VERSION: u8 = 1;
pub const SUPPORTED_OPERATIONS: u8 = 0x3f;
pub const CONTROL_RUN: [u8; 8] = [b'M', b'A', b'G', b'T', 1, 1, 0, 0];
pub const CONTROL_COMPLETE: [u8; 8] = [b'M', b'A', b'G', b'T', 1, 2, 0, 0];
pub const CONTROL_CONFLICT: [u8; 8] = [b'M', b'A', b'G', b'T', 1, 2, 1, 0];
pub const CONTROL_FAILED: [u8; 8] = [b'M', b'A', b'G', b'T', 1, 2, 2, 0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    Invalid = 0,
    Discover = 1,
    Inspect = 2,
    ReadDraft = 3,
    ReadApplied = 4,
    Replace = 5,
    Apply = 6,
}

impl Opcode {
    fn decode(value: u8) -> Result<Self, WireError> {
        match value {
            0 => Ok(Self::Invalid),
            1 => Ok(Self::Discover),
            2 => Ok(Self::Inspect),
            3 => Ok(Self::ReadDraft),
            4 => Ok(Self::ReadApplied),
            5 => Ok(Self::Replace),
            6 => Ok(Self::Apply),
            _ => Err(WireError::Opcode),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    Malformed = 1,
    StaleRevision = 2,
    InvalidRange = 3,
    InvalidText = 4,
    CapacityExceeded = 5,
    RevisionExhausted = 6,
    Denied = 7,
}

impl Status {
    fn decode(value: u8) -> Result<Self, WireError> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::Malformed),
            2 => Ok(Self::StaleRevision),
            3 => Ok(Self::InvalidRange),
            4 => Ok(Self::InvalidText),
            5 => Ok(Self::CapacityExceeded),
            6 => Ok(Self::RevisionExhausted),
            7 => Ok(Self::Denied),
            _ => Err(WireError::Status),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    Length,
    Version,
    Opcode,
    Status,
    Reserved,
    Range,
    Text,
}

fn printable(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| (b' '..=b'~').contains(byte))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    bytes: [u8; CHUNK_BYTES],
    length: u8,
}

impl Chunk {
    pub fn new(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() > CHUNK_BYTES {
            return Err(WireError::Length);
        }
        if !printable(bytes) {
            return Err(WireError::Text);
        }
        let mut chunk = Self {
            bytes: [0; CHUNK_BYTES],
            length: bytes.len() as u8,
        };
        chunk.bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(chunk)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.length)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextTarget {
    Draft,
    Applied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Discover,
    Inspect,
    Read {
        target: TextTarget,
        offset: u8,
        length: u8,
    },
    Replace {
        expected_revision: u64,
        start: u8,
        end: u8,
        text: Chunk,
    },
    Apply {
        expected_revision: u64,
    },
}

impl Command {
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::Discover => Opcode::Discover,
            Self::Inspect => Opcode::Inspect,
            Self::Read {
                target: TextTarget::Draft,
                ..
            } => Opcode::ReadDraft,
            Self::Read {
                target: TextTarget::Applied,
                ..
            } => Opcode::ReadApplied,
            Self::Replace { .. } => Opcode::Replace,
            Self::Apply { .. } => Opcode::Apply,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: u32,
    pub command: Command,
}

impl Request {
    pub fn encode(&self) -> Result<[u8; MESSAGE_BYTES], WireError> {
        let mut bytes = [0; MESSAGE_BYTES];
        bytes[0] = VERSION;
        bytes[1] = self.command.opcode() as u8;
        bytes[4..8].copy_from_slice(&self.id.to_le_bytes());
        match self.command {
            Command::Discover | Command::Inspect => {}
            Command::Read { offset, length, .. } => {
                if usize::from(offset) > CAPACITY || !(1..=CHUNK_BYTES as u8).contains(&length) {
                    return Err(WireError::Range);
                }
                bytes[16] = offset;
                bytes[17] = length;
            }
            Command::Replace {
                expected_revision,
                start,
                end,
                text,
            } => {
                if start > end || usize::from(end) > CAPACITY {
                    return Err(WireError::Range);
                }
                bytes[8..16].copy_from_slice(&expected_revision.to_le_bytes());
                bytes[16] = start;
                bytes[17] = end;
                bytes[18] = text.length;
                bytes[32..32 + usize::from(text.length)].copy_from_slice(text.as_bytes());
            }
            Command::Apply { expected_revision } => {
                bytes[8..16].copy_from_slice(&expected_revision.to_le_bytes());
            }
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() != MESSAGE_BYTES {
            return Err(WireError::Length);
        }
        if bytes[0] != VERSION {
            return Err(WireError::Version);
        }
        let opcode = Opcode::decode(bytes[1])?;
        let revision = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let command = match opcode {
            Opcode::Invalid => return Err(WireError::Opcode),
            Opcode::Discover => Command::Discover,
            Opcode::Inspect => Command::Inspect,
            Opcode::ReadDraft | Opcode::ReadApplied => Command::Read {
                target: if opcode == Opcode::ReadDraft {
                    TextTarget::Draft
                } else {
                    TextTarget::Applied
                },
                offset: bytes[16],
                length: bytes[17],
            },
            Opcode::Replace => {
                let length = usize::from(bytes[18]);
                if length > CHUNK_BYTES {
                    return Err(WireError::Length);
                }
                Command::Replace {
                    expected_revision: revision,
                    start: bytes[16],
                    end: bytes[17],
                    text: Chunk::new(&bytes[32..32 + length])?,
                }
            }
            Opcode::Apply => Command::Apply {
                expected_revision: revision,
            },
        };
        let request = Self {
            id: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            command,
        };
        // Re-encoding also rejects nonzero reserved fields and text padding.
        if request.encode()? != bytes {
            return Err(WireError::Reserved);
        }
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub revision: u64,
    pub draft_length: u8,
    pub applied_length: u8,
    pub caret: u8,
    pub has_applied: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadResult {
    pub offset: u8,
    pub text: Chunk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response {
    pub id: u32,
    pub opcode: Opcode,
    pub status: Status,
    pub snapshot: Snapshot,
    pub read: Option<ReadResult>,
}

impl Response {
    pub fn malformed(id: u32, snapshot: Snapshot) -> Self {
        Self {
            id,
            opcode: Opcode::Invalid,
            status: Status::Malformed,
            snapshot,
            read: None,
        }
    }

    pub fn encode(&self) -> Result<[u8; MESSAGE_BYTES], WireError> {
        let snapshot = self.snapshot;
        if usize::from(snapshot.draft_length) > CAPACITY
            || usize::from(snapshot.applied_length) > CAPACITY
            || snapshot.caret > snapshot.draft_length
            || (!snapshot.has_applied && snapshot.applied_length != 0)
        {
            return Err(WireError::Range);
        }
        if (self.opcode == Opcode::Invalid) != (self.status == Status::Malformed) {
            return Err(WireError::Opcode);
        }
        let is_read = matches!(self.opcode, Opcode::ReadDraft | Opcode::ReadApplied)
            && self.status == Status::Ok;
        if self.read.is_some() != is_read {
            return Err(WireError::Range);
        }
        let mut bytes = [0; MESSAGE_BYTES];
        bytes[0] = VERSION;
        bytes[1] = self.opcode as u8 | 0x80;
        bytes[2] = self.status as u8;
        bytes[3] = u8::from(snapshot.has_applied);
        bytes[4..8].copy_from_slice(&self.id.to_le_bytes());
        bytes[8..16].copy_from_slice(&snapshot.revision.to_le_bytes());
        bytes[16] = snapshot.draft_length;
        bytes[17] = snapshot.applied_length;
        bytes[18] = snapshot.caret;
        bytes[19] = CAPACITY as u8;
        bytes[20] = CHUNK_BYTES as u8;
        bytes[23] = SUPPORTED_OPERATIONS;
        if let Some(read) = self.read {
            let limit = if self.opcode == Opcode::ReadDraft {
                snapshot.draft_length
            } else {
                snapshot.applied_length
            };
            if usize::from(read.offset) + usize::from(read.text.length) > usize::from(limit) {
                return Err(WireError::Range);
            }
            bytes[21] = read.offset;
            bytes[22] = read.text.length;
            bytes[32..32 + usize::from(read.text.length)].copy_from_slice(read.text.as_bytes());
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() != MESSAGE_BYTES {
            return Err(WireError::Length);
        }
        if bytes[0] != VERSION {
            return Err(WireError::Version);
        }
        if bytes[1] & 0x80 == 0 {
            return Err(WireError::Opcode);
        }
        let opcode = Opcode::decode(bytes[1] & 0x7f)?;
        let status = Status::decode(bytes[2])?;
        let read =
            if matches!(opcode, Opcode::ReadDraft | Opcode::ReadApplied) && status == Status::Ok {
                let length = usize::from(bytes[22]);
                if length > CHUNK_BYTES {
                    return Err(WireError::Length);
                }
                Some(ReadResult {
                    offset: bytes[21],
                    text: Chunk::new(&bytes[32..32 + length])?,
                })
            } else {
                None
            };
        let response = Self {
            id: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            opcode,
            status,
            snapshot: Snapshot {
                revision: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
                draft_length: bytes[16],
                applied_length: bytes[17],
                caret: bytes[18],
                has_applied: bytes[3] != 0,
            },
            read,
        };
        if response.encode()? != bytes {
            return Err(WireError::Reserved);
        }
        Ok(response)
    }
}

/// The single semantic mutation path for human input and agent IPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation<'a> {
    Replace {
        start: usize,
        end: usize,
        text: &'a [u8],
    },
    SetCaret(usize),
    Apply,
    ClearDraft,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub edited: bool,
    pub applied: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    draft: [u8; CAPACITY],
    draft_length: usize,
    applied: [u8; CAPACITY],
    applied_length: usize,
    caret: usize,
    has_applied: bool,
    revision: u64,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub const fn new() -> Self {
        Self {
            draft: [0; CAPACITY],
            draft_length: 0,
            applied: [0; CAPACITY],
            applied_length: 0,
            caret: 0,
            has_applied: false,
            revision: 0,
        }
    }

    pub fn draft(&self) -> &[u8] {
        &self.draft[..self.draft_length]
    }
    pub fn applied(&self) -> &[u8] {
        &self.applied[..self.applied_length]
    }
    pub fn caret(&self) -> usize {
        self.caret
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            revision: self.revision,
            draft_length: self.draft_length as u8,
            applied_length: self.applied_length as u8,
            caret: self.caret as u8,
            has_applied: self.has_applied,
        }
    }

    /// Validate completely before changing state. Revisions never wrap. Apply
    /// always advances the revision because submission itself is observable.
    pub fn dispatch(
        &mut self,
        expected_revision: Option<u64>,
        operation: Operation<'_>,
    ) -> Result<Change, Status> {
        if expected_revision.is_some_and(|revision| revision != self.revision) {
            return Err(Status::StaleRevision);
        }
        let change = match operation {
            Operation::Replace { start, end, text } => {
                if start > end || end > self.draft_length {
                    return Err(Status::InvalidRange);
                }
                if !printable(text) {
                    return Err(Status::InvalidText);
                }
                if text.len() > CAPACITY - (self.draft_length - (end - start)) {
                    return Err(Status::CapacityExceeded);
                }
                Change {
                    edited: self.draft[start..end] != *text || self.caret != start + text.len(),
                    applied: false,
                }
            }
            Operation::SetCaret(position) => {
                if position > self.draft_length {
                    return Err(Status::InvalidRange);
                }
                Change {
                    edited: position != self.caret,
                    applied: false,
                }
            }
            Operation::Apply => Change {
                edited: false,
                applied: true,
            },
            Operation::ClearDraft => Change {
                edited: self.draft_length != 0 || self.caret != 0,
                applied: false,
            },
        };
        if !change.edited && !change.applied {
            return Ok(change);
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(Status::RevisionExhausted)?;
        match operation {
            Operation::Replace { start, end, text } => {
                self.draft
                    .copy_within(end..self.draft_length, start + text.len());
                self.draft[start..start + text.len()].copy_from_slice(text);
                self.draft_length = self.draft_length - (end - start) + text.len();
                self.draft[self.draft_length..].fill(0);
                self.caret = start + text.len();
            }
            Operation::SetCaret(position) => self.caret = position,
            Operation::Apply => {
                self.applied = self.draft;
                self.applied_length = self.draft_length;
                self.has_applied = true;
            }
            Operation::ClearDraft => {
                self.draft.fill(0);
                self.draft_length = 0;
                self.caret = 0;
            }
        }
        self.revision = revision;
        Ok(change)
    }

    pub fn handle_request(&mut self, request: Request) -> (Response, Change) {
        let mut change = Change::default();
        let mut response = Response {
            id: request.id,
            opcode: request.command.opcode(),
            status: Status::Ok,
            snapshot: self.snapshot(),
            read: None,
        };
        if request.encode().is_err() {
            return (Response::malformed(request.id, self.snapshot()), change);
        }
        let result = match request.command {
            Command::Discover | Command::Inspect => Ok(Change::default()),
            Command::Read {
                target,
                offset,
                length,
            } => {
                let source = match target {
                    TextTarget::Draft => self.draft(),
                    TextTarget::Applied => self.applied(),
                };
                let offset = usize::from(offset);
                if offset > source.len() {
                    Err(Status::InvalidRange)
                } else {
                    let end = (offset + usize::from(length)).min(source.len());
                    response.read = Some(ReadResult {
                        offset: offset as u8,
                        text: Chunk::new(&source[offset..end]).unwrap(),
                    });
                    Ok(Change::default())
                }
            }
            Command::Replace {
                expected_revision,
                start,
                end,
                text,
            } => self.dispatch(
                Some(expected_revision),
                Operation::Replace {
                    start: usize::from(start),
                    end: usize::from(end),
                    text: text.as_bytes(),
                },
            ),
            Command::Apply { expected_revision } => {
                self.dispatch(Some(expected_revision), Operation::Apply)
            }
        };
        match result {
            Ok(applied) => change = applied,
            Err(status) => response.status = status,
        }
        response.snapshot = self.snapshot();
        (response, change)
    }
}

#[cfg(test)]
mod tests;
