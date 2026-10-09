#![no_std]
//! Native threads, execution-context TLS, and process-private word waits. The
//! runtime owns every thread's stack and TLS storage, retaining them until join.

pub const SYS_TLS_SET: u64 = 27;
pub const SYS_TLS_GET: u64 = 28;
pub const SYS_THREAD_CREATE: u64 = 29;
pub const SYS_THREAD_EXIT: u64 = 30;
pub const SYS_THREAD_JOIN: u64 = 31;
pub const SYS_THREAD_ID: u64 = 32;
/// RDI points to an aligned readable u32, RSI is its expected value, and RDX is
/// a relative timeout. A mismatched value returns EAGAIN before checking a zero
/// timeout. Otherwise zero polls with ETIMEDOUT, and a nonzero timeout blocks.
/// Successful wake returns zero; callers must recheck their own condition.
pub const SYS_WAIT_U32: u64 = 33;
/// RDI identifies a readable word in the calling process, RSI bounds the wake
/// count by MAX_THREADS, and RDX must be zero. Returns the number actually woken.
/// Does not read, change, or materialize the word; expired waits are not counted.
pub const SYS_WAKE_U32: u64 = 34;
pub const TLS_BASES_SIZE: usize = 16;
pub const THREAD_START_SIZE: usize = 40;
pub const MAX_THREADS: usize = 4;
pub const USER_ADDRESS_END: u64 = 1 << 47;
pub const WAIT_WORD_SIZE: u64 = 4;
pub const MAX_WAIT_TICKS: u64 = 1000;
/// A wait without a deadline. Release or PROT_NONE still cancels it with EFAULT.
pub const WAIT_FOREVER: u64 = u64::MAX;

/// Structural WAIT arguments. Address range and readable permission are
/// separately checked by the kernel, so malformed arguments return EINVAL
/// before an otherwise invalid address can return EFAULT.
pub const fn wait_arguments_valid(address: u64, expected: u64, timeout: u64) -> bool {
    address.is_multiple_of(WAIT_WORD_SIZE)
        && expected <= u32::MAX as u64
        && (timeout <= MAX_WAIT_TICKS || timeout == WAIT_FOREVER)
}

/// WAKE does not read or change the word, but still requires a readable word
/// owned by the caller's process. A zero count validates without waking.
pub const fn wake_arguments_valid(address: u64, count: u64, reserved: u64) -> bool {
    address.is_multiple_of(WAIT_WORD_SIZE) && count <= MAX_THREADS as u64 && reserved == 0
}

pub const fn wait_word_address_valid(address: u64) -> bool {
    address.is_multiple_of(WAIT_WORD_SIZE)
        && address >= 4096
        && address <= USER_ADDRESS_END - WAIT_WORD_SIZE
}

/// A raw System V entry point receives `argument` in RDI and `stack` in RSP.
/// Every other general register is zero; floating-point state is freshly
/// initialized. The entry point must exit through SYS_THREAD_EXIT, never RET.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadStart {
    pub entry: u64,
    pub stack: u64,
    pub argument: u64,
    pub fs: u64,
    pub gs: u64,
}

impl ThreadStart {
    /// Structural validation only. The kernel additionally checks executable
    /// entry memory and writable stack memory in the creating process.
    pub const fn is_valid(self) -> bool {
        self.entry >= 4096
            && self.entry < USER_ADDRESS_END
            && self.stack >= 4096
            && self.stack < USER_ADDRESS_END
            && self.stack % 16 == 8
            && (TlsBases {
                fs: self.fs,
                gs: self.gs,
            })
            .is_valid()
    }

