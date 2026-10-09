//! Clock ABI and checked-copy probes. The IRQ tick and sleep ABIs retain their
//! existing meaning whether or not an independent elapsed clock is available.

use mirror_clock_protocol::{CLOCK_SNAPSHOT_SIZE, ClockSnapshot, SYS_CLOCK_GET};
use mirror_thread_protocol::USER_ADDRESS_END;

use crate::{
    EFAULT, EINVAL, KERNEL_BASE, PAGE_SIZE, PROT_READ, PROT_READ_WRITE, SYS_SLEEP, SYS_YIELD,
    require, syscall, vm_protect, vm_release, vm_reserve, vm_stats, write,
};

const ENODEV: i64 = -19;
const MEMORY: u64 = 0x1_0900_0000;
const SENTINEL: u8 = 0xa5;
static READONLY: [u8; CLOCK_SNAPSHOT_SIZE] = [0x5a; CLOCK_SNAPSHOT_SIZE];

fn get(address: u64) -> i64 {
    syscall(SYS_CLOCK_GET, address, CLOCK_SNAPSHOT_SIZE as u64, 0)
}

fn read_bytes(address: u64) -> [u8; CLOCK_SNAPSHOT_SIZE] {
    let mut bytes = [0; CLOCK_SNAPSHOT_SIZE];
    for (index, byte) in bytes.iter_mut().enumerate() {
        // SAFETY: Each caller owns a live readable range of this exact size.
        *byte = unsafe { ((address + index as u64) as *const u8).read_volatile() };
    }
    bytes
}

fn snapshot() -> ClockSnapshot {
    let mut bytes = [SENTINEL; CLOCK_SNAPSHOT_SIZE + 2];
    require(
        get(bytes[1..].as_mut_ptr() as u64) == CLOCK_SNAPSHOT_SIZE as i64,
        713,
    );
    require(
        bytes[0] == SENTINEL && bytes[CLOCK_SNAPSHOT_SIZE + 1] == SENTINEL,
        714,
    );
    let result = ClockSnapshot::decode(&bytes[1..CLOCK_SNAPSHOT_SIZE + 1]);
    require(result.is_some(), 715);
    let result = result.unwrap();
    // This implementation exposes HPET. The protocol itself permits future
    // clocks with a different positive resolution.
    require(result.resolution_ns <= 100, 716);
    result
}

fn check_progress() -> ClockSnapshot {
    let mut previous = snapshot();
    for _ in 0..8 {
        let current = snapshot();
        require(current.monotonic_ns >= previous.monotonic_ns, 717);
        require(current.resolution_ns == previous.resolution_ns, 718);
        previous = current;
    }
    require(syscall(SYS_YIELD, 0, 0, 0) == 0, 719);
    let current = snapshot();
    require(current.monotonic_ns >= previous.monotonic_ns, 720);
    require(current.resolution_ns == previous.resolution_ns, 721);
    require(syscall(SYS_SLEEP, 2, 0, 0) == 0, 722);
    let after_sleep = snapshot();
    require(after_sleep.monotonic_ns > current.monotonic_ns, 723);
    require(after_sleep.resolution_ns == current.resolution_ns, 724);
    after_sleep
}

