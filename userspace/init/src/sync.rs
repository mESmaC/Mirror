//! Process-private compare-and-wait, explicit wake, and blocking mutex probes.

use core::arch::asm;
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use mirror_sync::Mutex;
use mirror_thread_protocol::{
    MAX_THREADS, MAX_WAIT_TICKS, SYS_THREAD_CREATE, SYS_THREAD_EXIT, SYS_THREAD_JOIN, SYS_WAIT_U32,
    SYS_WAKE_U32, THREAD_START_SIZE, ThreadStart, USER_ADDRESS_END, WAIT_FOREVER,
};

use crate::{
    EAGAIN, EFAULT, EINVAL, ETIMEDOUT, KERNEL_BASE, PAGE_SIZE, PROT_NONE, PROT_READ,
    PROT_READ_WRITE, SYS_SLEEP, SYS_VM_PROTECT, SYS_VM_RELEASE, SYS_YIELD, exit, require, syscall,
    vm_protect, vm_release, vm_reserve, vm_stats, vm_write, write,
};

const STACKS: u64 = 0x1_0700_0000;
const MEMORY: u64 = 0x1_0800_0000;
const STACK_PAGES: u64 = 16;
const STACK_REGION_PAGES: u64 = STACK_PAGES + 2;
const WAIT: u32 = 0;
const GATED_WAIT: u32 = 1;
const MUTEX: u32 = 2;
const INCREMENTS: u64 = 8;
/// The battery's worker count. It is local, not the ABI limit, so raising
/// MAX_THREADS leaves the battery's semantics and indexing unchanged (spec
/// 0030a, 0030-D4). Wake counts of MAX_THREADS still mean "every waiter".
const WORKERS: usize = 3;

struct Task {
    address: AtomicU64,
    expected: AtomicU32,
    timeout: AtomicU64,
    mode: AtomicU32,
    stage: AtomicU32,
    result: AtomicI64,
}

impl Task {
    const fn new() -> Self {
        Self {
            address: AtomicU64::new(0),
            expected: AtomicU32::new(0),
            timeout: AtomicU64::new(0),
            mode: AtomicU32::new(0),
            stage: AtomicU32::new(0),
            result: AtomicI64::new(0),
        }
    }
}

static TASKS: [Task; WORKERS] = [const { Task::new() }; WORKERS];
static WORDS: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
static GATE: AtomicU32 = AtomicU32::new(0);
static COUNTER: Mutex<u64> = Mutex::new(0);

#[repr(align(4))]
struct ReadonlyWord(u32);
static READONLY: ReadonlyWord = ReadonlyWord(0x1827_3645);

fn address(word: &AtomicU32) -> u64 {
    word as *const AtomicU32 as u64
}

fn wait(address: u64, expected: u64, timeout: u64) -> i64 {
    syscall(SYS_WAIT_U32, address, expected, timeout)
}

fn wake(address: u64, count: u64) -> i64 {
    syscall(SYS_WAKE_U32, address, count, 0)
}

fn yield_now() {
    require(syscall(SYS_YIELD, 0, 0, 0) == 0, 601);
}

fn settle() {
    // A worker publishes stage 1 immediately before entering WAIT. Several
    // complete round-robin passes also cover preemption between those actions.
    for _ in 0..=WORKERS {
        yield_now();
    }
}

fn await_stage(index: usize, expected: u32) {
    for _ in 0..4096 {
        if TASKS[index].stage.load(Ordering::Acquire) == expected {
            return;
        }
        yield_now();
    }
    exit(602);
}

fn await_started(index: usize) {
    for _ in 0..4096 {
        // A finite wait can finish before its creator next runs. Observing its
        // completion still proves that the worker reached the wait operation.
        if TASKS[index].stage.load(Ordering::Acquire) >= 1 {
            return;
        }
        yield_now();
    }
    exit(657);
}

