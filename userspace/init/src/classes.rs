//! The scheduler-class battery's loads (spec 0052). The kernel sets each
//! process's class, times its sleeps and ends its spinners; the threads only
//! spin or sleep, and print nothing.
//!
//! Role 8 spins on every thread until the kernel ends the process. Role 9's
//! threads each run `ROUNDS` rounds: sleep until the next multiple of
//! `PERIOD` ticks, then spin until `SPIN_TICKS` ticks after waking, so that
//! all of them want a CPU at once. Its main thread then joins the others and
//! exits with status 0. The probe byte is the thread count, the main thread
//! included, from 1 to `MAX_THREADS`.

use mirror_thread_protocol::{MAX_THREADS, SYS_THREAD_CREATE, THREAD_START_SIZE, ThreadStart};

use crate::{
    PAGE_SIZE, PROT_READ_WRITE, SYS_SLEEP, SYS_TICKS, exit, require, syscall, threads, vm_reserve,
};

const STACKS: u64 = 0x1_0e00_0000;
const STACK_PAGES: u64 = 4;
/// The rounds of each role 9 thread; the kernel expects this many timed
/// sleeps from each (kernel/src/class_battery.rs, ROUNDS).
const ROUNDS: u64 = 12;
const PERIOD: i64 = 4;
const SPIN_TICKS: i64 = 2;
const JOIN_VALUE: u64 = 0x5eed_0052_0000_0000;

fn ticks() -> i64 {
    let ticks = syscall(SYS_TICKS, 0, 0, 0);
    require(ticks >= 0, 760);
    ticks
}

/// Start thread `index` at `entry` on its own stack.
fn create(entry: extern "C" fn(u64) -> !, index: u64) -> u64 {
    let start = ThreadStart {
        entry: entry as *const () as u64,
        stack: STACKS + (index + 1) * STACK_PAGES * PAGE_SIZE - 8,
        argument: index,
        fs: 0,
        gs: 0,
    };
    let bytes = start.encode();
    let tid = syscall(
        SYS_THREAD_CREATE,
        bytes.as_ptr() as u64,
        THREAD_START_SIZE as u64,
        0,
    );
    require(tid > 0, 761);
    tid as u64
}

/// Reserve every thread's stack, after checking the count.
fn reserve_stacks(count: u64) {
    require((1..=MAX_THREADS as u64).contains(&count), 762);
    vm_reserve(STACKS, count * STACK_PAGES, PROT_READ_WRITE);
}

/// Role 8: `count` threads spin until the kernel ends the process.
pub(super) fn spinners(count: u64) -> ! {
    reserve_stacks(count);
    for index in 1..count {
        create(spin, index);
    }
    spin(0)
}

extern "C" fn spin(_index: u64) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// Role 9: `count` threads run their rounds, then the main thread joins the
/// others and exits.
pub(super) fn sleepers(count: u64) -> ! {
    reserve_stacks(count);
    let mut tids = [0; MAX_THREADS];
    for index in 1..count {
        tids[index as usize] = create(sleeper, index);
    }
    rounds();
    for index in 1..count {
        require(
            threads::join(tids[index as usize]) == (0, JOIN_VALUE ^ index),
            763,
        );
    }
    exit(0)
}

extern "C" fn sleeper(index: u64) -> ! {
    rounds();
    threads::thread_exit(JOIN_VALUE ^ index)
}

/// Sleep to the next multiple of `PERIOD` ticks, then spin until
/// `SPIN_TICKS` ticks after waking, `ROUNDS` times. The tick count bounds
/// each spin.
fn rounds() {
    for _ in 0..ROUNDS {
        let now = ticks();
        require(
            syscall(SYS_SLEEP, (PERIOD - now % PERIOD) as u64, 0, 0) == 0,
            764,
        );
        let woke = ticks();
        while ticks() < woke + SPIN_TICKS {
            for _ in 0..256 {
                core::hint::spin_loop();
            }
        }
    }
}