    pub fn encode(self) -> [u8; THREAD_START_SIZE] {
        let mut bytes = [0; THREAD_START_SIZE];
        for (index, value) in [self.entry, self.stack, self.argument, self.fs, self.gs]
            .into_iter()
            .enumerate()
        {
            bytes[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != THREAD_START_SIZE {
            return None;
        }
        let start = Self {
            entry: u64::from_le_bytes(bytes[0..8].try_into().ok()?),
            stack: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            argument: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
            fs: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
            gs: u64::from_le_bytes(bytes[32..40].try_into().ok()?),
        };
        start.is_valid().then_some(start)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlsBases {
    pub fs: u64,
    pub gs: u64,
}

impl TlsBases {
    /// Both bases must be in the canonical lower half. Zero, unaligned, and
    /// currently unmapped addresses are valid; accesses still obey paging.
    pub const fn is_valid(self) -> bool {
        self.fs < USER_ADDRESS_END && self.gs < USER_ADDRESS_END
    }

    pub fn encode(self) -> [u8; TLS_BASES_SIZE] {
        let mut bytes = [0; TLS_BASES_SIZE];
        bytes[..8].copy_from_slice(&self.fs.to_le_bytes());
        bytes[8..].copy_from_slice(&self.gs.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != TLS_BASES_SIZE {
            return None;
        }
        let bases = Self {
            fs: u64::from_le_bytes(bytes[..8].try_into().ok()?),
            gs: u64::from_le_bytes(bytes[8..].try_into().ok()?),
        };
        bases.is_valid().then_some(bases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_and_wake_validate_full_width_inputs_and_boundaries() {
        for address in [4096, USER_ADDRESS_END - WAIT_WORD_SIZE] {
            assert!(wait_word_address_valid(address));
            for timeout in [0, 1, MAX_WAIT_TICKS, WAIT_FOREVER] {
                assert!(wait_arguments_valid(address, u32::MAX as u64, timeout));
            }
            assert!(wake_arguments_valid(address, 0, 0));
            assert!(wake_arguments_valid(address, MAX_THREADS as u64, 0));
        }
        for address in [0, 4092, USER_ADDRESS_END, u64::MAX] {
            assert!(!wait_word_address_valid(address));
        }
        for address in [4097, 4098, 4099] {
            assert!(!wait_word_address_valid(address));
            assert!(!wait_arguments_valid(address, 0, 0));
            assert!(!wake_arguments_valid(address, 0, 0));
        }
        assert!(!wait_arguments_valid(4096, 1_u64 << 32, 0));
        assert!(!wait_arguments_valid(4096, 0, MAX_WAIT_TICKS + 1));
        assert!(!wait_arguments_valid(4096, 0, WAIT_FOREVER - 1));
        assert!(!wake_arguments_valid(4096, MAX_THREADS as u64 + 1, 0));
        assert!(!wake_arguments_valid(4096, 0, 1));
    }

    fn start() -> ThreadStart {
        ThreadStart {
            entry: 0x401000,
            stack: 0x7fff_fff8,
            argument: u64::MAX,
            fs: 0x1122,
            gs: 0x3344,
        }
    }

    #[test]
    fn thread_record_preserves_full_width_argument_and_exact_layout() {
        let descriptor = start();
        let bytes = descriptor.encode();
        assert_eq!(&bytes[..8], &0x401000_u64.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x7fff_fff8_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &[0xff; 8]);
        assert_eq!(&bytes[24..32], &0x1122_u64.to_le_bytes());
        assert_eq!(&bytes[32..40], &0x3344_u64.to_le_bytes());
        assert_eq!(ThreadStart::decode(&bytes), Some(descriptor));
        assert_eq!(ThreadStart::decode(&bytes[..39]), None);
        assert_eq!(ThreadStart::decode(&[0; 41]), None);
    }

    #[test]
    fn thread_record_rejects_invalid_entry_stack_and_tls() {
        for entry in [0, 4095, USER_ADDRESS_END, u64::MAX] {
            assert!(!ThreadStart { entry, ..start() }.is_valid());
        }
        for stack in [0, 4095, 4096, 0x7fff_fff0, USER_ADDRESS_END + 8] {
            assert!(!ThreadStart { stack, ..start() }.is_valid());
        }
        assert!(
            ThreadStart {
                entry: 4096,
                stack: 4104,
                ..start()
            }
            .is_valid()
        );
        assert!(
            ThreadStart {
                entry: USER_ADDRESS_END - 1,
                stack: USER_ADDRESS_END - 8,
                fs: USER_ADDRESS_END - 1,
                gs: USER_ADDRESS_END - 1,
                ..start()
            }
            .is_valid()
        );
        for bases in [(USER_ADDRESS_END, 0), (0, USER_ADDRESS_END)] {
            assert_eq!(
                ThreadStart::decode(
                    &ThreadStart {
                        fs: bases.0,
                        gs: bases.1,
                        ..start()
                    }
                    .encode()
                ),
                None
            );
        }
    }

    #[test]
    fn base_contract_accepts_only_the_canonical_lower_half() {
        for base in [0, 1, 4095, 0x1_0400_0001, USER_ADDRESS_END - 1] {
            assert!(TlsBases { fs: base, gs: 0 }.is_valid());
            assert!(TlsBases { fs: 0, gs: base }.is_valid());
        }
        for base in [USER_ADDRESS_END, 0xffff_8000_0000_0000, u64::MAX] {
            assert!(!TlsBases { fs: base, gs: 0 }.is_valid());
            assert!(!TlsBases { fs: 0, gs: base }.is_valid());
        }
    }

    #[test]
    fn record_has_exact_little_endian_layout_and_checked_decode() {
        let bases = TlsBases {
            fs: 0x0000_1122_3344_5566,
            gs: 0x0000_7788_99aa_bbcc,
        };
        let bytes = [
            0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0, 0, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0, 0,
        ];
        assert_eq!(bases.encode(), bytes);
        assert_eq!(TlsBases::decode(&bytes), Some(bases));
        assert_eq!(TlsBases::decode(&bytes[..15]), None);
        assert_eq!(TlsBases::decode(&[0; 17]), None);
        assert_eq!(TlsBases::decode(&[0xff; 16]), None);
    }
}