fn launch(index: usize, mode: u32, address: u64, expected: u32, timeout: u64) -> u64 {
    let task = &TASKS[index];
    task.address.store(address, Ordering::Relaxed);
    task.expected.store(expected, Ordering::Relaxed);
    task.timeout.store(timeout, Ordering::Relaxed);
    task.mode.store(mode, Ordering::Relaxed);
    task.result.store(i64::MIN, Ordering::Relaxed);
    task.stage.store(0, Ordering::Release);
    let start = ThreadStart {
        entry: worker as *const () as u64,
        stack: STACKS
            + index as u64 * STACK_REGION_PAGES * PAGE_SIZE
            + (STACK_PAGES + 1) * PAGE_SIZE
            - 8,
        argument: task as *const Task as u64,
        fs: 0,
        gs: 0,
    }
    .encode();
    let tid = syscall(
        SYS_THREAD_CREATE,
        start.as_ptr() as u64,
        THREAD_START_SIZE as u64,
        0,
    );
    require(tid > 0, 603);
    tid as u64
}

fn join(tid: u64, expected: i64) {
    let result: i64;
    let value: u64;
    // SAFETY: JOIN's second return register carries the full thread exit value.
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
    require(result == 0 && value == expected as u64, 604);
}

extern "C" fn worker(argument: u64) -> ! {
    // SAFETY: The creating thread passes a live static Task and does not reuse
    // its stack or Task slot until JOIN has consumed the completed context.
    let task = unsafe { &*(argument as *const Task) };
    let mode = task.mode.load(Ordering::Acquire);
    task.stage.store(1, Ordering::Release);
    let result = if mode == MUTEX {
        for _ in 0..INCREMENTS {
            let mut counter = COUNTER.lock().unwrap();
            let old = *counter;
            // Force contention while the protected non-atomic data is live.
            yield_now();
            *counter = old + 1;
        }
        0
    } else {
        if mode == GATED_WAIT {
            while GATE.load(Ordering::Acquire) == 0 {
                yield_now();
            }
        }
        wait(
            task.address.load(Ordering::Acquire),
            task.expected.load(Ordering::Acquire) as u64,
            task.timeout.load(Ordering::Acquire),
        )
    };
    task.result.store(result, Ordering::Release);
    task.stage.store(2, Ordering::Release);
    syscall(SYS_THREAD_EXIT, result as u64, 0, 0);
    // SAFETY: A successful native thread exit never returns to its entry point.
    unsafe { asm!("ud2", options(noreturn, nostack)) }
}

fn check_arguments() {
    let word = address(&WORDS[0]);
    WORDS[0].store(7, Ordering::Release);
    for offset in [1, 2, 3] {
        require(wait(word + offset, 7, 0) == EINVAL, 605);
        require(wake(word + offset, 0) == EINVAL, 606);
    }
    for pointer in [0, 4092, USER_ADDRESS_END, KERNEL_BASE, u64::MAX - 3] {
        require(wait(pointer, 0, 0) == EFAULT, 607);
        require(wake(pointer, 0) == EFAULT, 608);
        // Structural errors have precedence over address permission failures.
        require(wait(pointer, 1 << 32, 0) == EINVAL, 609);
        require(wake(pointer, MAX_THREADS as u64 + 1) == EINVAL, 610);
    }
    require(wait(u64::MAX, 0, 0) == EINVAL, 611);
    for timeout in [MAX_WAIT_TICKS + 1, WAIT_FOREVER - 1] {
        require(wait(word, 7, timeout) == EINVAL, 612);
    }
    require(syscall(SYS_WAKE_U32, word, 0, 1) == EINVAL, 613);
    require(wait(word, 8, 0) == EAGAIN, 614);
    require(wait(word, 7, 0) == ETIMEDOUT, 615);
    require(
        wake(word, 0) == 0 && wake(word, MAX_THREADS as u64) == 0,
        616,
    );
    require(WORDS[0].load(Ordering::Acquire) == 7, 617);

    let readonly = (&raw const READONLY.0) as u64;
    require(wait(readonly, READONLY.0 as u64, 0) == ETIMEDOUT, 618);
    require(wake(readonly, MAX_THREADS as u64) == 0, 619);
}

