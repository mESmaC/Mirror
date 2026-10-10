#![no_std]
#![forbid(unsafe_code)]
//! Optional hardware monotonic elapsed time, independent of delivered timer
//! interrupts. Values are nanoseconds since clock initialization, not UTC.

/// RDI points to a writable CLOCK_SNAPSHOT_SIZE-byte output, RSI is its exact
/// length, and RDX is zero. Invalid length/reserved arguments return EINVAL
/// first. An unavailable clock returns ENODEV without inspecting or touching
/// the output range; otherwise an invalid output range returns EFAULT.
/// Success writes one complete record and returns CLOCK_SNAPSHOT_SIZE.
pub const SYS_CLOCK_GET: u64 = 35;
pub const CLOCK_SNAPSHOT_SIZE: usize = 24;
pub const CLOCK_VERSION: u32 = 1;

pub const fn clock_arguments_valid(length: u64, reserved: u64) -> bool {
    length == CLOCK_SNAPSHOT_SIZE as u64 && reserved == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSnapshot {
    pub monotonic_ns: u64,
    /// Positive clock resolution in nanoseconds, rounded up when necessary.
    pub resolution_ns: u64,
}

impl ClockSnapshot {
    pub const fn is_valid(self) -> bool {
        self.resolution_ns != 0
    }

    /// Little-endian version, reserved zero, elapsed time, and resolution.
    pub fn encode(self) -> [u8; CLOCK_SNAPSHOT_SIZE] {
        let mut bytes = [0; CLOCK_SNAPSHOT_SIZE];
        bytes[..4].copy_from_slice(&CLOCK_VERSION.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.monotonic_ns.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.resolution_ns.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != CLOCK_SNAPSHOT_SIZE
            || u32::from_le_bytes(bytes[..4].try_into().ok()?) != CLOCK_VERSION
            || bytes[4..8] != [0; 4]
        {
            return None;
        }
        let snapshot = Self {
            monotonic_ns: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            resolution_ns: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
        };
        snapshot.is_valid().then_some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_preserves_exact_layout_and_full_width_values() {
        let snapshot = ClockSnapshot {
            monotonic_ns: 0x8877_6655_4433_2211,
            resolution_ns: 0x1020_3040_5060_7080,
        };
        let bytes = snapshot.encode();
        assert_eq!(&bytes[..8], &[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            &bytes[8..16],
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
        assert_eq!(
            &bytes[16..],
            &[0x80, 0x70, 0x60, 0x50, 0x40, 0x30, 0x20, 0x10]
        );
        assert_eq!(ClockSnapshot::decode(&bytes), Some(snapshot));
        for monotonic_ns in [0, 1, i64::MAX as u64, u64::MAX] {
            for resolution_ns in [1, 100, 101, u64::MAX] {
                let snapshot = ClockSnapshot {
                    monotonic_ns,
                    resolution_ns,
                };
                assert_eq!(ClockSnapshot::decode(&snapshot.encode()), Some(snapshot));
            }
        }
    }

    #[test]
    fn malformed_length_version_reserved_and_resolution_are_rejected() {
        let good = ClockSnapshot {
            monotonic_ns: 42,
            resolution_ns: 10,
        }
        .encode();
        for length in 0..CLOCK_SNAPSHOT_SIZE {
            assert_eq!(ClockSnapshot::decode(&good[..length]), None);
        }
        assert_eq!(ClockSnapshot::decode(&[0; CLOCK_SNAPSHOT_SIZE + 1]), None);
        for version in [0_u32, 2, u32::MAX] {
            let mut bytes = good;
            bytes[..4].copy_from_slice(&version.to_le_bytes());
            assert_eq!(ClockSnapshot::decode(&bytes), None);
        }
        for index in 4..8 {
            let mut bytes = good;
            bytes[index] = 1;
            assert_eq!(ClockSnapshot::decode(&bytes), None);
        }
        let invalid = ClockSnapshot {
            monotonic_ns: 42,
            resolution_ns: 0,
        };
        assert!(!invalid.is_valid());
        assert_eq!(ClockSnapshot::decode(&invalid.encode()), None);
    }

    #[test]
    fn arguments_require_exact_length_and_zero_reserved() {
        assert!(clock_arguments_valid(CLOCK_SNAPSHOT_SIZE as u64, 0));
        for length in [0, 23, 25, 1_u64 << 32 | 24, u64::MAX] {
            assert!(!clock_arguments_valid(length, 0));
        }
        for reserved in [1, 1_u64 << 32, u64::MAX] {
            assert!(!clock_arguments_valid(CLOCK_SNAPSHOT_SIZE as u64, reserved));
        }
    }
}
