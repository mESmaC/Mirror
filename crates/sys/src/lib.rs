#![no_std]
#![forbid(unsafe_code)]
//! Native Mirror ABI numbers and negative error results (spec 0020).
//!
//! `int 0x80` takes a syscall number in RAX and three arguments in RDI, RSI,
//! RDX. RAX returns a nonnegative success value or one of the negative errors
//! below. Ordinary calls preserve the other general registers; successful
//! THREAD_JOIN also returns RDX, and successful WAIT_U32 returns RSI and RDX.
//!
//! Strings cross the ABI as an address and a byte count, with complete UTF-8
//! validation before use. A NUL byte is data, not a terminator. Binary records
//! remain byte arrays with their own protocol validation. All pointers, lengths
//! and integers use the existing x86_64 register and little-endian wire formats.
//! This crate supplies constants and validation, without a syscall backend.

pub const SYS_WRITE: u64 = 1;
pub const SYS_TICKS: u64 = 2;
pub const SYS_EXIT: u64 = 3;
pub const SYS_YIELD: u64 = 4;
pub const SYS_SLEEP: u64 = 5;
pub const SYS_PID: u64 = 6;
pub const SYS_BOOT_HANDLE: u64 = 7;
pub const SYS_SEND: u64 = 8;
pub const SYS_RECV: u64 = 9;
pub const SYS_RECV_WAIT: u64 = 10;
pub const SYS_CLOSE: u64 = 11;
pub const SYS_DUP: u64 = 12;
pub const SYS_SURFACE_INFO: u64 = 13;
pub const SYS_SURFACE_PRESENT: u64 = 14;
pub const SYS_INPUT_WAIT: u64 = 15;
pub const SYS_INPUT_POLL: u64 = 16;
pub const SYS_APP_LAUNCH: u64 = 17;
pub const SYS_APP_INFO: u64 = 18;
pub const SYS_APP_EVENT: u64 = 19;
pub const SYS_APP_READ: u64 = 20;
pub const SYS_APP_CLOSE: u64 = 21;
pub const SYS_SURFACE_COMMIT: u64 = 22;
pub const SYS_VM_RESERVE: u64 = 23;
pub const SYS_VM_RELEASE: u64 = 24;
pub const SYS_VM_PROTECT: u64 = 25;
pub const SYS_VM_STATS: u64 = 26;
pub const SYS_TLS_SET: u64 = 27;
pub const SYS_TLS_GET: u64 = 28;
pub const SYS_THREAD_CREATE: u64 = 29;
pub const SYS_THREAD_EXIT: u64 = 30;
pub const SYS_THREAD_JOIN: u64 = 31;
pub const SYS_THREAD_ID: u64 = 32;
pub const SYS_WAIT_U32: u64 = 33;
pub const SYS_WAKE_U32: u64 = 34;
pub const SYS_CLOCK_GET: u64 = 35;
/// Reserved for UTC clock reads (0056a). Returns ENOSYS without accessing memory.
pub const SYS_REALTIME_GET: u64 = 36;
/// Reserved for random bytes (0036). Returns ENOSYS without accessing memory.
pub const SYS_RANDOM_BYTES: u64 = 37;

/// Missing, foreign, or already joined thread.
pub const ESRCH: i64 = -3;
/// Missing, closed, stale, or foreign handle, or invalid bootstrap slot.
pub const EBADF: i64 = -9;
/// Would block, capacity exhausted, stale revision, or wait value mismatch.
pub const EAGAIN: i64 = -11;
/// Physical memory, VM quota, or VM region capacity exhausted.
pub const ENOMEM: i64 = -12;
/// Insufficient rights or wrong object type.
pub const EACCES: i64 = -13;
/// Invalid user memory or a wait cancelled by mapping withdrawal.
pub const EFAULT: i64 = -14;
/// A thread already has a joiner.
pub const EBUSY: i64 = -16;
/// An exact memory reservation overlaps an existing region.
pub const EEXIST: i64 = -17;
/// The optional clock is unavailable.
pub const ENODEV: i64 = -19;
/// Invalid argument or malformed UTF-8 string or protocol record.
pub const EINVAL: i64 = -22;
/// Handle table full or monotonic handle token space exhausted.
pub const EMFILE: i64 = -24;
/// The peer channel endpoint is closed.
pub const EPIPE: i64 = -32;
/// Joining oneself or a cycle of thread joins.
pub const EDEADLK: i64 = -35;
/// Unknown or reserved, unimplemented syscall.
pub const ENOSYS: i64 = -38;
/// A surface revision cannot increase without wrapping.
pub const EOVERFLOW: i64 = -75;
/// Message length or receive capacity is outside the accepted range.
pub const EMSGSIZE: i64 = -90;
/// A blocking operation or matched zero-timeout address wait reached its deadline.
pub const ETIMEDOUT: i64 = -110;

/// Validate an entire native ABI string, including bytes after embedded NULs.
/// The caller validates and copies the full user range before passing its bytes.
pub fn decode_utf8(bytes: &[u8]) -> Result<&str, i64> {
    core::str::from_utf8(bytes).map_err(|_| EINVAL)
}

#[cfg(test)]
mod tests;
