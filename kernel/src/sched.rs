//! Kernel-owned process and thread tables and the run queue (spec 0030a).
//!
//! Processes and threads live in fixed-capacity `.bss` tables, not in the
//! stack frames of per-session loops. One FIFO run queue holds every ready
//! thread. `run` dispatches threads, not processes: it takes the first queued
//! thread whose process its CPU policy allows to run, and it idles in HLT when
//! none may. Sessions stay sequential, and each ends by its own rule.
//!
//! Only the BSP touches any of this, with IF=0: the scheduler between user
//! entries, and the running thread's synchronous syscall and CPL3 IRQ handlers
//! while the scheduler waits inside `resume_user`. CPL0 IRQ handlers and APs
//! never do. Entries are reached through raw slot pointers, never through a
//! reference to a whole table. A thread entry belongs to its process: code
//! reaches it through the process (`Process::member`), except for the
//! scheduler's short reads while it holds no process reference.
//!
//! This is a child module of `userspace`, so it shares the private process,
//! thread and syscall types without widening their visibility.

use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{
    ACTIVE, ACTIVE_INPUT, ACTIVE_IPC, ACTIVE_SURFACE, ACTIVE_WINDOWS, CpuPolicy, Outcome, Process,
    ShellRuntime, State, Thread, release_handles, resume_process, thread_abi, vm_abi, wake_threads,
};
use crate::{
    arch, input, interrupts, ipc::Ipc, memory::FrameAllocator, paging::PageTables, serial, stacks,
    surface, timer,
};

/// Process-table capacity (0030-D4).
pub(super) const PROCESS_SLOTS: usize = 64;
/// Thread-table capacity across all processes (0030-D4).
pub(super) const THREAD_SLOTS: usize = 256;
/// Marks an empty member index in a process's thread list.
pub(super) const NO_SLOT: u16 = u16::MAX;

/// Fixed-capacity slots that start zeroed, so a static table lands in .bss
/// and costs no image bytes. An entry is initialized exactly while its
/// occupancy bit is set.
struct Table<T, const N: usize, const W: usize> {
    entries: UnsafeCell<[MaybeUninit<T>; N]>,
    occupied: UnsafeCell<[u64; W]>,
}

// SAFETY: Only the BSP touches a table, with IF=0, and only from code that
// never runs concurrently with itself: the scheduler between user entries, or
// the running thread's synchronous handlers while the scheduler waits. Every
// access goes through a raw slot pointer; no reference to a whole table exists.
unsafe impl<T, const N: usize, const W: usize> Sync for Table<T, N, W> {}

impl<T, const N: usize, const W: usize> Table<T, N, W> {
    const fn new() -> Self {
        assert!(N == W * 64 && N <= NO_SLOT as usize);
        Self {
            entries: UnsafeCell::new([const { MaybeUninit::zeroed() }; N]),
            occupied: UnsafeCell::new([0; W]),
        }
    }

    fn word(&self, index: usize) -> *mut u64 {
        assert!(index < N, "table slot out of range");
        self.occupied.get().cast::<u64>().wrapping_add(index / 64)
    }

    fn is_occupied(&self, index: usize) -> bool {
        let word = self.word(index);
        // SAFETY: The word is in range, and the bitmap is only ever read or
        // written through such momentary raw accesses on the BSP with IF=0.
        unsafe { word.read() & (1 << (index % 64)) != 0 }
    }

    fn count(&self) -> usize {
        (0..N).filter(|&index| self.is_occupied(index)).count()
    }

    /// The occupied entry at `index`.
    fn entry(&self, index: usize) -> *mut T {
        assert!(self.is_occupied(index), "vacant table slot");
        self.entries.get().cast::<T>().wrapping_add(index)
    }

    /// Initialize the lowest vacant slot with `make(slot)`. Callers check
    /// for room first, so a full table here is a kernel bug.
    fn insert_with(&self, make: impl FnOnce(usize) -> T) -> usize {
        let index = (0..N)
            .find(|&index| !self.is_occupied(index))
            .expect("table full after its room check");
        let value = make(index);
        let word = self.word(index);
        // SAFETY: The slot is vacant, so nothing references it. The write
        // initializes it before its bit publishes it to other accessors.
        unsafe {
            self.entries.get().cast::<T>().add(index).write(value);
            word.write(word.read() | 1 << (index % 64));
        }
        index
    }

