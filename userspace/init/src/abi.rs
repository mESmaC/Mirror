//! Smoke probes for the shared table, reserved calls and UTF-8 string boundary.

use mirror_sys::{EINVAL, ENOSYS, SYS_RANDOM_BYTES, SYS_REALTIME_GET, SYS_WRITE};

use crate::{KERNEL_BASE, require, syscall, vm_stats};

pub fn check(console: u64) {
    let before = vm_stats();
    let mut output = [0xa5_u8; 32];
    for number in [SYS_REALTIME_GET, SYS_RANDOM_BYTES] {
        for address in [0, KERNEL_BASE, u64::MAX] {
            require(syscall(number, address, u64::MAX, u64::MAX) == ENOSYS, 760);
        }
        require(
            syscall(number, output.as_mut_ptr() as u64, output.len() as u64, 0) == ENOSYS,
            761,
        );
        require(output == [0xa5; 32], 762);
    }
    let after = vm_stats();
    require(
        after.reserved == before.reserved && after.committed == before.committed,
        763,
    );
    require(syscall(0, 0, 0, 0) == ENOSYS, 764);

    for bytes in [
        &b"Mirror UTF-8: \xc3\xa9\xce\xbb\xe4\xb8\xad\xf0\x9f\x99\x82\n"[..],
        &b"Mirror UTF-8: before\0after\n"[..],
    ] {
        require(
            syscall(
                SYS_WRITE,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
                console,
            ) == bytes.len() as i64,
            765,
        );
    }
    for bytes in [
        &b"\x80"[..],
        &b"\xc0\x80"[..],
        &b"\xe2\x82"[..],
        &b"\xed\xa0\x80"[..],
        &b"\xf4\x90\x80\x80"[..],
        &b"valid prefix\0\xff"[..],
    ] {
        require(
            syscall(
                SYS_WRITE,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
                console,
            ) == EINVAL,
            766,
        );
    }
}
