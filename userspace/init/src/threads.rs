//! Native thread probes share a process address space and capabilities while
//! keeping independent stacks, TLS bases, general registers and FPU contexts.

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use mirror_thread_protocol::{
    MAX_THREADS, SYS_THREAD_CREATE, SYS_THREAD_EXIT, SYS_THREAD_ID, SYS_THREAD_JOIN, SYS_TLS_GET,
    THREAD_START_SIZE, TLS_BASES_SIZE, ThreadStart, USER_ADDRESS_END,
};

use crate::{
    EAGAIN, EBADF, EFAULT, EINVAL, KERNEL_BASE, PAGE_SIZE, PROT_NONE, PROT_READ_WRITE, RIGHT_WRITE,
    SYS_BOOT_HANDLE, SYS_CLOSE, SYS_DUP, SYS_PID, SYS_SLEEP, SYS_WRITE, SYS_YIELD, exit, require,
    syscall, vm_protect, vm_release, vm_reserve, vm_stats, vm_write, write,
};

global_asm!(include_str!("threads.S"));

const ESRCH: i64 = -3;
const EBUSY: i64 = -16;
const EDEADLK: i64 = -35;
const STACK_PAGES: u64 = 16;
const STACK_REGION_PAGES: u64 = STACK_PAGES + 2;
const STACKS: u64 = 0x1_0500_0000;
const DESCRIPTOR: u64 = 0x1_0600_0000;
const EXIT_VALUE: u64 = 0xfedc_ba98_7654_3200;
const BASIC: u64 = 0;
const WAIT: u64 = 1;
const JOIN: u64 = 2;
const CYCLE: u64 = 3;
const PARK: u64 = 4;
const BURN: u64 = 5;
const MAIN_EXIT: u64 = 18;
const PROCESS_EXIT: u64 = 19;
const FAULT: u64 = 20;

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct FxState([u8; 512]);

#[repr(C, align(16))]
struct Task {
    initial: UnsafeCell<FxState>,
    index: AtomicU64,
    stack: AtomicU64,
    fs: AtomicU64,
    gs: AtomicU64,
    mode: AtomicU64,
    target: AtomicU64,
    progress: AtomicU64,
    tid: AtomicU64,
    value: AtomicU64,
}

// SAFETY: The entry assembly writes `initial` once before entering Rust, and
// only that thread reads it. A slot is reconfigured only after successful join.
// Every field that other live threads can access is atomic.
unsafe impl Sync for Task {}

impl Task {
    const fn new() -> Self {
        Self {
            initial: UnsafeCell::new(FxState([0; 512])),
            index: AtomicU64::new(0),
            stack: AtomicU64::new(0),
            fs: AtomicU64::new(0),
            gs: AtomicU64::new(0),
            mode: AtomicU64::new(0),
            target: AtomicU64::new(0),
            progress: AtomicU64::new(0),
            tid: AtomicU64::new(0),
            value: AtomicU64::new(0),
        }
    }
}

static TASKS: [Task; MAX_THREADS - 1] = [const { Task::new() }; MAX_THREADS - 1];
static TLS_WORDS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
static PROCESS: AtomicU64 = AtomicU64::new(0);
static MAIN: AtomicU64 = AtomicU64::new(0);
static CONSOLE: AtomicU64 = AtomicU64::new(0);
static SHARED_HANDLE: AtomicU64 = AtomicU64::new(0);
static GATE: AtomicU64 = AtomicU64::new(0);
static COMPLETED: AtomicU64 = AtomicU64::new(0);
static PARK_WORD: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" {
    fn mirror_thread_entry();
    fn mirror_thread_fpu(expected: *const FxState, actual: *mut FxState);
}

fn load(value: &AtomicU64) -> u64 {
    value.load(Ordering::Acquire)
}

fn store(value: &AtomicU64, data: u64) {
    value.store(data, Ordering::Release);
}

fn yield_now() {
    require(syscall(SYS_YIELD, 0, 0, 0) == 0, 501);
}

fn await_value(value: &AtomicU64, expected: u64) {
    // BASIC workers sleep and await timer preemption. A count of yields can
    // expire before even one timer tick when only the polling parents remain
    // runnable. Every unsuccessful poll must allow at least one delivered tick
    // without charging the waiting thread's CPU budget.
    for _ in 0..100 {
        if load(value) == expected {
            return;
        }
        require(syscall(SYS_SLEEP, 1, 0, 0) == 0, 574);
    }
    require(load(value) == expected, 502);
}