    /// Move the entry at `index` out and vacate its slot.
    ///
    /// # Safety
    /// The slot is occupied and no reference to its entry is live.
    unsafe fn remove(&self, index: usize) -> T {
        let entry = self.entry(index);
        let word = self.word(index);
        // SAFETY: The caller guarantees an occupied, unreferenced entry. The
        // bit clears after the read, so no accessor sees a moved-out value.
        unsafe {
            let value = entry.read();
            word.write(word.read() & !(1 << (index % 64)));
            value
        }
    }
}

/// State that only one scheduler operation uses at a time. The flag turns an
/// accidental reentrant use into a panic instead of aliased references.
struct Exclusive<T> {
    busy: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: Only the BSP uses it, with IF=0, and `with` refuses reentry, so at
// most one mutable reference to the value exists at any time.
unsafe impl<T> Sync for Exclusive<T> {}

impl<T> Exclusive<T> {
    const fn new(value: T) -> Self {
        Self {
            busy: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> R {
        assert!(!arch::interrupts_enabled());
        assert!(
            !self.busy.swap(true, Ordering::Acquire),
            "reentrant scheduler state"
        );
        // SAFETY: The busy flag proves this is the only live reference; the
        // closure cannot reach this cell again without panicking above.
        let result = operation(unsafe { &mut *self.value.get() });
        self.busy.store(false, Ordering::Release);
        result
    }
}

/// Ready threads in FIFO order, as an intrusive list of thread slots.
struct RunQueue {
    head: u16,
    tail: u16,
    length: u16,
    next: [u16; THREAD_SLOTS],
    queued: [bool; THREAD_SLOTS],
}

static PROCESSES: Table<Process, PROCESS_SLOTS, { PROCESS_SLOTS / 64 }> = Table::new();
static THREADS: Table<Thread, THREAD_SLOTS, { THREAD_SLOTS / 64 }> = Table::new();
static RUN_QUEUE: Exclusive<RunQueue> = Exclusive::new(RunQueue {
    head: 0,
    tail: 0,
    length: 0,
    next: [0; THREAD_SLOTS],
    queued: [false; THREAD_SLOTS],
});
static REPORTED: AtomicBool = AtomicBool::new(false);

/// Print `MIRROR_SCHED_READY` once, before the first session, in every
/// flavor. The tables are statics, so setup only checks that they are empty.
pub(super) fn report_ready() {
    assert!(
        !REPORTED.swap(true, Ordering::SeqCst),
        "scheduler reported twice"
    );
    assert_eq!(PROCESSES.count(), 0);
    assert_eq!(THREADS.count(), 0);
    assert_eq!(RUN_QUEUE.with(|queue| queue.length), 0);
    serial::println!(
        "MIRROR_SCHED_READY processes={} threads={} max_threads={} regions={}",
        PROCESS_SLOTS,
        THREAD_SLOTS,
        thread_abi::MAX_THREADS,
        vm_abi::MAX_REGIONS
    );
}

/// Whether both tables can take this many more entries.
pub(super) fn has_room(processes: usize, threads: usize) -> bool {
    PROCESS_SLOTS - PROCESSES.count() >= processes && THREAD_SLOTS - THREADS.count() >= threads
}

/// Store a new process and return its slot. Check `has_room` first.
pub(super) fn insert_process(process: Process) -> usize {
    PROCESSES.insert_with(|_| process)
}

/// Store the thread that `make` builds for its slot, and return the slot.
/// The thread is not queued; `enqueue` it once it is ready to run.
pub(super) fn insert_thread(make: impl FnOnce(u16) -> Thread) -> u16 {
    // Table::new bounds the capacity below NO_SLOT, so every slot fits.
    THREADS.insert_with(|slot| make(slot as u16)) as u16
}

/// A process entry.
///
/// # Safety
/// `slot` is occupied. No other reference to that entry is live, and none is
/// created while the returned one is in use: in particular, not while a
/// handler's `with_active` borrow of the same process is live.
pub(super) unsafe fn process<'a>(slot: usize) -> &'a mut Process {
    // SAFETY: The caller guarantees an occupied slot without aliases.
    unsafe { &mut *PROCESSES.entry(slot) }
}

/// The raw entry of thread `slot`, which must be occupied. Only the owning
/// process's member accessors and the scheduler dereference it.
pub(super) fn thread_entry(slot: u16) -> *mut Thread {
    THREADS.entry(usize::from(slot))
}

/// Remove a thread from the table and the run queue. A thread owns no
/// resource beyond its entry, so it is simply dropped.
///
/// # Safety
/// `slot` is occupied and no reference to its entry is live. Its owner drops
/// the slot from its member list.
pub(super) unsafe fn remove_thread(slot: u16) {
    unqueue(slot);
    // SAFETY: The caller guarantees an occupied, unreferenced entry.
    let _thread = unsafe { THREADS.remove(usize::from(slot)) };
}

/// Move a process and all its threads out of the tables. The caller owns
/// the returned process, whose address space it must destroy.
///
/// # Safety
/// `slot` is occupied, and no reference to the process or its threads is live.
unsafe fn remove_process(slot: usize) -> Process {
    // SAFETY: The caller guarantees an occupied, unreferenced entry.
    let process = unsafe { PROCESSES.remove(slot) };
    for &member in process.members.iter().filter(|&&member| member != NO_SLOT) {
        // SAFETY: Members are occupied slots owned by this process, which no
        // longer has a table entry through which anything could reach them.
        unsafe { remove_thread(member) };
    }
    process
}

/// Take a finished process out of the tables once its session has validated
/// it. Every remaining thread was finished with it.
///
/// # Safety
/// As for `remove_process`.
pub(super) unsafe fn reap(slot: usize) -> Process {
    {
        // SAFETY: The caller guarantees an occupied, unaliased slot; this
        // borrow ends before the entry moves.
        let process = unsafe { process(slot) };
        assert!(process.finished(), "reaped a live process");
        assert!(
            process
                .threads()
                .all(|thread| thread.state == State::Finished),
            "reaped a process with an unfinished thread"
        );
    }
    // SAFETY: The caller's guarantee, and the borrow above has ended.
    unsafe { remove_process(slot) }
}

/// Roll back a process that has never run, after its creation failed partway.
///
/// # Safety
/// As for `remove_process`.
pub(super) unsafe fn discard(slot: usize) -> Process {
    {
        // SAFETY: As in reap.
        let process = unsafe { process(slot) };
        assert_eq!(process.dispatches, 0, "discarded a process that ran");
    }
    // SAFETY: The caller's guarantee, and the borrow above has ended.
    unsafe { remove_process(slot) }
}

/// Occupied process slots, in slot order.
pub(super) fn process_slots() -> impl Iterator<Item = usize> {
    (0..PROCESS_SLOTS).filter(|&slot| PROCESSES.is_occupied(slot))
}

/// Append a ready thread to the run queue.
pub(super) fn enqueue(slot: u16) {
    let index = usize::from(slot);
    RUN_QUEUE.with(|queue| {
        assert!(!queue.queued[index], "thread queued twice");
        if queue.length == 0 {
            queue.head = slot;
        } else {
            queue.next[usize::from(queue.tail)] = slot;
        }
        queue.tail = slot;
        queue.queued[index] = true;
        queue.length += 1;
    });
}

/// Take a thread off the run queue if it is on it.
pub(super) fn unqueue(slot: u16) {
    RUN_QUEUE.with(|queue| {
        if !queue.queued[usize::from(slot)] {
            return;
        }
        let mut previous = None;
        let mut current = queue.head;
        for _ in 0..queue.length {
            if current == slot {
                unlink(queue, previous, current);
                return;
            }
            previous = Some(current);
            current = queue.next[usize::from(current)];
        }
        unreachable!("queued thread missing from the run queue");
    });
}

fn unlink(queue: &mut RunQueue, previous: Option<u16>, slot: u16) {
    let next = queue.next[usize::from(slot)];
    match previous {
        Some(previous) => queue.next[usize::from(previous)] = next,
        None => queue.head = next,
    }
    if queue.tail == slot {
        queue.tail = previous.unwrap_or(0);
    }
    queue.queued[usize::from(slot)] = false;
    queue.length -= 1;
}

/// Dequeue the first thread for which `runnable` holds, scanning at most the
/// whole queue (THREAD_SLOTS entries).
fn dequeue_first(mut runnable: impl FnMut(u16) -> bool) -> Option<u16> {
    RUN_QUEUE.with(|queue| {
        let mut previous = None;
        let mut current = queue.head;
        for _ in 0..queue.length {
            if runnable(current) {
                unlink(queue, previous, current);
                return Some(current);
            }
            previous = Some(current);
            current = queue.next[usize::from(current)];
        }
        None
    })
}

/// How one sequential session ends.
#[derive(Clone, Copy)]
pub(super) enum End {
    /// Init's smoke pair ends when both processes finish. Only `first` may
    /// exhaust its budget, in scenario 7, and only after `second` exited.
    #[cfg(feature = "boot-smoke")]
    Pair {
        first: usize,
        second: usize,
        scenario: u64,
    },
    /// The shell session ends when the shell finishes; its caller then
    /// closes the live apps.
    Shell { shell: usize },
}

/// Everything one session shares, owned by its seeding function. Dispatch
/// publishes the parts a process may use, by its `Access`.
pub(super) struct Session<'a, 'fb> {
    pub(super) ipc: Ipc,
    pub(super) surface: Option<surface::Session<'a, 'fb>>,
    pub(super) input: Option<input::Session<'a>>,
    pub(super) runtime: Option<ShellRuntime<'a>>,
    pub(super) end: End,
}

impl Session<'_, '_> {
    fn ended(&self) -> bool {
        match self.end {
            #[cfg(feature = "boot-smoke")]
            End::Pair { .. } => process_slots().all(|slot| {
                // SAFETY: The scheduler holds no other process reference.
                unsafe { process(slot) }.finished()
            }),
            // SAFETY: As above.
            End::Shell { shell } => unsafe { process(shell) }.finished(),
        }
    }
}

/// Scheduling counts for `MIRROR_SCHEDULER_OK`.
#[derive(Clone, Copy, Default)]
pub(super) struct Stats {
    /// Dispatches that changed process.
    pub(super) switches: usize,
    pub(super) idle_ticks: u64,
}

/// Run one session until its end rule holds. On return no thread runs, no
/// session pointer is published, and the session's processes, finished or
/// not, stay in the table for the caller to drain.
pub(super) fn run(
    session: &mut Session<'_, '_>,
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
) -> Stats {
    assert!(!arch::interrupts_enabled());
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    // SAFETY: IF=0 and no user context is active. The BSP's guarded RSP0
    // stack is mapped in the shared upper half of every process root.
    unsafe { interrupts::set_kernel_stack(stacks::bsp_rsp0_top()) };
    let mut stats = Stats::default();
    let mut last = None;
    loop {
        assert!(!arch::interrupts_enabled());
        assert_unpublished();
        // A finished process's endpoints close before any peer's receive is
        // checked, so a blocked peer observes closure (readme, IPC).
        for slot in process_slots() {
            // SAFETY: The scheduler holds no other process reference.
            release_handles(unsafe { process(slot) }, &mut session.ipc);
        }
        if let Some(runtime) = &mut session.runtime {
            runtime.reclaim_finished(frames, &mut session.ipc);
        }
        if session.ended() {
            return stats;
        }
        let now = timer::ticks();
        wake(session, now);
        match session.end {
            #[cfg(feature = "boot-smoke")]
            End::Pair { .. } => {}
            End::Shell { shell } => complete_shell_requests(session, frames, shell),
        }
        let Some(slot) = select(now) else {
            idle(&mut stats);
            continue;
        };
        // SAFETY: The thread is occupied and nothing references it here.
        let owner = usize::from(unsafe { (*thread_entry(slot)).process });
        if last != Some(owner) {
            stats.switches += 1;
            last = Some(owner);
        }
        dispatch(session, kernel, frames, owner, slot);
        #[cfg(feature = "boot-smoke")]
        if let End::Pair {
            first,
            second,
            scenario,
        } = session.end
        {
            check_budget(owner, first, second, scenario);
        }
    }
}

fn assert_unpublished() {
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_IPC.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_SURFACE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_INPUT.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_WINDOWS.load(Ordering::SeqCst).is_null());
}

/// Complete joins and poll every blocked thread's wake condition, process by
/// process in slot order. Wait queues replace this poll in 0030a step 3.
fn wake(session: &mut Session<'_, '_>, now: u64) {
    for slot in process_slots() {
        // SAFETY: The scheduler holds no other process reference.
        let process = unsafe { process(slot) };
        if process.finished() {
            continue;
        }
        let input = session.input.as_mut().filter(|_| process.access.input);
        let windows = session
            .runtime
            .as_mut()
            .filter(|_| process.access.windows)
            .map(|runtime| &mut runtime.windows);
        wake_threads(process, &mut session.ipc, input, windows, now);
    }
}

/// Run the shell's deferred launch and close calls, which create and reap
/// table entries and so cannot run inside its syscall.
fn complete_shell_requests(
    session: &mut Session<'_, '_>,
    frames: &mut FrameAllocator<'_>,
    shell_slot: usize,
) {
    let runtime = session.runtime.as_mut().expect("shell runtime");
    // SAFETY: The scheduler holds no other reference to the shell. Launch and
    // close reach only other slots: the apps they create or reap.
    let shell = unsafe { process(shell_slot) };
    if shell.finished() {
        return;
    }
    for index in 0..thread_abi::MAX_THREADS {
        let Some(thread) = shell.member(index) else {
            continue;
        };
        let state = thread.state;
        shell.current = index;
        let result = match state {
            State::Launching(kind) => runtime
                .launch(frames, shell, kind)
                .unwrap_or_else(|error| error as u64),
            State::Closing(pid) => runtime
                .close(frames, shell, &mut session.ipc, pid, Outcome::Terminated)
                .map_or_else(|error| error as u64, |()| 0),
            _ => continue,
        };
        let thread = shell.thread_mut();
        thread.context.frame.rax = result;
        thread.wake();
    }
}

/// Dequeue the first ready thread whose process may run now. The CPU policy
/// stays the gate, so a throttled service never resumes.
fn select(now: u64) -> Option<u16> {
    dequeue_first(|slot| {
        // SAFETY: Queued slots are occupied, and the scheduler holds no
        // reference to this thread or its process; both borrows end here.
        let (state, owner) = unsafe {
            let thread = &*thread_entry(slot);
            (thread.state, usize::from(thread.process))
        };
        assert_eq!(state, State::Ready, "queued thread is not ready");
        // SAFETY: As above.
        let process = unsafe { process(owner) };
        assert!(!process.finished(), "finished process left a queued thread");
        match &mut process.policy {
            CpuPolicy::Probe => true,
            CpuPolicy::Shell(policy) | CpuPolicy::Application(policy) => policy.eligible(now),
        }
    })
}

fn dispatch(
    session: &mut Session<'_, '_>,
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    owner: usize,
    slot: u16,
) {
    // SAFETY: The thread's owner is occupied, and the scheduler holds no other
    // reference to it. resume_process publishes it only across user entry.
    let process = unsafe { process(owner) };
    process.current = process
        .members
        .iter()
        .position(|&member| member == slot)
        .expect("queued thread has no owner");
    let access = process.access;
    resume_process(
        kernel,
        frames,
        process,
        &mut session.ipc,
        session.surface.as_mut().filter(|_| access.surface),
        session.input.as_mut().filter(|_| access.input),
        session
            .runtime
            .as_mut()
            .filter(|_| access.windows)
            .map(|runtime| &mut runtime.windows),
    );
    // A preempted, yielding or completed thread rejoins the queue's tail.
    if process.thread().state == State::Ready {
        enqueue(slot);
    }
}

/// Halt until an interrupt when no queued thread may run. Some thread must
/// be waiting on time or input, or be throttled, or nothing could wake.
fn idle(stats: &mut Stats) {
    assert!(
        process_slots().any(|slot| {
            // SAFETY: The scheduler holds no other process reference.
            let process = unsafe { process(slot) };
            !process.finished()
                && process.threads().any(|thread| {
                    matches!(
                        thread.state,
                        State::Ready
                            | State::Sleeping(_)
                            | State::Receiving(_)
                            | State::InputWaiting(_)
                            | State::Waiting(_)
                    )
                })
        }),
        "every live thread is blocked without a waker"
    );
    let charged = || -> u64 {
        process_slots()
            // SAFETY: As above.
            .map(|slot| unsafe { process(slot) }.charged_ticks)
            .sum()
    };
    let before_charge = charged();
    let before = timer::ticks();
    // SAFETY: No user context, published pointer or lock is live, and the
    // kernel root is active. STI's interrupt shadow closes the check-to-HLT
    // wakeup race; a CPL0 IRQ only acknowledges and counts, and CLI restores
    // IF=0 before any scheduler state is touched again.
    unsafe { core::arch::asm!("sti", "hlt", "cli", options(nostack)) };
    stats.idle_ticks += timer::ticks().wrapping_sub(before);
    assert_eq!(charged(), before_charge, "idle charged user CPU ticks");
}

/// Scenario 7's CPU hog may exhaust only role 0's budget, and only after its
/// peer exited: preemption must keep the peer running meanwhile.
#[cfg(feature = "boot-smoke")]
fn check_budget(owner: usize, first: usize, second: usize, scenario: u64) {
    // SAFETY: The scheduler holds no other process reference.
    if unsafe { process(owner) }.outcome != Some(Outcome::BudgetExhausted) {
        return;
    }
    assert_eq!(owner, first, "peer exhausted its budget");
    assert_eq!(scenario, 7, "unexpected budget exhaustion");
    assert_eq!(
        // SAFETY: As above; this borrow ends before the next.
        unsafe { process(second) }.outcome,
        Some(Outcome::Exit(0)),
        "CPU-bound task starved peer"
    );
    assert!(
        // SAFETY: As above.
        unsafe { process(first) }.post_checks_preemptions > 0,
        "CPU-only loop was never preempted"
    );
}
