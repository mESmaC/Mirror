//! The SMP battery (spec 0051, step 5): one process's threads on every CPU
//! that runs userspace, unpinned.
//!
//! Role 6 runs 16 threads. They spin until each CPU has had one, ping-pong
//! futex words in pairs, pass one IPC message each through a channel whose
//! both ends the process holds, and meet at futex barriers between phases.
//! Then half of them exit and the main thread joins them, while the other
//! half keep writing one page, which the main thread then releases: the
//! kernel requires the next write to fault, and keeps the released frame as
//! a canary that no stale translation may write. Role 7 runs 8 threads that
//! keep writing a page the main thread then makes read-only. With probe 1,
//! role 6's threads only spin, for the Panic case. Every blocking call
//! checks a per-thread register and FPU canary on its way back.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use mirror_thread_protocol::{
    MAX_THREADS, SYS_THREAD_CREATE, SYS_WAIT_U32, SYS_WAKE_U32, THREAD_START_SIZE, ThreadStart,
    WAIT_FOREVER,
};
use mirror_vm_protocol::{ARENA_START, PROT_READ};

use crate::{
    EAGAIN, ETIMEDOUT, PAGE_SIZE, PROT_READ_WRITE, SYS_BOOT_HANDLE, SYS_RECV_WAIT, SYS_SEND,
    SYS_TICKS, SYS_VM_PROTECT, SYS_VM_RELEASE, exit, require, syscall, threads, vm_reserve,
    vm_write, write,
};

global_asm!(include_str!("smp.S"));

unsafe extern "C" {
    /// Make a native call with callee-saved registers and XMM8-12 loaded
    /// from `canary`, and exit with status 700 unless every one comes back
    /// unchanged (smp.S).
    fn mirror_smp_syscall(number: u64, arg0: u64, arg1: u64, arg2: u64, canary: u64) -> i64;
}

/// Role 2's threads, its main thread included.
const THREADS: usize = MAX_THREADS;
/// Role 3's threads, its main thread included.
const PROTECT_THREADS: usize = 8;
/// Role 2's threads from this index on exit after the IPC phase and are
/// joined; the ones below it, past the main thread, write the page.
const JOINED: usize = THREADS / 2;
const STACK_PAGES: u64 = 4;
const STACKS: u64 = 0x1_0d00_0000;
/// The page the writers write: the kernel checks the fault there and keeps
/// its frame as the canary (kernel/src/userspace.rs, BATTERY_PAGE).
const PAGE: u64 = ARENA_START + 0x0300_0000;
/// How long the main thread keeps every thread spinning, so that each CPU
/// takes one.
const SPREAD_TICKS: i64 = 8;
const ROUNDS: u32 = 16;
const JOIN_VALUE: u64 = 0x5eed_0051_0000_0000;
const MESSAGE_TAG: u8 = 0x51;
/// Bounds every retry loop.
const LIMIT: u32 = 100_000;

static SPREAD_DONE: AtomicU32 = AtomicU32::new(0);
static SPINS: [AtomicU64; THREADS] = [const { AtomicU64::new(0) }; THREADS];
static ARRIVED: AtomicU32 = AtomicU32::new(0);
static GENERATION: AtomicU32 = AtomicU32::new(0);
static PINGPONG: [AtomicU32; THREADS / 2] = [const { AtomicU32::new(0) }; THREADS / 2];
static RECEIVED: AtomicU32 = AtomicU32::new(0);
static WRITES: [AtomicU64; THREADS] = [const { AtomicU64::new(0) }; THREADS];
static SEND: AtomicU64 = AtomicU64::new(0);
static RECEIVE: AtomicU64 = AtomicU64::new(0);
static SPIN_ONLY: AtomicU32 = AtomicU32::new(0);

/// Thread `index`'s canary: distinct per thread, so a context restored into
/// the wrong thread fails too.
fn canary(index: usize) -> u64 {
    0x0051_c0de_5eed_0000 ^ ((index as u64) << 40) ^ ((index as u64) << 4)
}

/// A native call through the canary check.
fn checked(number: u64, arg0: u64, arg1: u64, arg2: u64, index: usize) -> i64 {
    // SAFETY: The assembly makes one native call with these arguments and
    // restores every callee-saved register; it touches no memory but its
    // own stack.
    unsafe { mirror_smp_syscall(number, arg0, arg1, arg2, canary(index)) }
}