fn id() -> u64 {
    let tid = syscall(SYS_THREAD_ID, 0, 0, 0);
    require(tid > 0, 503);
    tid as u64
}

fn join(tid: u64) -> (i64, u64) {
    let result: i64;
    let value: u64;
    // SAFETY: JOIN is the one native call with a second return register. All
    // other registers are preserved, including across its blocking interval.
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_THREAD_JOIN => result,
            in("rdi") tid,
            in("rsi") 0u64,
            inout("rdx") 0u64 => value,
            options(nostack),
        );
    }
    (result, value)
}

fn thread_exit(value: u64) -> ! {
    syscall(SYS_THREAD_EXIT, value, 0, 0);
    // SAFETY: Returning from thread exit is an ABI failure.
    unsafe { asm!("ud2", options(noreturn, nostack)) }
}

fn descriptor(index: usize, mode: u64, tls: bool) -> ThreadStart {
    let task = &TASKS[index];
    let stack =
        STACKS + index as u64 * STACK_REGION_PAGES * PAGE_SIZE + (STACK_PAGES + 1) * PAGE_SIZE - 8;
    let fs = if tls {
        (&raw const TLS_WORDS) as u64 + index as u64 * 16
    } else {
        0
    };
    let gs = if tls { fs + 8 } else { 0 };
    TLS_WORDS[index * 2].store(EXIT_VALUE ^ index as u64, Ordering::Relaxed);
    TLS_WORDS[index * 2 + 1].store(!EXIT_VALUE ^ index as u64, Ordering::Relaxed);
    store(&task.index, index as u64);
    store(&task.stack, stack);
    store(&task.fs, fs);
    store(&task.gs, gs);
    store(&task.mode, mode);
    store(&task.target, 0);
    store(&task.progress, 0);
    store(&task.tid, 0);
    store(&task.value, EXIT_VALUE ^ index as u64);
    ThreadStart {
        entry: mirror_thread_entry as *const () as u64,
        stack,
        argument: task as *const Task as u64,
        fs,
        gs,
    }
}

fn create(start: ThreadStart) -> i64 {
    let bytes = start.encode();
    syscall(
        SYS_THREAD_CREATE,
        bytes.as_ptr() as u64,
        THREAD_START_SIZE as u64,
        0,
    )
}

fn launch(index: usize, mode: u64) -> u64 {
    let tid = create(descriptor(index, mode, true));
    require(tid > 0 && tid as u64 != load(&MAIN), 504);
    tid as u64
}

fn allocate_stacks() {
    for index in 0..TASKS.len() {
        let base = STACKS + index as u64 * STACK_REGION_PAGES * PAGE_SIZE;
        vm_reserve(base, STACK_REGION_PAGES, PROT_NONE);
        vm_protect(base + PAGE_SIZE, STACK_PAGES, PROT_READ_WRITE);
    }
}

fn release_stacks() {
    for index in 0..TASKS.len() {
        vm_release(
            STACKS + index as u64 * STACK_REGION_PAGES * PAGE_SIZE,
            STACK_REGION_PAGES,
        );
    }
}

fn check_tls(task: &Task) {
    let mut bytes = [0; TLS_BASES_SIZE];
    require(
        syscall(
            SYS_TLS_GET,
            bytes.as_mut_ptr() as u64,
            TLS_BASES_SIZE as u64,
            0,
        ) == TLS_BASES_SIZE as i64,
        505,
    );
    require(
        u64::from_le_bytes(bytes[..8].try_into().unwrap()) == load(&task.fs),
        506,
    );
    require(
        u64::from_le_bytes(bytes[8..].try_into().unwrap()) == load(&task.gs),
        507,
    );
    if load(&task.fs) != 0 {
        let fs: u64;
        let gs: u64;
        // SAFETY: The creating thread published these live BSS words before
        // creation, and does not change them until this thread is joined.
        unsafe {
            asm!("mov {fs}, qword ptr fs:[0]", "mov {gs}, qword ptr gs:[0]",
                fs = out(reg) fs, gs = out(reg) gs, options(nostack, preserves_flags));
        }
        require(fs == EXIT_VALUE ^ load(&task.index), 508);
        require(gs == !EXIT_VALUE ^ load(&task.index), 509);
    }
}

fn check_fresh(task: &Task) {
    // SAFETY: Our assembly entry completed this snapshot before calling Rust.
    let FxState(bytes) = unsafe { task.initial.get().read() };
    require(bytes[..2] == 0x037f_u16.to_le_bytes(), 510);
    require(bytes[2..5] == [0, 0, 0], 511);
    require(bytes[24..28] == 0x1f80_u32.to_le_bytes(), 512);
    require(bytes[160..416] == [0; 256], 513);
    for register in 0..8 {
        require(
            bytes[32 + register * 16..42 + register * 16] == [0; 10],
            514,
        );
    }
}