fn check_copy(previous: ClockSnapshot) {
    let baseline = vm_stats();
    vm_reserve(MEMORY, 2, PROT_READ_WRITE);
    let output = MEMORY + PAGE_SIZE - 8;
    // Reject the entire range before materializing even its writable prefix.
    vm_protect(MEMORY + PAGE_SIZE, 1, PROT_READ);
    let before = vm_stats();
    require(get(output) == EFAULT, 725);
    require(vm_stats() == before, 726);
    vm_protect(MEMORY + PAGE_SIZE, 1, PROT_READ_WRITE);
    require(get(output) == CLOCK_SNAPSHOT_SIZE as i64, 727);
    require(
        vm_stats().committed == baseline.committed + 2 * PAGE_SIZE,
        728,
    );
    let current = ClockSnapshot::decode(&read_bytes(output));
    require(current.is_some(), 729);
    let current = current.unwrap();
    require(current.monotonic_ns >= previous.monotonic_ns, 730);
    require(current.resolution_ns == previous.resolution_ns, 731);

    for index in 0..CLOCK_SNAPSHOT_SIZE {
        // SAFETY: Both pages have been committed and remain owned and writable.
        unsafe { ((output + index as u64) as *mut u8).write_volatile(SENTINEL) };
    }
    vm_protect(MEMORY + PAGE_SIZE, 1, PROT_READ);
    let before = vm_stats();
    require(get(output) == EFAULT, 732);
    require(read_bytes(output) == [SENTINEL; CLOCK_SNAPSHOT_SIZE], 733);
    require(vm_stats() == before, 734);

    // An unmapped suffix must leave the still-mapped eight-byte prefix intact.
    vm_release(MEMORY + PAGE_SIZE, 1);
    let before = vm_stats();
    require(get(output) == EFAULT, 735);
    require(vm_stats() == before, 736);
    for index in 0..8 {
        // SAFETY: Only the retained prefix is read after releasing the suffix.
        require(
            unsafe { ((output + index) as *const u8).read_volatile() } == SENTINEL,
            737,
        );
    }
    vm_release(MEMORY, 1);
    require(vm_stats() == baseline, 738);
}

pub(super) fn check(console: u64) {
    let mut bytes = [SENTINEL; CLOCK_SNAPSHOT_SIZE];
    let pointer = bytes.as_mut_ptr() as u64;
    let baseline = vm_stats();
    for length in [0, 23, 25, 1_u64 << 32 | 24, u64::MAX] {
        require(syscall(SYS_CLOCK_GET, pointer, length, 0) == EINVAL, 700);
        require(bytes == [SENTINEL; CLOCK_SNAPSHOT_SIZE], 701);
        require(syscall(SYS_CLOCK_GET, 0, length, 0) == EINVAL, 702);
    }
    for reserved in [1, 1_u64 << 32, u64::MAX] {
        require(
            syscall(SYS_CLOCK_GET, pointer, CLOCK_SNAPSHOT_SIZE as u64, reserved) == EINVAL,
            703,
        );
        require(bytes == [SENTINEL; CLOCK_SNAPSHOT_SIZE], 704);
        require(
            syscall(SYS_CLOCK_GET, 0, CLOCK_SNAPSHOT_SIZE as u64, reserved) == EINVAL,
            705,
        );
    }
    require(vm_stats() == baseline, 706);
    let result = get(pointer);
    require(
        result == ENODEV || result == CLOCK_SNAPSHOT_SIZE as i64,
        707,
    );
    let invalid = [
        0,
        KERNEL_BASE,
        USER_ADDRESS_END - 8,
        u64::MAX - 7,
        READONLY.as_ptr() as u64,
    ];
    if result == ENODEV {
        require(bytes == [SENTINEL; CLOCK_SNAPSHOT_SIZE], 708);
        for address in invalid {
            require(get(address) == ENODEV, 709);
        }
        vm_reserve(MEMORY, 2, PROT_READ_WRITE);
        let before = vm_stats();
        require(get(MEMORY + PAGE_SIZE - 8) == ENODEV, 710);
        require(vm_stats() == before, 711);
        vm_release(MEMORY, 2);
        require(vm_stats() == baseline, 712);
        write(console, b"Mirror init: clock unavailable\n");
        return;
    }
    for address in invalid {
        require(get(address) == EFAULT, 739);
    }
    require(
        read_bytes(READONLY.as_ptr() as u64) == [0x5a; CLOCK_SNAPSHOT_SIZE],
        740,
    );
    require(vm_stats() == baseline, 741);
    let previous = check_progress();
    check_copy(previous);
    require(vm_stats() == baseline, 742);
    write(console, b"Mirror init: clock checks passed\n");
}