fn ticks() -> i64 {
    let ticks = syscall(SYS_TICKS, 0, 0, 0);
    require(ticks >= 0, 730);
    ticks
}

fn start(entry: extern "C" fn(u64) -> !, index: usize) -> ThreadStart {
    let top = STACKS + (index as u64 + 1) * STACK_PAGES * PAGE_SIZE;
    ThreadStart {
        entry: entry as *const () as u64,
        stack: top - 8,
        argument: index as u64,
        fs: 0,
        gs: 0,
    }
}

fn create(entry: extern "C" fn(u64) -> !, index: usize) -> u64 {
    let bytes = start(entry, index).encode();
    let tid = syscall(
        SYS_THREAD_CREATE,
        bytes.as_ptr() as u64,
        THREAD_START_SIZE as u64,
        0,
    );
    require(tid > 0, 731);
    tid as u64
}

/// Run init's SMP battery role `role` (6 or 7) as the main thread.
pub(super) fn battery(console: u64, role: u64, probe: u64) -> ! {
    let threads = if role == 6 { THREADS } else { PROTECT_THREADS };
    vm_reserve(STACKS, threads as u64 * STACK_PAGES, PROT_READ_WRITE);
    // Commit the page before any writer starts, so its frame is resident.
    vm_reserve(PAGE, 1, PROT_READ_WRITE);
    vm_write(PAGE, 0);
    if role == 7 {
        protect(console);
    }
    SPIN_ONLY.store(u32::from(probe == 1), Ordering::Release);
    for (slot, handle) in [(1, &SEND), (2, &RECEIVE)] {
        let value = syscall(SYS_BOOT_HANDLE, slot, 0, 0);
        require(value > 0, 732);
        handle.store(value as u64, Ordering::Release);
    }
    let mut tids = [0; THREADS];
    for (index, tid) in tids.iter_mut().enumerate().skip(1) {
        *tid = create(worker, index);
    }
    spread(0);
    barrier(0);
    pingpong(0);
    barrier(0);
    write(console, b"Mirror init: SMP futex checks passed\n");
    exchange(0);
    barrier(0);
    require(RECEIVED.load(Ordering::Acquire) == (1 << THREADS) - 1, 733);
    write(console, b"Mirror init: SMP IPC checks passed\n");
    for (index, &tid) in tids.iter().enumerate().skip(JOINED) {
        require(threads::join(tid) == (0, JOIN_VALUE ^ index as u64), 734);
    }
    write(console, b"Mirror init: SMP join checks passed\n");
    end_with_fault(console, 1..JOINED, SYS_VM_RELEASE, 0)
}

extern "C" fn worker(index: u64) -> ! {
    let index = index as usize;
    require((1..THREADS).contains(&index), 735);
    spread(index);
    barrier(index);
    pingpong(index);
    barrier(index);
    exchange(index);
    barrier(index);
    if index >= JOINED {
        threads::thread_exit(JOIN_VALUE ^ index as u64);
    }
    write_forever(index)
}

/// Role 3: writers keep writing the page until the main thread makes it
/// read-only; the next write must fault.
fn protect(console: u64) -> ! {
    for index in 1..PROTECT_THREADS {
        create(protect_worker, index);
    }
    end_with_fault(console, 1..PROTECT_THREADS, SYS_VM_PROTECT, PROT_READ)
}

extern "C" fn protect_worker(index: u64) -> ! {
    let index = index as usize;
    require((1..PROTECT_THREADS).contains(&index), 736);
    write_forever(index)
}

