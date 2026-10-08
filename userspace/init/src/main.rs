#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// Relink when the package-specific script changes, without a host build script.
const _: &str = include_str!("../linker.ld");

const SYS_WRITE: u64 = 1;
const SYS_TICKS: u64 = 2;
const SYS_EXIT: u64 = 3;
const SYS_YIELD: u64 = 4;
const SYS_SLEEP: u64 = 5;
const SYS_PID: u64 = 6;
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const ENOSYS: i64 = -38;
const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;
const CODE_BASE: u64 = 0x0040_0000;
const STACK_GUARD: u64 = 0x7ffe_f000;
const DATA_SENTINEL: u64 = 0xfedc_ba98_7654_3210;
const BSS_SENTINEL: u64 = 0x1122_3344_5566_7788;
const STACK_SENTINEL: u64 = 0xcafe_babe_0123_4567;

// RDI packs the probe in its low byte and the process role in the next byte.
// The entry trampoline establishes the System V alignment for the Rust call.
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

fn check_syscall_registers(pid: i64) {
    let result: u64;
    let seed = (pid as u64).rotate_left(32);
    let mut rcx = seed ^ 0x11;
    let mut rdi = seed ^ 0x22;
    let mut rsi = seed ^ 0x33;
    let mut rdx = seed ^ 0x44;
    let mut r8 = seed ^ 0x55;
    let mut r9 = seed ^ 0x66;
    let mut r10 = seed ^ 0x77;
    let mut r11 = seed ^ 0x88;
    let mut r12 = seed ^ 0x99;
    let mut r13 = seed ^ 0xaa;
    let mut r14 = seed ^ 0xbb;
    let mut r15 = seed ^ 0xcc;
    // Declare outputs so broken register restoration can be detected without
    // compiler assumptions hiding the problem. RBX/RBP are reserved by LLVM;
    // save, test, and restore them inside the assembly instead. Capture DF after
    // rescheduling and clear it before any Rust code resumes.
    unsafe {
        asm!(
            "push rbx",
            "push rbp",
            "push rsp",
            "mov rbx, r8",
            "xor rbx, 0x88",
            "mov rbp, r8",
            "xor rbp, 0xbb",
            "std",
            "int 0x80",
            "pushfq",
            "cld",
            "test rax, rax",
            "jnz 2f",
            "xor rbx, 0x88",
            "cmp rbx, r8",
            "jne 2f",
            "xor rbp, 0xbb",
            "cmp rbp, r8",
            "jne 2f",
            "test qword ptr [rsp], 0x400",
            "jz 2f",
            "lea rax, [rsp + 16]",
            "cmp rax, [rsp + 8]",
            "jne 2f",
            "xor eax, eax",
            "jmp 3f",
            "2:",
            "mov eax, 1",
            "3:",
            "add rsp, 16",
            "pop rbp",
            "pop rbx",
            inout("rax") SYS_YIELD => result,
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
        );
    }
    require(
        result == 0
            && rcx == seed ^ 0x11
            && rdi == seed ^ 0x22
            && rsi == seed ^ 0x33
            && rdx == seed ^ 0x44
            && r8 == seed ^ 0x55
            && r9 == seed ^ 0x66
            && r10 == seed ^ 0x77
            && r11 == seed ^ 0x88
            && r12 == seed ^ 0x99
            && r13 == seed ^ 0xaa
            && r14 == seed ^ 0xbb
            && r15 == seed ^ 0xcc,
        28,
    );
}

fn check_private_state(pid: i64, stack_value: &u64) {
    require(syscall(SYS_PID, 0, 0, 0) == pid, 37);
    // SAFETY: These statics and the borrowed local are live in this process's
    // private address space. Volatile reads force rechecking after each switch.
    unsafe {
        require(
            (&raw const INITIALIZED).read_volatile() == DATA_SENTINEL ^ pid as u64,
            38,
        );
        require(
            (&raw const ZERO_INITIALIZED).read_volatile() == BSS_SENTINEL ^ pid as u64,
            39,
        );
        require(
            core::ptr::read_volatile(stack_value) == STACK_SENTINEL ^ pid as u64,
            40,
        );
    }
}