fn check_fpu(tid: u64) {
    let mut expected = FxState([0; 512]);
    let mut actual = FxState([0; 512]);
    let rounding = (1 + tid % 2) as u16;
    expected.0[..2].copy_from_slice(&(0x037f_u16 | rounding << 10).to_le_bytes());
    expected.0[24..28].copy_from_slice(&(0x1f80_u32 | u32::from(rounding) << 13).to_le_bytes());
    for register in 0..16 {
        let low = EXIT_VALUE ^ (tid << 16) ^ register as u64;
        let high = !EXIT_VALUE ^ (tid << 32) ^ register as u64;
        expected.0[160 + register * 16..168 + register * 16].copy_from_slice(&low.to_le_bytes());
        expected.0[168 + register * 16..176 + register * 16].copy_from_slice(&high.to_le_bytes());
    }
    // Populate all eight physical x87 registers with distinct exact normals.
    expected.0[4] = 0xff;
    for register in 0..8 {
        let mantissa = 0x8000_0000_0000_0000_u64 | tid << 32 | register as u64;
        expected.0[32 + register * 16..40 + register * 16].copy_from_slice(&mantissa.to_le_bytes());
        expected.0[40 + register * 16..42 + register * 16]
            .copy_from_slice(&0x3fff_u16.to_le_bytes());
    }
    // SAFETY: Both images are aligned and live. The assembly restores valid
    // architectural state, schedules twice, captures it, then resets controls.
    unsafe { mirror_thread_fpu(&expected, &mut actual) };
    require(actual.0[..5] == expected.0[..5], 515);
    require(actual.0[24..28] == expected.0[24..28], 516);
    require(actual.0[160..416] == expected.0[160..416], 517);
    for register in 0..8 {
        require(
            actual.0[32 + register * 16..42 + register * 16]
                == expected.0[32 + register * 16..42 + register * 16],
            518,
        );
    }
}

#[unsafe(no_mangle)]
extern "C" fn mirror_thread_main(task: &'static Task, initial_stack: u64) -> ! {
    check_fresh(task);
    require(
        initial_stack == load(&task.stack) && initial_stack % 16 == 8,
        519,
    );
    let tid = id();
    store(&task.tid, tid);
    require(syscall(SYS_PID, 0, 0, 0) == load(&PROCESS) as i64, 520);
    require(
        syscall(SYS_BOOT_HANDLE, 0, 0, 0) == load(&CONSOLE) as i64,
        521,
    );
    require(join(tid).0 == EDEADLK, 522);
    check_tls(task);
    match load(&task.mode) {
        BASIC => {
            await_value(&GATE, 1);
            let stack_canary = EXIT_VALUE ^ tid;
            // SAFETY: `stack_canary` is a live, initialized and aligned local.
            // Volatile stack data observes accidental sharing of thread stacks.
            unsafe { (&raw const stack_canary).read_volatile() };
            check_fpu(tid);
            check_tls(task);
            require(
                // SAFETY: `stack_canary` is still a live, initialized and aligned local.
                unsafe { (&raw const stack_canary).read_volatile() } == EXIT_VALUE ^ tid,
                523,
            );
            require(syscall(SYS_WRITE, 0, 0, load(&CONSOLE)) == 0, 524);
            if load(&task.index) == 0 && load(&SHARED_HANDLE) != 0 {
                require(syscall(SYS_CLOSE, load(&SHARED_HANDLE), 0, 0) == 0, 525);
                store(&SHARED_HANDLE, 0);
            }
        }
        WAIT => await_value(&GATE, 1),
        PARK => {
            store(&task.progress, 1);
            // A process-wide exit/fault must cancel a thread blocked forever,
            // including its implicit wait-queue membership.
            syscall(
                mirror_thread_protocol::SYS_WAIT_U32,
                PARK_WORD.as_ptr() as u64,
                0,
                mirror_thread_protocol::WAIT_FOREVER,
            );
            exit(569);
        }
        BURN => {
            await_value(&GATE, 1);
            burn();
        }
        JOIN => {
            while load(&task.target) == 0 {
                yield_now();
            }
            store(&task.progress, 1);
            let result = join(load(&task.target));
            require(result == (0, EXIT_VALUE), 526);
        }
        CYCLE => {
            while load(&task.target) == 0 {
                yield_now();
            }
            await_value(&TASKS[0].progress, 1);
            yield_now();
            yield_now();
            require(join(load(&task.target)).0 == EDEADLK, 527);
            // JOIN mode expects this value when its target completes.
            store(&task.value, EXIT_VALUE);
        }
        MAIN_EXIT => {
            require(join(load(&MAIN)) == (0, 0), 528);
            require(load(&GATE) == 0x1820, 529);
            require(syscall(SYS_PID, 0, 0, 0) == load(&PROCESS) as i64, 530);
            write(load(&CONSOLE), b"Mirror init: main thread exit survived\n");
            thread_exit(0);
        }
        PROCESS_EXIT | FAULT => {
            await_value(&GATE, 1);
            if load(&task.mode) == PROCESS_EXIT {
                write(load(&CONSOLE), b"Mirror init: worker process exit\n");
                exit(0);
            }
            write(load(&CONSOLE), b"Mirror init: worker fault\n");
            // SAFETY: This probe intentionally faults to test process teardown.
            unsafe { asm!("ud2", options(noreturn, nostack)) }
        }
        _ => exit(531),
    }
    COMPLETED.fetch_add(1, Ordering::AcqRel);
    store(&task.progress, 2);
    thread_exit(load(&task.value));
}