/// Every thread spins until the main thread has seen `SPREAD_TICKS` pass,
/// so that the scheduler spreads them over every CPU. With probe 1 none
/// ever stops: the Panic case panics while they run.
fn spread(index: usize) {
    let spin_only = SPIN_ONLY.load(Ordering::Acquire) != 0;
    let begin = if index == 0 { ticks() } else { 0 };
    loop {
        let spins = SPINS[index].fetch_add(1, Ordering::Relaxed);
        if spin_only {
            continue;
        }
        if index == 0 {
            if spins.is_multiple_of(1024) && ticks() - begin >= SPREAD_TICKS {
                SPREAD_DONE.store(1, Ordering::Release);
                return;
            }
        } else if SPREAD_DONE.load(Ordering::Acquire) != 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

/// A futex barrier for all of role 6's threads.
fn barrier(index: usize) {
    let generation = GENERATION.load(Ordering::Acquire);
    if ARRIVED.fetch_add(1, Ordering::AcqRel) + 1 == THREADS as u32 {
        ARRIVED.store(0, Ordering::Relaxed);
        GENERATION.store(generation.wrapping_add(1), Ordering::Release);
        require(
            syscall(SYS_WAKE_U32, GENERATION.as_ptr() as u64, THREADS as u64, 0) >= 0,
            737,
        );
        return;
    }
    for _ in 0..LIMIT {
        let seen = GENERATION.load(Ordering::Acquire);
        if seen != generation {
            return;
        }
        let result = checked(
            SYS_WAIT_U32,
            GENERATION.as_ptr() as u64,
            u64::from(seen),
            WAIT_FOREVER,
            index,
        );
        require(result == 0 || result == EAGAIN, 738);
    }
    exit(739);
}

/// Threads 2p and 2p+1 pass a token through futex word p: each waits for
/// its turn, takes it and wakes the other, `ROUNDS` times.
fn pingpong(index: usize) {
    let word = &PINGPONG[index / 2];
    let parity = (index % 2) as u32;
    for round in 0..ROUNDS {
        let mine = 2 * round + parity;
        for _ in 0..LIMIT {
            let seen = word.load(Ordering::Acquire);
            if seen == mine {
                break;
            }
            require(seen < mine, 740);
            let result = checked(
                SYS_WAIT_U32,
                word.as_ptr() as u64,
                u64::from(seen),
                WAIT_FOREVER,
                index,
            );
            require(result == 0 || result == EAGAIN, 741);
        }
        require(word.load(Ordering::Acquire) == mine, 742);
        word.store(mine + 1, Ordering::Release);
        require(syscall(SYS_WAKE_U32, word.as_ptr() as u64, 1, 0) >= 0, 743);
    }
}

/// Every thread sends one message naming itself through the process's
/// channel, retrying while its four-message queue is full, then receives
/// one; together they must receive every name once.
fn exchange(index: usize) {
    let message = [index as u8, MESSAGE_TAG, 0, 0, 0, 0, 0, 0];
    let send = SEND.load(Ordering::Acquire);
    let mut sent = false;
    for _ in 0..LIMIT {
        let result = syscall(SYS_SEND, send, message.as_ptr() as u64, 8);
        if result == 8 {
            sent = true;
            break;
        }
        require(result == EAGAIN, 744);
        threads::yield_now();
    }
    require(sent, 745);
    let receive = RECEIVE.load(Ordering::Acquire);
    let mut buffer = [0_u8; 8];
    let mut received = false;
    for _ in 0..LIMIT {
        let result = checked(SYS_RECV_WAIT, receive, buffer.as_mut_ptr() as u64, 8, index);
        if result == 8 {
            received = true;
            break;
        }
        require(result == ETIMEDOUT, 746);
    }
    require(received, 747);
    require(
        buffer[1] == MESSAGE_TAG && usize::from(buffer[0]) < THREADS,
        748,
    );
    let bit = 1 << buffer[0];
    require(RECEIVED.fetch_or(bit, Ordering::AcqRel) & bit == 0, 749);
}

/// Keep writing this thread's word of the page until a write faults.
fn write_forever(index: usize) -> ! {
    let word = (PAGE + index as u64 * 8) as *mut u64;
    let mut value = 0_u64;
    loop {
        value += 1;
        // SAFETY: The page is reserved read-write and resident, and each
        // writer owns its word. The write that faults once the main thread
        // releases the page, or makes it read-only, ends the process.
        unsafe { word.write_volatile(value) };
        WRITES[index].store(value, Ordering::Release);
    }
}

/// Wait until every writer in `writers` has written, then make the call
/// that takes the page away from them: the next write must fault, which
/// ends the process with the outcome the kernel checks. The main thread
/// yields meanwhile, so with one CPU a writer runs.
fn end_with_fault(
    console: u64,
    writers: core::ops::Range<usize>,
    number: u64,
    protection: u64,
) -> ! {
    for _ in 0..LIMIT {
        if writers
            .clone()
            .all(|index| WRITES[index].load(Ordering::Acquire) != 0)
        {
            break;
        }
        threads::yield_now();
    }
    require(
        writers
            .clone()
            .all(|index| WRITES[index].load(Ordering::Acquire) != 0),
        750,
    );
    write(console, b"Mirror init: SMP writers running\n");
    // A writer's fault may end the process before this call returns.
    syscall(number, PAGE, PAGE_SIZE, protection);
    for _ in 0..LIMIT {
        threads::yield_now();
    }
    exit(751)
}