fn check_lazy_memory() {
    vm_reserve(MEMORY, 2, PROT_READ_WRITE);
    let before = vm_stats().committed;
    require(wake(MEMORY, 0) == 0, 620);
    require(wake(MEMORY + PAGE_SIZE - 4, MAX_THREADS as u64) == 0, 621);
    require(vm_stats().committed == before, 622);
    // Comparing the word really reads it, even when the expected value differs.
    require(wait(MEMORY, 1, 0) == EAGAIN, 623);
    require(vm_stats().committed == before + PAGE_SIZE, 624);
    require(wait(MEMORY, 0, 0) == ETIMEDOUT, 625);
    vm_protect(MEMORY + PAGE_SIZE, 1, PROT_READ);
    require(wake(MEMORY + PAGE_SIZE, MAX_THREADS as u64) == 0, 626);
    require(vm_stats().committed == before + PAGE_SIZE, 627);
    require(wait(MEMORY + PAGE_SIZE, 1, 0) == EAGAIN, 628);
    require(vm_stats().committed == before + 2 * PAGE_SIZE, 629);
    vm_protect(MEMORY, 1, PROT_NONE);
    require(wait(MEMORY, 0, 0) == EFAULT, 630);
    require(wake(MEMORY, 0) == EFAULT, 631);
    vm_release(MEMORY, 2);
}

fn check_wake() {
    let first = address(&WORDS[0]);
    let second = address(&WORDS[1]);
    WORDS[0].store(17, Ordering::Release);
    WORDS[1].store(19, Ordering::Release);
    let tids = [
        launch(0, WAIT, first, 17, WAIT_FOREVER),
        launch(1, WAIT, first, 17, WAIT_FOREVER),
        launch(2, WAIT, second, 19, WAIT_FOREVER),
    ];
    for index in 0..TASKS.len() {
        await_stage(index, 1);
    }
    settle();
    require(wake(first, 0) == 0, 632);
    require(wake(address(&WORDS[2]), MAX_THREADS as u64) == 0, 633);
    WORDS[0].store(18, Ordering::Release);
    settle();
    // Merely changing the word never wakes a queued waiter.
    require(
        TASKS
            .iter()
            .all(|task| task.stage.load(Ordering::Acquire) == 1),
        634,
    );
    require(wake(first, 1) == 1, 635);
    settle();
    let completed = TASKS[..2]
        .iter()
        .filter(|task| task.stage.load(Ordering::Acquire) == 2)
        .count();
    require(
        completed == 1 && TASKS[2].stage.load(Ordering::Acquire) == 1,
        636,
    );
    require(wake(first, MAX_THREADS as u64) == 1, 637);
    require(wake(second, MAX_THREADS as u64) == 1, 638);
    for tid in tids {
        join(tid, 0);
    }
    require(WORDS[0].load(Ordering::Acquire) == 18, 639);
    require(WORDS[1].load(Ordering::Acquire) == 19, 640);
    require(wake(first, MAX_THREADS as u64) == 0, 641);

    GATE.store(0, Ordering::Release);
    let tid = launch(0, GATED_WAIT, first, 18, WAIT_FOREVER);
    await_stage(0, 1);
    WORDS[0].store(20, Ordering::Release);
    // This wake can arrive before the worker calls WAIT. Its subsequent atomic
    // comparison observes the new value instead of sleeping after a lost wake.
    require(wake(first, 1) == 0, 642);
    GATE.store(1, Ordering::Release);
    join(tid, EAGAIN);
}

