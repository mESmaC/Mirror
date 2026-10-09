#![no_std]
//! Fixed-size process, frame and routed-input contracts for the native desktop.
//! Geometry, decorations, focus and stacking remain userspace policy.
//!
//! All wire integers are little-endian. APP_LAUNCH returns a positive control
//! handle; APP_INFO and APP_READ return the copied byte count. APP_EVENT,
//! APP_CLOSE and SURFACE_COMMIT return zero on success. Negative returns are
//! errors. APP_READ requires the requested front-buffer revision and returns
//! -11 without modifying the caller's packet if that revision has changed.

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 192;
pub const TILE: usize = 16;
pub const FRAME_BYTES: usize = WIDTH * HEIGHT * 4;
pub const EVENT_SIZE: usize = 32;
pub const INFO_SIZE: usize = 40;
pub const READ_HEADER: usize = 24;
pub const MAX_READ_PACKET: usize = READ_HEADER + TILE * TILE * 4;
pub const MAX_APPS: usize = 2;
pub const NOTES: u64 = 0;
pub const COUNTER: u64 = 1;
pub const SYS_APP_LAUNCH: u64 = 17;
pub const SYS_APP_INFO: u64 = 18;
pub const SYS_APP_EVENT: u64 = 19;
pub const SYS_APP_READ: u64 = 20;
pub const SYS_APP_CLOSE: u64 = 21;
pub const SYS_SURFACE_COMMIT: u64 = 22;
pub const RUNNING: u32 = 1;
pub const EXITED: u32 = 2;
pub const FAULTED: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Version,
    Kind,
    Flags,
    Range,
    Reserved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Key {
        flags: u32,
        kind: u32,
        ascii: u8,
    },
    Pointer {
        x: u32,
        y: u32,
        buttons: u8,
    },
    Reset,
    Focus(bool),
    /// Accepted by the kernel only in smoke builds/sessions.
    TestFault,
    /// Finite CPU-bound verification work; likewise restricted to smoke.
    TestBusy,
}

