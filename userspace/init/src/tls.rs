//! The explicit native TLS ABI is checked through real FS/GS memory accesses.
//! Peers use identical private mappings with distinct segment bases and values.

use core::arch::{asm, global_asm};

use mirror_thread_protocol::{SYS_TLS_GET, SYS_TLS_SET, TLS_BASES_SIZE, USER_ADDRESS_END};

use crate::{
    EFAULT, EINVAL, KERNEL_BASE, PAGE_SIZE, PROT_READ, PROT_READ_WRITE, SYS_PID, SYS_SLEEP,
    SYS_YIELD, require, syscall, vm_protect, vm_read, vm_release, vm_reserve, vm_stats, vm_write,
    write,
};

global_asm!(include_str!("tls.S"));

const DEMAND: u64 = 0x1_0400_0000;
const FIRST: u64 = 0x9182_7304_5566_7788;
const SECOND: u64 = 0xa998_b776_c554_d332;
const THIRD: u64 = 0x1302_5789_abc0_def6;
const FOURTH: u64 = 0x789a_bcde_f102_3456;

// BSS storage leaves no anonymous reservation behind when the existing VM
// checks begin. These bases stay live throughout the FPU preemption probe.
static mut CANARIES: [u64; 8] = [0; 8];

unsafe extern "C" {
    pub(crate) fn mirror_probe_fsgsbase_fault();
}

fn bases() -> [u64; 2] {
    let mut bytes = [0u8; TLS_BASES_SIZE];
    require(
        syscall(
            SYS_TLS_GET,
            bytes.as_mut_ptr() as u64,
            bytes.len() as u64,
            0,
        ) == TLS_BASES_SIZE as i64,
        400,
    );
    [
        u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        u64::from_le_bytes(bytes[8..].try_into().unwrap()),
    ]
}

fn selectors() -> [u16; 2] {
    let fs: u16;
    let gs: u16;
    // SAFETY: Reading segment selectors is permitted in userspace.
    unsafe {
        asm!(
            "mov {fs:x}, fs",
            "mov {gs:x}, gs",
            fs = out(reg) fs,
            gs = out(reg) gs,
            options(nomem, nostack, preserves_flags),
        );
    }
    [fs, gs]
}

fn set(fs: u64, gs: u64) {
    require(syscall(SYS_TLS_SET, fs, gs, 0) == 0, 401);
    require(selectors() == [0, 0], 402);
    require(bases() == [fs, gs], 403);
}

fn canary_bases(pid: u64) -> [u64; 2] {
    // Peers use distinct bases within identically mapped private BSS pages.
    // Restoring the other process's base therefore reads untouched zeroes.
    let address = (&raw const CANARIES) as u64 + (pid & 1) * 32;
    [address + 8, address + 24]
}

fn expected(pid: u64) -> [u64; 4] {
    [FIRST ^ pid, SECOND ^ pid, THIRD ^ pid, FOURTH ^ pid]
}

fn load_words() -> [u64; 4] {
    let first: u64;
    let second: u64;
    let third: u64;
    let fourth: u64;
    // SAFETY: Callers establish both bases in owned writable memory with an
    // eight-byte word at offsets -8 and 0. A reserved page may demand-fault.
    unsafe {
        asm!(
            "mov {first}, qword ptr fs:[-8]",
            "mov {second}, qword ptr fs:[0]",
            "mov {third}, qword ptr gs:[-8]",
            "mov {fourth}, qword ptr gs:[0]",
            first = out(reg) first,
            second = out(reg) second,
            third = out(reg) third,
            fourth = out(reg) fourth,
            options(nostack, preserves_flags),
        );
    }
    [first, second, third, fourth]
}

