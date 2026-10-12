#![no_std]
#![forbid(unsafe_code)]
//! Anonymous virtual-memory ABI. Addresses and lengths are page aligned;
//! reservation does not commit physical pages. Protection never permits execute.

pub const SYS_VM_RESERVE: u64 = 23;
pub const SYS_VM_RELEASE: u64 = 24;
pub const SYS_VM_PROTECT: u64 = 25;
pub const SYS_VM_STATS: u64 = 26;
pub const PAGE_SIZE: u64 = 4096;
pub const PROT_NONE: u64 = 0;
pub const PROT_READ: u64 = 1;
pub const PROT_READ_WRITE: u64 = 3;
pub const ARENA_START: u64 = 0x1_0000_0000;
pub const ARENA_END: u64 = 0x10_0000_0000;
pub const MAX_REGIONS: usize = 64;
pub const RESERVED_LIMIT: u64 = 256 * 1024 * 1024;
pub const COMMITTED_LIMIT: u64 = 32 * 1024 * 1024;
pub const STATS_SIZE: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmStats {
    pub reserved_bytes: u64,
    pub committed_bytes: u64,
    pub regions: u64,
    pub reserved_limit: u64,
    pub committed_limit: u64,
}

impl VmStats {
    pub fn encode(self) -> [u8; STATS_SIZE] {
        let mut bytes = [0; STATS_SIZE];
        bytes[..4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        for (chunk, value) in bytes[8..].as_chunks_mut::<8>().0.iter_mut().zip([
            self.reserved_bytes,
            self.committed_bytes,
            self.regions,
            self.reserved_limit,
            self.committed_limit,
        ]) {
            *chunk = value.to_le_bytes();
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != STATS_SIZE
            || bytes[..4] != 1_u32.to_le_bytes()
            || bytes[4..8] != (PAGE_SIZE as u32).to_le_bytes()
        {
            return None;
        }
        let word = |index: usize| u64::from_le_bytes(bytes[index..index + 8].try_into().unwrap());
        let stats = Self {
            reserved_bytes: word(8),
            committed_bytes: word(16),
            regions: word(24),
            reserved_limit: word(32),
            committed_limit: word(40),
        };
        if stats.reserved_bytes > stats.reserved_limit
            || stats.committed_bytes > stats.reserved_bytes
            || stats.committed_bytes > stats.committed_limit
            || !stats.reserved_bytes.is_multiple_of(PAGE_SIZE)
            || !stats.committed_bytes.is_multiple_of(PAGE_SIZE)
            || stats.regions > MAX_REGIONS as u64
        {
            return None;
        }
        Some(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_are_versioned_and_reject_impossible_accounting() {
        let stats = VmStats {
            reserved_bytes: PAGE_SIZE * 9,
            committed_bytes: PAGE_SIZE,
            regions: 2,
            reserved_limit: RESERVED_LIMIT,
            committed_limit: COMMITTED_LIMIT,
        };
        assert_eq!(VmStats::decode(&stats.encode()), Some(stats));
        assert_eq!(VmStats::decode(&stats.encode()[..47]), None);
        let mut bytes = stats.encode();
        bytes[0] = 2;
        assert_eq!(VmStats::decode(&bytes), None);
        bytes = stats.encode();
        bytes[16..24].copy_from_slice(&(10 * PAGE_SIZE).to_le_bytes());
        assert_eq!(VmStats::decode(&bytes), None);
        bytes = stats.encode();
        bytes[8] = 1;
        assert_eq!(VmStats::decode(&bytes), None);
    }

    #[test]
    fn region_count_is_bounded_by_the_sixty_four_region_limit() {
        // ABI (spec 0030a, 0030-D4): sixty-four regions per process.
        assert_eq!(MAX_REGIONS, 64);
        let full = VmStats {
            reserved_bytes: PAGE_SIZE * MAX_REGIONS as u64,
            committed_bytes: 0,
            regions: MAX_REGIONS as u64,
            reserved_limit: RESERVED_LIMIT,
            committed_limit: COMMITTED_LIMIT,
        };
        assert_eq!(VmStats::decode(&full.encode()), Some(full));
        let over = VmStats {
            regions: MAX_REGIONS as u64 + 1,
            ..full
        };
        assert_eq!(VmStats::decode(&over.encode()), None);
    }
}