impl Event {
    pub fn encode(self) -> Result<[u8; EVENT_SIZE], Error> {
        let mut words = [0u32; 8];
        words[0] = 1;
        match self {
            Self::Key { flags, kind, ascii } => {
                if flags & !63 != 0 {
                    return Err(Error::Flags);
                }
                if !(1..=14).contains(&kind) {
                    return Err(Error::Kind);
                }
                if ascii != 0 && (kind != 1 || !(32..=126).contains(&ascii)) {
                    return Err(Error::Range);
                }
                words[1] = 1;
                words[2] = flags;
                words[3] = kind;
                words[5] = u32::from(ascii);
            }
            Self::Pointer { x, y, buttons } => {
                if x >= WIDTH as u32 || y >= HEIGHT as u32 {
                    return Err(Error::Range);
                }
                if buttons & !7 != 0 {
                    return Err(Error::Flags);
                }
                words[1] = 2;
                words[2] = u32::from(buttons);
                words[6] = x;
                words[7] = y;
            }
            Self::Reset => words[1] = 3,
            Self::Focus(focused) => {
                words[1] = 4;
                words[2] = u32::from(focused);
            }
            Self::TestFault => words[1] = 5,
            Self::TestBusy => words[1] = 6,
        }
        let mut bytes = [0u8; EVENT_SIZE];
        for (destination, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            *destination = word.to_le_bytes();
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != EVENT_SIZE {
            return Err(Error::Length);
        }
        let mut words = [0u32; 8];
        for (word, source) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*source);
        }
        if words[0] != 1 {
            return Err(Error::Version);
        }
        let event = match words[1] {
            1 => Self::Key {
                flags: words[2],
                kind: words[3],
                ascii: u8::try_from(words[5]).map_err(|_| Error::Range)?,
            },
            2 => Self::Pointer {
                x: words[6],
                y: words[7],
                buttons: u8::try_from(words[2]).map_err(|_| Error::Flags)?,
            },
            3 => Self::Reset,
            4 if words[2] <= 1 => Self::Focus(words[2] != 0),
            5 => Self::TestFault,
            6 => Self::TestBusy,
            _ => return Err(Error::Kind),
        };
        if event.encode()? != bytes {
            return Err(Error::Reserved);
        }
        Ok(event)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppInfo {
    pub pid: u64,
    pub revision: u64,
    pub state: u32,
    pub width: u32,
    pub height: u32,
    pub kind: u32,
    pub detail: u64,
}

impl AppInfo {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != INFO_SIZE {
            return Err(Error::Length);
        }
        let u32_at = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let u64_at = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let info = Self {
            pid: u64_at(0),
            revision: u64_at(8),
            state: u32_at(16),
            width: u32_at(20),
            height: u32_at(24),
            kind: u32_at(28),
            detail: u64_at(32),
        };
        if !(1..=i64::MAX as u64).contains(&info.pid)
            || !(RUNNING..=FAULTED).contains(&info.state)
            || info.width != WIDTH as u32
            || info.height != HEIGHT as u32
            || info.kind > COUNTER as u32
        {
            return Err(Error::Range);
        }
        Ok(info)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileRequest {
    pub revision: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl TileRequest {
    pub fn packet_len(self) -> Result<usize, Error> {
        if !(1..=TILE as u32).contains(&self.width)
            || !(1..=TILE as u32).contains(&self.height)
            || self.x > (WIDTH as u32).saturating_sub(self.width)
            || self.y > (HEIGHT as u32).saturating_sub(self.height)
        {
            return Err(Error::Range);
        }
        Ok(READ_HEADER + self.width as usize * self.height as usize * 4)
    }

    pub fn encode_header(self) -> Result<[u8; READ_HEADER], Error> {
        self.packet_len()?;
        let mut bytes = [0u8; READ_HEADER];
        bytes[..8].copy_from_slice(&self.revision.to_le_bytes());
        for (destination, field) in bytes[8..].as_chunks_mut::<4>().0.iter_mut().zip([
            self.x,
            self.y,
            self.width,
            self.height,
        ]) {
            *destination = field.to_le_bytes();
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_round_trip_and_canonical_records() {
        for event in [
            Event::Key {
                flags: 1,
                kind: 1,
                ascii: b'A',
            },
            Event::Pointer {
                x: 319,
                y: 191,
                buttons: 7,
            },
            Event::Reset,
            Event::Focus(false),
            Event::Focus(true),
            Event::TestFault,
            Event::TestBusy,
        ] {
            let bytes = event.encode().unwrap();
            assert_eq!(Event::decode(&bytes), Ok(event));
            assert_eq!(Event::decode(&bytes[..31]), Err(Error::Length));
            let mut bad = bytes;
            bad[16] = 1;
            assert_eq!(Event::decode(&bad), Err(Error::Reserved));
        }
    }
    #[test]
    fn invalid_input_has_no_canonical_encoding() {
        for event in [
            Event::Key {
                flags: 64,
                kind: 1,
                ascii: b'a',
            },
            Event::Key {
                flags: 1,
                kind: 0,
                ascii: 0,
            },
            Event::Key {
                flags: 1,
                kind: 2,
                ascii: b'a',
            },
            Event::Key {
                flags: 1,
                kind: 1,
                ascii: 127,
            },
            Event::Pointer {
                x: u32::MAX,
                y: 0,
                buttons: 0,
            },
            Event::Pointer {
                x: 0,
                y: 192,
                buttons: 0,
            },
            Event::Pointer {
                x: 0,
                y: 0,
                buttons: 8,
            },
        ] {
            assert!(event.encode().is_err());
        }
    }
    #[test]
    fn tile_bounds_and_exact_transfer_size() {
        let tile = TileRequest {
            revision: 7,
            x: 304,
            y: 176,
            width: 16,
            height: 16,
        };
        assert_eq!(tile.packet_len(), Ok(MAX_READ_PACKET));
        assert_eq!(&tile.encode_header().unwrap()[..8], &7u64.to_le_bytes());
        for invalid in [
            TileRequest { x: 305, ..tile },
            TileRequest {
                y: u32::MAX,
                ..tile
            },
            TileRequest { width: 0, ..tile },
            TileRequest { height: 17, ..tile },
        ] {
            assert!(invalid.encode_header().is_err());
        }
    }

    #[test]
    fn process_metadata_rejects_invalid_identity_state_and_extent() {
        let mut bytes = [0u8; INFO_SIZE];
        bytes[..8].copy_from_slice(&1u64.to_le_bytes());
        for (offset, value) in [(16, RUNNING), (20, WIDTH as u32), (24, HEIGHT as u32)] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(AppInfo::decode(&bytes).unwrap().pid, 1);
        assert_eq!(AppInfo::decode(&bytes[..39]), Err(Error::Length));
        for (offset, value) in [(0, 0u32), (16, 0), (16, 4), (20, 0), (24, 193), (28, 2)] {
            let mut bad = bytes;
            bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert_eq!(AppInfo::decode(&bad), Err(Error::Range));
        }
    }
}