fn check_timeout() {
    let word = address(&WORDS[0]);
    let expected = WORDS[0].load(Ordering::Acquire);
    require(wait(word, expected as u64, 1) == ETIMEDOUT, 643);
    let tid = launch(0, WAIT, word, expected, 1);
    await_started(0);
    settle();
    require(syscall(SYS_SLEEP, 3, 0, 0) == 0, 644);
    // A waiter whose relative deadline passed cannot be counted as an explicit
    // wake, whether or not it has had another userspace dispatch yet.
    require(wake(word, MAX_THREADS as u64) == 0, 645);
    join(tid, ETIMEDOUT);
}

fn pending_memory_wait() -> u64 {
    let tid = launch(0, WAIT, MEMORY, 0, WAIT_FOREVER);
    await_stage(0, 1);
    settle();
    tid
}

fn check_invalidation() {
    vm_reserve(MEMORY, 1, PROT_READ_WRITE);
    vm_write(MEMORY, 0);
    let tid = pending_memory_wait();
    // Both mutations cover a hole and must fail transactionally. Neither may
    // cancel a waiter attached to the valid first page.
    require(
        syscall(SYS_VM_RELEASE, MEMORY, 2 * PAGE_SIZE, 0) == EFAULT,
        646,
    );
    require(
        syscall(SYS_VM_PROTECT, MEMORY, 2 * PAGE_SIZE, PROT_NONE) == EFAULT,
        647,
    );
    vm_protect(MEMORY, 1, PROT_READ);
    settle();
    require(TASKS[0].stage.load(Ordering::Acquire) == 1, 648);
    require(wake(MEMORY, 1) == 1, 649);
    join(tid, 0);

    let tid = pending_memory_wait();
    vm_protect(MEMORY, 1, PROT_NONE);
    vm_protect(MEMORY, 1, PROT_READ_WRITE);
    require(wake(MEMORY, MAX_THREADS as u64) == 0, 650);
    join(tid, EFAULT);

    let tid = pending_memory_wait();
    vm_release(MEMORY, 1);
    vm_reserve(MEMORY, 1, PROT_READ_WRITE);
    require(wake(MEMORY, MAX_THREADS as u64) == 0, 651);
    join(tid, EFAULT);
    // Reusing the same virtual address gets a fresh wait registration; no old
    // queue entry survives release and accidentally consumes this wake.
    let tid = pending_memory_wait();
    require(wake(MEMORY, 1) == 1, 652);
    join(tid, 0);
    vm_release(MEMORY, 1);
}

fn check_mutex() {
    let mut guard = COUNTER.lock().unwrap();
    require(COUNTER.try_lock().is_none(), 653);
    *guard = 0;
    let tids = [
        launch(0, MUTEX, 0, 0, 0),
        launch(1, MUTEX, 0, 0, 0),
        launch(2, MUTEX, 0, 0, 0),
    ];
    for index in 0..TASKS.len() {
        await_stage(index, 1);
    }
    settle();
    require(
        TASKS
            .iter()
            .all(|task| task.stage.load(Ordering::Acquire) == 1),
        654,
    );
    drop(guard);
    for tid in tids {
        join(tid, 0);
    }
    let guard = COUNTER.try_lock().unwrap();
    require(*guard == INCREMENTS * TASKS.len() as u64, 655);
}

pub(super) fn check(console: u64) {
    let baseline = vm_stats();
    for index in 0..TASKS.len() {
        let base = STACKS + index as u64 * STACK_REGION_PAGES * PAGE_SIZE;
        vm_reserve(base, STACK_REGION_PAGES, PROT_NONE);
        vm_protect(base + PAGE_SIZE, STACK_PAGES, PROT_READ_WRITE);
    }
    check_arguments();
    check_lazy_memory();
    check_wake();
    check_timeout();
    check_invalidation();
    check_mutex();
    for index in 0..TASKS.len() {
        vm_release(
            STACKS + index as u64 * STACK_REGION_PAGES * PAGE_SIZE,
            STACK_REGION_PAGES,
        );
    }
    require(vm_stats() == baseline, 656);
    write(console, b"Mirror init: synchronization checks passed\n");
}