fn check_validation(start: ThreadStart) {
    let bytes = start.encode();
    let pointer = bytes.as_ptr() as u64;
    for length in [0, 39, 41, u64::MAX] {
        require(syscall(SYS_THREAD_CREATE, 0, length, 0) == EINVAL, 532);
    }
    require(syscall(SYS_THREAD_CREATE, pointer, 40, 1) == EINVAL, 533);
    for pointer in [0, KERNEL_BASE, u64::MAX - 8] {
        require(syscall(SYS_THREAD_CREATE, pointer, 40, 0) == EFAULT, 534);
    }
    for entry in [0, 4095, USER_ADDRESS_END, u64::MAX] {
        require(create(ThreadStart { entry, ..start }) == EINVAL, 535);
    }
    for stack in [0, 4095, 4096, start.stack - 8, USER_ADDRESS_END + 8] {
        require(create(ThreadStart { stack, ..start }) == EINVAL, 536);
    }
    for fs in [USER_ADDRESS_END, KERNEL_BASE, u64::MAX] {
        require(create(ThreadStart { fs, ..start }) == EINVAL, 537);
        require(create(ThreadStart { gs: fs, ..start }) == EINVAL, 538);
    }
    for entry in [STACKS + PAGE_SIZE, (&raw const TASKS) as u64, 0x4000_0000] {
        require(create(ThreadStart { entry, ..start }) == EFAULT, 539);
    }
    for stack in [STACKS + 8, start.entry & !15 | 8, 0x4000_0008] {
        require(create(ThreadStart { stack, ..start }) == EFAULT, 540);
    }
    require(syscall(SYS_THREAD_ID, 1, 0, 0) == EINVAL, 541);
    require(syscall(SYS_THREAD_ID, 0, 1, 0) == EINVAL, 542);
    require(syscall(SYS_THREAD_ID, 0, 0, 1) == EINVAL, 543);
    require(syscall(SYS_THREAD_EXIT, 0, 1, 0) == EINVAL, 544);
    require(syscall(SYS_THREAD_EXIT, 0, 0, 1) == EINVAL, 545);
    require(syscall(SYS_THREAD_JOIN, 0, 1, 0) == EINVAL, 546);
    // RDX is an output register on successful JOIN only, so the generic
    // one-result wrapper remains valid for calls rejected before joining.
    require(syscall(SYS_THREAD_JOIN, 0, 0, 1) == EINVAL, 547);
    require(join(load(&MAIN)).0 == EDEADLK, 548);
    require(join(0).0 == ESRCH && join(u64::MAX).0 == ESRCH, 549);
}

