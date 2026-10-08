#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// Relink when the package-specific script changes, without a host build script.
const _: &str = include_str!("../linker.ld");

const SYS_WRITE: u64 = 1;
const SYS_TICKS: u64 = 2;
const SYS_EXIT: u64 = 3;
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const ENOSYS: i64 = -38;
const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;
const CODE_BASE: u64 = 0x0040_0000;
const STACK_GUARD: u64 = 0x7ffe_f000;

// RDI is the boot-selected probe number. The entry trampoline only establishes
// the System V stack alignment needed to call the Rust application.
global_asm!(
    ".section .text.entry,\"ax\"",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "xor rbp, rbp",
    "and rsp, -16",
    "call init_main",
    "ud2",
    ".size _start, .-_start",
    ".previous",
);

unsafe extern "C" {
    static __readonly_end: u8;
}

static mut INITIALIZED: u64 = 0x0123_4567_89ab_cdef;
static mut ZERO_INITIALIZED: u64 = 0;

/// The first native Mirror ABI preserves all registers except the result in RAX.
/// Integer addresses let the kernel reject invalid pointers without constructing
/// invalid Rust references in the application.
fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    // SAFETY: vector 0x80 is the kernel-provided userspace entry. Its handler
    // restores the user stack and every general-purpose register except RAX.
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") number => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            options(nostack),
        );
    }
    result
}

fn write(message: &[u8]) {
    if syscall(SYS_WRITE, message.as_ptr() as u64, message.len() as u64, 0) != message.len() as i64
    {
        exit(10);
    }
}

fn exit(status: u64) -> ! {
    syscall(SYS_EXIT, status, 0, 0);
    // An exit returning is an ABI failure. UD2 makes that visible to the kernel.
    unsafe { asm!("ud2", options(noreturn, nostack)) }
}

fn require(condition: bool, status: u64) {
    if !condition {
        exit(status);
    }
}

fn check_syscall_registers() {
    let mut rcx = 0x11u64;
    let mut rdi = 0x22u64;
    let mut rsi = 0x33u64;
    let mut rdx = 0x44u64;
    let mut r8 = 0x55u64;
    let mut r9 = 0x66u64;
    let mut r10 = 0x77u64;
    let mut r11 = 0x88u64;
    let mut r12 = 0x99u64;
    let mut r13 = 0xaau64;
    let mut r14 = 0xbbu64;
    let mut r15 = 0xccu64;
    // Declare outputs here so a broken syscall implementation can be detected
    // without violating the compiler's register-preservation assumptions.
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_TICKS => _,
            inout("rcx") rcx,
            inout("rdi") rdi,
            inout("rsi") rsi,
            inout("rdx") rdx,
            inout("r8") r8,
            inout("r9") r9,
            inout("r10") r10,
            inout("r11") r11,
            inout("r12") r12,
            inout("r13") r13,
            inout("r14") r14,
            inout("r15") r15,
            options(nostack),
        );
    }
    require(
        rcx == 0x11
            && rdi == 0x22
            && rsi == 0x33
            && rdx == 0x44
            && r8 == 0x55
            && r9 == 0x66
            && r10 == 0x77
            && r11 == 0x88
            && r12 == 0x99
            && r13 == 0xaa
            && r14 == 0xbb
            && r15 == 0xcc,
        28,
    );
}

#[unsafe(no_mangle)]
extern "C" fn init_main(probe: u64) -> ! {
    let code_segment: u16;
    // SAFETY: reading CS is permitted at every privilege level.
    unsafe {
        asm!("mov {selector:x}, cs", selector = out(reg) code_segment, options(nomem, nostack))
    };
    require(code_segment & 3 == 3, 11);

    write(b"Mirror init: running Rust in ring 3\n");

    // These volatile accesses ensure both file-backed data and zero-filled BSS
    // are exercised even in optimized builds.
    unsafe {
        require(
            (&raw const INITIALIZED).read_volatile() == 0x0123_4567_89ab_cdef,
            12,
        );
        require((&raw const ZERO_INITIALIZED).read_volatile() == 0, 13);
        (&raw mut INITIALIZED).write_volatile(0xfedc_ba98_7654_3210);
        (&raw mut ZERO_INITIALIZED).write_volatile(0x1122_3344_5566_7788);
        require(
            (&raw const INITIALIZED).read_volatile() == 0xfedc_ba98_7654_3210,
            14,
        );
        require(
            (&raw const ZERO_INITIALIZED).read_volatile() == 0x1122_3344_5566_7788,
            15,
        );
    }

    let mut stack_value = 0u64;
    // SAFETY: this local remains live and writable for both accesses.
    unsafe {
        (&raw mut stack_value).write_volatile(0xcafe_babe_0123_4567);
        require(
            (&raw const stack_value).read_volatile() == 0xcafe_babe_0123_4567,
            16,
        );
    }

    require(syscall(SYS_WRITE, KERNEL_BASE, 1, 0) == EFAULT, 20);
    require(syscall(SYS_WRITE, 0, 1, 0) == EFAULT, 21);
    require(syscall(SYS_WRITE, u64::MAX - 1, 4, 0) == EFAULT, 22);
    let readonly_end = (&raw const __readonly_end) as u64;
    require(syscall(SYS_WRITE, readonly_end - 1, 2, 0) == EFAULT, 23);
    require(syscall(u64::MAX, 0, 0, 0) == ENOSYS, 24);
    require(syscall(SYS_WRITE, 0, 0, 0) == 0, 29);
    require(syscall(SYS_WRITE, CODE_BASE, 257, 0) == EINVAL, 32);
    check_syscall_registers();

    let initial_ticks = syscall(SYS_TICKS, 0, 0, 0);
    require(initial_ticks >= 0, 25);
    let mut previous_ticks = initial_ticks;
    let mut timer_progressed = false;
    for _ in 0..1_000_000 {
        let ticks = syscall(SYS_TICKS, 0, 0, 0);
        require(ticks >= previous_ticks, 26);
        if ticks > initial_ticks {
            timer_progressed = true;
            break;
        }
        previous_ticks = ticks;
        core::hint::spin_loop();
    }
    require(timer_progressed, 27);

    write(b"Mirror init: userspace checks passed\n");

    // Each boot runs at most one destructive protection probe. Assembly avoids
    // expressing deliberately invalid accesses as undefined Rust operations.
    match probe {
        0 => exit(0),
        1 => unsafe {
            asm!("mov rax, [rdi]", in("rdi") KERNEL_BASE, out("rax") _, options(nostack));
        },
        2 => unsafe {
            asm!("mov byte ptr [rdi], 0x90", in("rdi") CODE_BASE, options(nostack));
        },
        3 => unsafe {
            // If execution protection is missing, RET will not give a false
            // success: the kernel must observe a page fault on this stack page.
            asm!("push 0xc3", "jmp rsp", options(noreturn));
        },
        4 => unsafe {
            asm!("mov rax, [rdi]", in("rdi") STACK_GUARD, out("rax") _, options(nostack));
        },
        5 => unsafe { asm!("cli", options(nostack)) },
        6 => unsafe {
            // A syscall can arrive with an unusable user stack. The kernel must
            // reject this return frame before IRET can fault in supervisor mode.
            // No Rust code may run after changing RSP to the noncanonical value.
            asm!(
                "mov rsp, 0x0000800000000000",
                "mov rax, {ticks}",
                "int 0x80",
                "ud2",
                ticks = const SYS_TICKS,
                options(noreturn),
            );
        },
        _ => exit(30),
    }
    // Reaching here means a protection probe unexpectedly succeeded.
    exit(31)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    exit(99)
}