fn store_words(words: [u64; 4]) {
    // SAFETY: The same live owned ranges required by load_words are writable.
    unsafe {
        asm!(
            "mov qword ptr fs:[-8], {first}",
            "mov qword ptr fs:[0], {second}",
            "mov qword ptr gs:[-8], {third}",
            "mov qword ptr gs:[0], {fourth}",
            first = in(reg) words[0],
            second = in(reg) words[1],
            third = in(reg) words[2],
            fourth = in(reg) words[3],
            options(nostack, preserves_flags),
        );
    }
}

fn check_words(pid: u64) {
    require(load_words() == expected(pid), 404);
}

pub(super) fn check_initial() {
    // Called before the first ordinary syscall, on both initial processes and
    // the later fresh pair after normal exit or destructive fault teardown.
    require(selectors() == [0, 0], 405);
    require(bases() == [0, 0], 406);
    // SAFETY: This process has not written its private BSS canaries yet.
    require(unsafe { (&raw const CANARIES).read() } == [0; 8], 407);
}

pub(super) fn start(pid: u64) {
    // Unmapped and unaligned low-half bases are valid. Mapping and offset
    // validity are checked by the CPU only when a segment access is performed.
    for [fs, gs] in [[0, 0], [1, USER_ADDRESS_END - 1], [USER_ADDRESS_END - 1, 1]] {
        set(fs, gs);
    }
    let [fs, gs] = canary_bases(pid);
    set(fs, gs);
    store_words(expected(pid));
    check_words(pid);
    for invalid in [
        USER_ADDRESS_END,
        0x0001_0000_0000_0000,
        KERNEL_BASE,
        u64::MAX,
    ] {
        require(syscall(SYS_TLS_SET, invalid, gs, 0) == EINVAL, 408);
        require(bases() == [fs, gs] && selectors() == [0, 0], 409);
        require(syscall(SYS_TLS_SET, fs, invalid, 0) == EINVAL, 410);
        require(bases() == [fs, gs] && selectors() == [0, 0], 411);
        check_words(pid);
    }
    for reserved in [1, u64::MAX] {
        require(syscall(SYS_TLS_SET, 0, 0, reserved) == EINVAL, 412);
        require(bases() == [fs, gs], 413);
    }

    // SAFETY: CPL 3 may load the present flat user data selector 0x2b, and AX is
    // a declared clobber. Nothing addresses memory through FS or GS until `set`
    // installs owned bases again. Loading this selector resets the actual hidden
    // base. GET and context switches must observe that change rather than
    // resurrect the old bases last assigned through SET.
    unsafe {
        asm!(
            "mov ax, 0x2b",
            "mov fs, ax",
            "mov gs, ax",
            out("ax") _,
            options(nostack, preserves_flags),
        );
    }
    require(selectors() == [0x2b, 0x2b] && bases() == [0, 0], 414);
    require(syscall(SYS_TLS_SET, fs, USER_ADDRESS_END, 0) == EINVAL, 415);
    require(selectors() == [0x2b, 0x2b] && bases() == [0, 0], 416);
    require(syscall(SYS_YIELD, 0, 0, 0) == 0, 417);
    require(selectors() == [0x2b, 0x2b] && bases() == [0, 0], 418);
    require(syscall(SYS_SLEEP, 2, 0, 0) == 0, 419);
    require(selectors() == [0x2b, 0x2b] && bases() == [0, 0], 420);
    set(fs, gs);

    // SAFETY: Loading null selectors into FS and GS is permitted at CPL 3, and AX
    // is a declared clobber. Nothing addresses memory through FS or GS until
    // `set` installs owned bases again. Null-selector loads need not have
    // identical hidden-base behavior on all processors. Preserve the actual
    // observed bases and exact selector bits.
    unsafe {
        asm!(
            "mov ax, 3",
            "mov fs, ax",
            "mov ax, 2",
            "mov gs, ax",
            out("ax") _,
            options(nostack, preserves_flags),
        );
    }
    require(selectors() == [3, 2], 421);
    let observed = bases();
    require(selectors() == [3, 2], 422);
    require(syscall(SYS_YIELD, 0, 0, 0) == 0, 423);
    require(bases() == observed && selectors() == [3, 2], 424);

    let initial = vm_stats();
    let demand = vm_reserve(DEMAND, 4, PROT_READ_WRITE);
    let demand_fs = demand + 8 + (pid & 1) * 32;
    let demand_gs = demand + PAGE_SIZE + 8 + (pid & 1) * 32;
    set(demand_fs, demand_gs);
    require(load_words() == [0; 4], 425);
    require(
        vm_stats().committed == initial.committed + 2 * PAGE_SIZE,
        426,
    );
    store_words(expected(pid));
    require(syscall(SYS_PID, 0, 0, 0) == pid as i64, 427);
    check_words(pid);
    require(syscall(SYS_YIELD, 0, 0, 0) == 0, 428);
    check_words(pid);
    require(syscall(SYS_SLEEP, 2, 0, 0) == 0, 429);
    check_words(pid);

    let output = demand + 3 * PAGE_SIZE - 8;
    let before = vm_stats();
    for length in [0, 15, 17, u64::MAX] {
        require(syscall(SYS_TLS_GET, output, length, 0) == EINVAL, 430);
        require(vm_stats() == before, 431);
    }
    require(
        syscall(SYS_TLS_GET, output, TLS_BASES_SIZE as u64, 1) == EINVAL,
        432,
    );
    require(vm_stats() == before, 433);
    for address in [0, KERNEL_BASE, USER_ADDRESS_END - 8, u64::MAX - 7] {
        require(
            syscall(SYS_TLS_GET, address, TLS_BASES_SIZE as u64, 0) == EFAULT,
            434,
        );
        require(vm_stats() == before, 435);
    }

    // Whole-range validation must reject a read-only suffix without committing
    // the untouched writable prefix or changing TLS state.
    vm_protect(demand + 3 * PAGE_SIZE, 1, PROT_READ);
    let before = vm_stats();
    require(
        syscall(SYS_TLS_GET, output, TLS_BASES_SIZE as u64, 0) == EFAULT,
        436,
    );
    require(vm_stats() == before, 437);
    check_words(pid);
    vm_protect(demand + 3 * PAGE_SIZE, 1, PROT_READ_WRITE);
    require(
        syscall(SYS_TLS_GET, output, TLS_BASES_SIZE as u64, 0) == TLS_BASES_SIZE as i64,
        438,
    );
    require(
        vm_stats().committed == initial.committed + 4 * PAGE_SIZE,
        439,
    );
    require(vm_read(output) == demand_fs, 440);
    require(vm_read(output + 8) == demand_gs, 441);
    check_words(pid);

    // A mapped prefix must also remain byte-for-byte unchanged when the suffix
    // is read-only or missing, including after the successful deferred copy.
    vm_write(output, FIRST);
    vm_write(output + 8, SECOND);
    vm_protect(demand + 3 * PAGE_SIZE, 1, PROT_READ);
    require(
        syscall(SYS_TLS_GET, output, TLS_BASES_SIZE as u64, 0) == EFAULT,
        442,
    );
    require(
        vm_read(output) == FIRST && vm_read(output + 8) == SECOND,
        443,
    );
    vm_release(demand + 3 * PAGE_SIZE, 1);
    let before = vm_stats();
    require(
        syscall(SYS_TLS_GET, output, TLS_BASES_SIZE as u64, 0) == EFAULT,
        444,
    );
    require(vm_stats() == before && vm_read(output) == FIRST, 445);
    check_words(pid);

    set(fs, gs);
    check_words(pid);
    vm_release(demand, 3);
    require(vm_stats() == initial, 446);
}

pub(super) fn finish(console: u64, pid: u64) {
    // FPU's assembly probe has forced timer preemption, blocking, ordinary
    // syscalls, direct page faults, and deferred checked copies in this interval.
    require(bases() == canary_bases(pid) && selectors() == [0, 0], 447);
    check_words(pid);
    set(0, 0);
    write(console, b"Mirror init: TLS checks passed\n");
}