pub(super) fn check(console: u64, pid: u64) {
    let baseline = vm_stats();
    store(&PROCESS, pid);
    store(&MAIN, id());
    store(&CONSOLE, console);
    allocate_stacks();
    check_validation(descriptor(0, BASIC, true));

    let shared = syscall(SYS_DUP, console, RIGHT_WRITE, 0);
    require(shared > 0, 550);
    store(&SHARED_HANDLE, shared as u64);
    store(&GATE, 0);
    let first = launch(0, BASIC);
    let second = launch(1, BASIC);
    let third = launch(2, BASIC);
    require(first != second && first != third && second != third, 551);
    // Do not mutate an active Task merely to provoke the capacity error.
    let overflow = ThreadStart {
        entry: mirror_thread_entry as *const () as u64,
        stack: load(&TASKS[0].stack),
        argument: (&raw const TASKS) as u64,
        fs: 0,
        gs: 0,
    };
    require(create(overflow) == EAGAIN, 552);
    store(&GATE, 1);
    for task in &TASKS {
        await_value(&task.progress, 2);
    }
    // Progress is published immediately before THREAD_EXIT. Give every ready
    // sibling a final turn so this capacity check covers retained exit records.
    for _ in 0..MAX_THREADS {
        yield_now();
    }
    // Exited but unjoined threads continue to occupy their bounded slots.
    require(create(overflow) == EAGAIN, 570);
    for (index, tid) in [first, second, third].into_iter().enumerate() {
        require(join(tid) == (0, EXIT_VALUE ^ index as u64), 553);
        require(join(tid).0 == ESRCH, 554);
    }
    require(load(&COMPLETED) == 3, 555);
    require(syscall(SYS_WRITE, 0, 0, shared as u64) == EBADF, 556);
    require(id() == load(&MAIN), 557);

    // A valid descriptor crosses into an untouched, demand-zero page. Its last
    // two words are zero TLS bases; copying it must materialize that page.
    vm_reserve(DESCRIPTOR, 2, PROT_READ_WRITE);
    let demand = descriptor(0, BASIC, false);
    let pointer = DESCRIPTOR + PAGE_SIZE - 24;
    vm_write(pointer, demand.entry);
    vm_write(pointer + 8, demand.stack);
    vm_write(pointer + 16, demand.argument);
    let before = vm_stats().committed;
    let reused = syscall(SYS_THREAD_CREATE, pointer, 40, 0);
    require(
        reused > 0 && ![first, second, third].contains(&(reused as u64)),
        558,
    );
    require(vm_stats().committed >= before + PAGE_SIZE, 559);
    require(join(reused as u64) == (0, EXIT_VALUE), 560);
    vm_release(DESCRIPTOR, 2);

    // One joiner owns a blocked target; a second join cannot consume it.
    store(&GATE, 0);
    let target = launch(0, WAIT);
    let waiter = launch(1, JOIN);
    store(&TASKS[1].target, target);
    await_value(&TASKS[1].progress, 1);
    for _ in 0..4 {
        yield_now();
    }
    require(join(target).0 == EBUSY, 561);
    store(&GATE, 1);
    require(join(waiter) == (0, EXIT_VALUE ^ 1), 562);
    require(join(target).0 == ESRCH, 563);

    // A -> B is already blocked when B tries B -> A. Cycle rejection allows B
    // to exit, wake A, and let the main thread consume A normally.
    let a = launch(0, JOIN);
    let b = launch(1, CYCLE);
    store(&TASKS[0].target, b);
    store(&TASKS[1].target, a);
    await_value(&TASKS[1].progress, 2);
    require(join(a) == (0, EXIT_VALUE), 564);
    require(join(b).0 == ESRCH, 565);

    release_stacks();
    require(vm_stats() == baseline, 566);
    write(console, b"Mirror init: thread checks passed\n");
}

pub(super) fn scenario(scenario: u64) -> ! {
    allocate_stacks();
    store(&GATE, 0);
    match scenario {
        MAIN_EXIT => {
            launch(0, MAIN_EXIT);
            store(&GATE, 0x1820);
            thread_exit(0);
        }
        PROCESS_EXIT | FAULT => {
            let worker = launch(0, scenario);
            launch(1, PARK);
            await_value(&TASKS[1].progress, 1);
            for _ in 0..MAX_THREADS {
                yield_now();
            }
            store(&GATE, 1);
            if scenario == PROCESS_EXIT {
                syscall(SYS_SLEEP, 1000, 0, 0);
            } else {
                join(worker);
            }
            exit(567);
        }
        _ => exit(568),
    }
}

pub(super) fn current_id() -> u64 {
    id()
}

pub(super) fn check_foreign(tid: u64) {
    require(tid > 0 && tid != id(), 572);
    require(join(tid).0 == ESRCH, 573);
}

fn burn() -> ! {
    // SAFETY: The deliberate CPU hog never accesses memory or leaves ring 3.
    // Timer preemption and the process's shared quota must stop all siblings.
    unsafe {
        asm!(
            "xor eax, eax",
            "2:",
            "add rax, 1",
            "jmp 2b",
            options(noreturn, nostack)
        );
    }
}

pub(super) fn budget() -> ! {
    allocate_stacks();
    store(&GATE, 0);
    launch(0, BURN);
    launch(1, BURN);
    store(&GATE, 1);
    burn();
}