fn check_preempted_arithmetic(pid: i64) {
    const ITERATIONS: u64 = 1024;
    let first_tick = syscall(SYS_TICKS, 0, 0, 0);
    require(first_tick >= 0, 52);
    let mut previous_tick = first_tick;
    let mut completed = false;
    for block in 0..1_000_000u64 {
        let low_seed = DATA_SENTINEL
            ^ (pid as u64).rotate_left(17)
            ^ block.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let high_seed = BSS_SENTINEL ^ (pid as u64).rotate_left(41) ^ block;
        let sum = low_seed as u128 + (u64::MAX - 4) as u128 * ITERATIONS as u128;
        let expected_high = high_seed.wrapping_add((sum >> 64) as u64);
        let mut low = low_seed;
        let mut high = high_seed;
        let remaining: u64;
        // SAFETY: This finite block touches only declared register outputs.
        // Timer IRQs may land between ADD and ADC, so preserving the carry flag
        // is required in addition to saving RAX/RDX/RCX across preemption.
        unsafe {
            asm!(
                "2:",
                "add rax, -5",
                "adc rdx, 0",
                "dec rcx",
                "jnz 2b",
                inout("rax") low,
                inout("rdx") high,
                inout("rcx") ITERATIONS => remaining,
                options(nomem, nostack),
            );
        }
        require(low == sum as u64, 53);
        require(high == expected_high, 54);
        require(remaining == 0, 55);
        // TICKS only observes time; this phase never voluntarily reschedules.
        let tick = syscall(SYS_TICKS, 0, 0, 0);
        require(tick >= previous_tick, 56);
        previous_tick = tick;
        if tick - first_tick >= 8 {
            completed = true;
            break;
        }
    }
    require(completed, 57);
}

#[unsafe(no_mangle)]
extern "C" fn init_main(launch: u64) -> ! {
    let probe = launch & 0xff;
    let role = launch >> 8;
    require(role <= 1 && probe <= 7 && (role == 0 || probe == 0), 33);
    let code_segment: u16;
    // SAFETY: reading CS is permitted at every privilege level.
    unsafe {
        asm!("mov {selector:x}, cs", selector = out(reg) code_segment, options(nomem, nostack))
    };
    require(code_segment & 3 == 3, 11);

    write(b"Mirror init: running Rust in ring 3\n");
    let pid = syscall(SYS_PID, 0, 0, 0);
    require(pid > 0, 34);
    require(syscall(SYS_PID, 0, 0, 0) == pid, 35);

    // These volatile accesses ensure both file-backed data and zero-filled BSS
    // are exercised even in optimized builds.
    unsafe {
        require(
            (&raw const INITIALIZED).read_volatile() == 0x0123_4567_89ab_cdef,
            12,
        );
        require((&raw const ZERO_INITIALIZED).read_volatile() == 0, 13);
        (&raw mut INITIALIZED).write_volatile(DATA_SENTINEL ^ pid as u64);
        (&raw mut ZERO_INITIALIZED).write_volatile(BSS_SENTINEL ^ pid as u64);
        require(
            (&raw const INITIALIZED).read_volatile() == DATA_SENTINEL ^ pid as u64,
            14,
        );
        require(
            (&raw const ZERO_INITIALIZED).read_volatile() == BSS_SENTINEL ^ pid as u64,
            15,
        );
    }

    let mut stack_value = 0u64;
    // SAFETY: this local remains live and writable for both accesses.
    unsafe {
        (&raw mut stack_value).write_volatile(STACK_SENTINEL ^ pid as u64);
        require(
            (&raw const stack_value).read_volatile() == STACK_SENTINEL ^ pid as u64,
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
    check_syscall_registers(pid);

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

    require(syscall(SYS_SLEEP, 1001, 0, 0) == EINVAL, 45);
    require(syscall(SYS_SLEEP, u64::MAX, 0, 0) == EINVAL, 46);
    require(syscall(SYS_SLEEP, 0, 0, 0) == 0, 47);
    check_private_state(pid, &stack_value);
    for _ in 0..4 {
        require(syscall(SYS_YIELD, 0, 0, 0) == 0, 36);
        check_private_state(pid, &stack_value);
        let before_sleep = syscall(SYS_TICKS, 0, 0, 0);
        require(before_sleep >= 0, 41);
        require(syscall(SYS_SLEEP, 2, 0, 0) == 0, 42);
        let after_sleep = syscall(SYS_TICKS, 0, 0, 0);
        require(after_sleep >= before_sleep, 43);
        require(after_sleep - before_sleep >= 2, 44);
        check_private_state(pid, &stack_value);
    }
    check_preempted_arithmetic(pid);
    check_private_state(pid, &stack_value);
    if role == 1 {
        // Let the primary reach its final probe first. In the CPU-bound case,
        // this peer must wake and finish while timer preemption handles the hog.
        let before_sleep = syscall(SYS_TICKS, 0, 0, 0);
        require(before_sleep >= 0, 48);
        require(syscall(SYS_SLEEP, 4, 0, 0) == 0, 49);
        let after_sleep = syscall(SYS_TICKS, 0, 0, 0);
        require(after_sleep >= before_sleep, 50);
        require(after_sleep - before_sleep >= 4, 51);
        check_private_state(pid, &stack_value);
    }

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
            // reject this return frame against the native address contract.
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
        7 => unsafe {
            // No syscalls or blocking instructions: only timer preemption can
            // let another process run or enforce this process's CPU budget.
            asm!(
                "xor eax, eax",
                "2:",
                "add rax, 1",
                "jmp 2b",
                options(noreturn, nostack),
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
