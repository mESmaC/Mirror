//! Kernel-owned process and thread tables, the run queue and the wait queues
//! (spec 0030a).
//!
//! Processes and threads live in fixed-capacity `.bss` tables, not in the
//! stack frames of per-session loops. One FIFO run queue holds every ready
//! thread. `run` dispatches threads, not processes: it takes the first queued
//! thread whose process its CPU policy allows to run, and it idles in HLT when
//! none may. Sessions stay sequential, and each ends by its own rule.
//!
//! A blocked thread sits on the FIFO queue of the event that completes it
//! (its process's address waits, an IPC channel, device input or one app's
//! routed input) and, when its wait is timed, on one deadline heap; a joiner
//! waits on its target's `joiner` field instead (0030-D2). An event only
//! signals waiters, in queue order (0030-D6). The scheduler completes the
//! signaled waits, with any user copy, before its next dispatch.
//!
//! A finished process stays in the process table as a zombie. Once the
//! scheduler regains control it releases the process's thread entries; the
//! zombie keeps its outcome, counters and address space until its session
//! validates and reaps it, which destroys the address space (0030-D5).
//!
//! A signal also asks for a reschedule at the next trap exit: the syscall
//! that signaled, the tick that reached a deadline, or a PS/2 IRQ that
//! completed input while a thread waits for it. The interrupted thread then
//! returns to the scheduler still ready, keeping the rest of its quantum.
//! Each completion pass appends the threads it wakes to the run queue, and
//! the next dispatch takes the first eligible one. The interrupted thread
//! rejoins at the head of the queue, so it resumes right after that woken
//! thread, or first when none is eligible, ahead of every entry queued before
//! it ran (0030-D3).
//!
//! One lock, `SCHED`, covers all of it: the tables, the run queue, the wait
//! queues, every process and thread, and the published session (spec 0051,
//! point 1). It is an IRQ-off ticket spinlock. A CPU holds it for its whole
//! scheduler pass and for the whole of a CPL3 handler, from the handler's
//! first process access until its frame returns, so a check and the park
//! that depends on it never split. Session setup and teardown hold it too.
//! It is released across user entry, HLT, heap growth and the stack-pool
//! trim, and never held across a wait for another CPU's acknowledgement.
//! CPL0 IRQ handlers never take it. Lock order is `SCHED`, then serial.
//!
//! Each CPU names the thread it runs, or whose wait or request its pass
//! completes, in its `CpuData`; handlers find their process and thread there.
//! Entries are reached through raw slot pointers, never through a reference
//! to a whole table. A thread entry belongs to its process: code reaches it
//! through the process (`Process::member`), except for the scheduler's short
//! reads while it holds no process reference.
//!
//! Every CPU in `smp::userspace_mask` runs user threads (spec 0051, point
//! 5). The BSP runs `run`, which owns the session: its loop top tears
//! processes down, and its pass alone reaches deadlines, drains device input
//! and completes the requests that need the frame allocator, the heap or the
//! kernel VA window, which an AP hands it on a work list. An AP joins an open
//! session from its idle loop (`run_ap`), runs and completes threads whose
//! process's CPU mask allows it, and leaves at the session's end barrier. A
//! CPU whose pass leaves work that an idle CPU could take kicks that CPU.
//!
//! This is a child module of `userspace`, so it shares the private process,
//! thread and syscall types without widening their visibility.

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering},
};

use super::{
    CpuPolicy, MemoryClass, Outcome, Process, ShellRuntime, State, Thread, complete_wait,
    deadline_reached, finish_slice, release_handles, resume_on_ap, resume_process, thread_abi,
    vm_abi,
};
#[cfg(feature = "boot-smoke")]
use crate::elf;
use crate::{
    arch, clock, cpudata, heap, input, interrupts,
    ipc::{self, Ipc},
    memory::FrameAllocator,
    paging::PageTables,
    serial, smp, surface, timer, windows,
};

/// Process-table capacity (0030-D4).
pub(super) const PROCESS_SLOTS: usize = 64;
/// Thread-table capacity across all processes (0030-D4).
pub(super) const THREAD_SLOTS: usize = 256;
/// Marks an empty member index in a process's thread list.
pub(super) const NO_SLOT: u16 = u16::MAX;
/// How long a CPU waits for `SCHED` before it panics (spec 0051, point 1).
/// Holders never wait for another CPU, so a longer hold is a stuck holder.
const LOCK_WAIT_NS: u64 = 1_000_000_000;
/// `SchedLock::owner` while no CPU holds the lock.
const NO_OWNER: u32 = u32::MAX;
/// How long the BSP waits at a session's end for every AP to leave it (spec
/// 0051, point 9). A kicked AP returns its thread at its next trap and never
/// waits for the BSP on the way out, so a longer wait is a stuck AP.
const BARRIER_WAIT_NS: u64 = 1_000_000_000;
/// `PHASE`: no session is published.
const CLOSED: u8 = 0;
/// `PHASE`: a session is published, and an AP that runs userspace joins it.
const OPEN: u8 = 1;
/// `PHASE`: the session's end rule holds; no AP joins, and each AP leaves.
const CLOSING: u8 = 2;

/// The scheduler lock (spec 0051, point 1): a ticket spinlock taken with IF
/// clear, which records the slot of the CPU that holds it.
struct SchedLock {
    next: AtomicU32,
    serving: AtomicU32,
    owner: AtomicU32,
}

static SCHED: SchedLock = SchedLock {
    next: AtomicU32::new(0),
    serving: AtomicU32::new(0),
    owner: AtomicU32::new(NO_OWNER),
};

impl SchedLock {
    /// Take the lock, spinning with IF clear for at most `LOCK_WAIT_NS`.
    fn acquire(&self) {
        assert!(!arch::interrupts_enabled(), "scheduler lock requires IF=0");
        let me = cpudata::current_cpu() as u32;
        assert_ne!(
            self.owner.load(Ordering::Relaxed),
            me,
            "reentrant scheduler lock"
        );
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        if self.serving.load(Ordering::Acquire) != ticket {
            let mut bound = clock::SpinBound::new(LOCK_WAIT_NS);
            while self.serving.load(Ordering::Acquire) != ticket {
                if bound.expired() {
                    panic!(
                        "scheduler lock wait exceeded: owner slot {}",
                        self.owner.load(Ordering::Relaxed)
                    );
                }
                core::hint::spin_loop();
            }
        }
        self.owner.store(me, Ordering::Relaxed);
        cpudata::set_sched_held(true);
    }

    /// Release the lock, which this CPU holds.
    fn release(&self) {
        assert!(self.held_here(), "released an unheld scheduler lock");
        cpudata::set_sched_held(false);
        self.owner.store(NO_OWNER, Ordering::Relaxed);
        // Only the holder advances `serving`; Release publishes its writes to
        // the next holder's Acquire.
        self.serving.fetch_add(1, Ordering::Release);
    }

    /// Whether this CPU holds the lock. Only this CPU stores its own slot in
    /// `owner`, and it clears it before releasing, so the answer is exact.
    fn held_here(&self) -> bool {
        self.owner.load(Ordering::Relaxed) == cpudata::current_cpu() as u32
    }
}

/// Proof that this CPU holds `SCHED`, which it releases when dropped.
pub(super) struct SchedGuard(PhantomData<*mut ()>);

impl Drop for SchedGuard {
    fn drop(&mut self) {
        SCHED.release();
    }
}

/// Take `SCHED` for session setup, a run or teardown.
pub(super) fn lock() -> SchedGuard {
    SCHED.acquire();
    SchedGuard(PhantomData)
}

/// Run `operation` with `SCHED` released, retaking it afterwards: for user
/// entry, HLT, heap growth and the stack-pool trim, which may wait for other
/// CPUs or for a long time. This CPU holds it on entry.
pub(super) fn unlocked<R>(operation: impl FnOnce() -> R) -> R {
    SCHED.release();
    let result = operation();
    SCHED.acquire();
    result
}

/// Take `SCHED` and never release it: the holder in ApPanic and SchedStall
/// (spec 0051, step 4), an AP that then panics or spins until the crash
/// stops it.
#[cfg(feature = "boot-smoke")]
pub(crate) fn hold_scheduler_lock_for_test() {
    SCHED.acquire();
}

/// Whether another CPU has taken a ticket for `SCHED` and spins for it
/// while this CPU holds it (ApPanic).
#[cfg(feature = "boot-smoke")]
pub(crate) fn scheduler_lock_contended_for_test() -> bool {
    assert!(SCHED.held_here(), "contention check without the lock");
    SCHED
        .next
        .load(Ordering::Relaxed)
        .wrapping_sub(SCHED.serving.load(Ordering::Relaxed))
        >= 2
}

/// Spin for `SCHED`, as every CPU does, and release it at once: the BSP's
/// half of ApPanic and SchedStall, where an AP holds it for good.
#[cfg(feature = "boot-smoke")]
pub(crate) fn wait_for_scheduler_lock_for_test() {
    drop(lock());
}

/// Panic unless this CPU holds `SCHED`.
fn assert_held() {
    assert!(SCHED.held_here(), "scheduler state without the lock");
}

/// Take `SCHED` for a CPL3 handler on its first process access, unless this
/// handler already holds it. `end_trap` releases it.
pub(super) fn hold_for_trap() {
    assert!(
        interrupts::user_context_active(),
        "trap hold outside a trap"
    );
    if !SCHED.held_here() {
        SCHED.acquire();
    }
}

/// Release a CPL3 handler's hold, if it took one, as its frame returns. The
/// scheduler on this CPU is suspended in user entry, where it holds nothing.
pub(super) fn end_trap() {
    assert!(interrupts::user_context_active(), "trap end outside a trap");
    if SCHED.held_here() {
        SCHED.release();
    }
}

/// Fixed-capacity slots that start zeroed, so a static table lands in .bss
/// and costs no image bytes. An entry is initialized exactly while its
/// occupancy bit is set.
struct Table<T, const N: usize, const W: usize> {
    entries: UnsafeCell<[MaybeUninit<T>; N]>,
    occupied: UnsafeCell<[u64; W]>,
}

// SAFETY: Every access holds SCHED: `word` and `entry`, through which all of
// them go, assert it, so one CPU at a time touches a table. Every access goes
// through a raw slot pointer; no reference to a whole table exists.
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
        assert_held();
        assert!(index < N, "table slot out of range");
        self.occupied.get().cast::<u64>().wrapping_add(index / 64)
    }

    fn is_occupied(&self, index: usize) -> bool {
        let word = self.word(index);
        // SAFETY: The word is in range, and the bitmap is only ever read or
        // written through such momentary raw accesses under SCHED.
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

/// Scheduler state that `SCHED` protects (spec 0051, point 1). The busy flag
/// still turns an accidental reentrant use on the holding CPU into a panic
/// instead of aliased references.
struct Locked<T> {
    busy: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: `with` runs only on the CPU that holds SCHED, and refuses reentry
// there, so at most one mutable reference to the value exists at any time.
unsafe impl<T> Sync for Locked<T> {}

impl<T> Locked<T> {
    const fn new(value: T) -> Self {
        Self {
            busy: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> R {
        assert!(!arch::interrupts_enabled());
        assert_held();
        assert!(
            !self.busy.swap(true, Ordering::Acquire),
            "reentrant scheduler state"
        );
        // SAFETY: SCHED excludes other CPUs, and the busy flag proves this is
        // the only live reference on this one; the closure cannot reach this
        // cell again without panicking above.
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
    /// The scheduler's current completion pass, counted from 1.
    pass: u64,
    /// The pass that queued each thread as just woken, or 0. Dispatch takes
    /// the current pass's threads first (0030-D3).
    woken_in: [u64; THREAD_SLOTS],
}

static PROCESSES: Table<Process, PROCESS_SLOTS, { PROCESS_SLOTS / 64 }> = Table::new();
static THREADS: Table<Thread, THREAD_SLOTS, { THREAD_SLOTS / 64 }> = Table::new();
static RUN_QUEUE: Locked<RunQueue> = Locked::new(RunQueue {
    head: 0,
    tail: 0,
    length: 0,
    next: [0; THREAD_SLOTS],
    queued: [false; THREAD_SLOTS],
    pass: 0,
    woken_in: [0; THREAD_SLOTS],
});
static WAITS: Locked<Waits> = Locked::new(Waits::new());
static BSP_WORK: Locked<BspWork> = Locked::new(BspWork {
    list: List::EMPTY,
    links: Links::new(),
    listed: [false; THREAD_SLOTS],
});
static REPORTED: AtomicBool = AtomicBool::new(false);
/// Where the session is in its life (`CLOSED`, `OPEN`, `CLOSING`). Changed
/// only by the BSP under SCHED, so a check under SCHED is exact.
static PHASE: AtomicU8 = AtomicU8::new(CLOSED);
/// The APs in the open session's loop, as a mask of slots. An AP sets its bit
/// under SCHED as it joins and clears it once it has left and released SCHED.
static PARTICIPANTS: AtomicU64 = AtomicU64::new(0);
/// The kernel tables the published session borrows, which the APs' root
/// switches restore (spec 0051, point 5). Valid while the session is.
static KERNEL: AtomicPtr<PageTables> = AtomicPtr::new(core::ptr::null_mut());
/// The running session, which `run` publishes for its whole run (spec 0051,
/// point 1). Handlers and deferred completions reach the session objects
/// through `with_session`, under SCHED.
static SESSION: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// Set while `with_session` lends the session out, to refuse reentry.
static SESSION_BUSY: AtomicBool = AtomicBool::new(false);

/// The event queue a blocked thread waits on (0030-D2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Queue {
    /// An address wait. Each process has one queue for all its addresses;
    /// a wake selects that address's entries, which keep their FIFO order.
    Futex { process: u16, address: u64 },
    /// A receive on an IPC channel, by channel index.
    Channel(usize),
    /// A wait for device input, in the session that owns the input lease.
    Input,
    /// A wait for one app's routed input, by window index.
    App(usize),
}

const CHANNELS: usize = ipc::CHANNEL_COUNT;
const APPS: usize = windows::APP_COUNT;
/// Futex queues by process slot, then channels, device input and apps.
const LISTS: usize = PROCESS_SLOTS + CHANNELS + 1 + APPS;
/// The device-input queue's list index.
const INPUT_LIST: usize = PROCESS_SLOTS + CHANNELS;

impl Queue {
    fn list(self) -> usize {
        match self {
            Self::Futex { process, .. } => {
                assert!(usize::from(process) < PROCESS_SLOTS);
                usize::from(process)
            }
            Self::Channel(index) => {
                assert!(index < CHANNELS, "invalid channel queue");
                PROCESS_SLOTS + index
            }
            Self::Input => INPUT_LIST,
            Self::App(index) => {
                assert!(index < APPS, "invalid app queue");
                PROCESS_SLOTS + CHANNELS + 1 + index
            }
        }
    }
}

/// A FIFO of thread slots linked through a `Links` array. Its fields mean
/// nothing while it is empty, so the all-zero value is the empty list.
#[derive(Clone, Copy)]
struct List {
    head: u16,
    tail: u16,
    length: u16,
}

impl List {
    const EMPTY: Self = Self {
        head: 0,
        tail: 0,
        length: 0,
    };
}

/// Doubly linked list links by thread slot. A slot's links are valid only
/// while it is on one of the lists that this array links.
struct Links {
    next: [u16; THREAD_SLOTS],
    previous: [u16; THREAD_SLOTS],
}

impl Links {
    const fn new() -> Self {
        Self {
            next: [0; THREAD_SLOTS],
            previous: [0; THREAD_SLOTS],
        }
    }

    fn push(&mut self, list: &mut List, slot: u16) {
        if list.length == 0 {
            list.head = slot;
        } else {
            self.next[usize::from(list.tail)] = slot;
            self.previous[usize::from(slot)] = list.tail;
        }
        list.tail = slot;
        list.length = list.length.checked_add(1).expect("list overflow");
    }

    fn remove(&mut self, list: &mut List, slot: u16) {
        let index = usize::from(slot);
        assert_ne!(list.length, 0, "removed from an empty list");
        if list.head == slot {
            list.head = self.next[index];
        } else {
            self.next[usize::from(self.previous[index])] = self.next[index];
        }
        if list.tail == slot {
            list.tail = self.previous[index];
        } else {
            self.previous[usize::from(self.next[index])] = self.previous[index];
        }
        list.length -= 1;
    }

    /// The list's slots in FIFO order; at most `length` steps.
    fn slots(&self, list: List) -> impl Iterator<Item = u16> + '_ {
        let mut current = list.head;
        (0..list.length).map(move |_| {
            let slot = current;
            current = self.next[usize::from(slot)];
            slot
        })
    }
}

/// Timed waits by deadline, a binary min-heap of thread slots. Equal
/// deadlines expire in the order they were set, so expiry is deterministic.
struct Deadlines {
    heap: [u16; THREAD_SLOTS],
    length: u16,
    /// Each queued slot's index in `heap`.
    position: [u16; THREAD_SLOTS],
    queued: [bool; THREAD_SLOTS],
    deadline: [u64; THREAD_SLOTS],
    order: [u64; THREAD_SLOTS],
    next_order: u64,
}

impl Deadlines {
    const fn new() -> Self {
        Self {
            heap: [0; THREAD_SLOTS],
            length: 0,
            position: [0; THREAD_SLOTS],
            queued: [false; THREAD_SLOTS],
            deadline: [0; THREAD_SLOTS],
            order: [0; THREAD_SLOTS],
            next_order: 0,
        }
    }

    /// Deadlines are tick counts within 1000 ticks of now, so the wrapping
    /// difference orders them, as `deadline_reached` compares them.
    fn earlier(&self, first: u16, second: u16) -> bool {
        let (first, second) = (usize::from(first), usize::from(second));
        match self.deadline[first].wrapping_sub(self.deadline[second]) {
            0 => self.order[first] < self.order[second],
            difference => difference >= 1 << 63,
        }
    }

    fn place(&mut self, index: usize, slot: u16) {
        self.heap[index] = slot;
        self.position[usize::from(slot)] = index as u16;
    }

    fn sift_up(&mut self, mut index: usize) {
        let slot = self.heap[index];
        while index > 0 {
            let parent = (index - 1) / 2;
            if !self.earlier(slot, self.heap[parent]) {
                break;
            }
            self.place(index, self.heap[parent]);
            index = parent;
        }
        self.place(index, slot);
    }

    fn sift_down(&mut self, mut index: usize) {
        let slot = self.heap[index];
        let length = usize::from(self.length);
        loop {
            let left = 2 * index + 1;
            if left >= length {
                break;
            }
            let right = left + 1;
            let child = if right < length && self.earlier(self.heap[right], self.heap[left]) {
                right
            } else {
                left
            };
            if !self.earlier(self.heap[child], slot) {
                break;
            }
            self.place(index, self.heap[child]);
            index = child;
        }
        self.place(index, slot);
    }

    fn insert(&mut self, slot: u16, deadline: u64) {
        let index = usize::from(slot);
        assert!(!self.queued[index], "deadline set twice");
        self.queued[index] = true;
        self.deadline[index] = deadline;
        self.order[index] = self.next_order;
        self.next_order += 1;
        let end = usize::from(self.length);
        self.length += 1;
        self.place(end, slot);
        self.sift_up(end);
    }

    fn remove(&mut self, slot: u16) {
        assert!(self.queued[usize::from(slot)], "removed an unset deadline");
        self.queued[usize::from(slot)] = false;
        let index = usize::from(self.position[usize::from(slot)]);
        self.length -= 1;
        let last = usize::from(self.length);
        if index != last {
            self.place(index, self.heap[last]);
            self.sift_down(index);
            self.sift_up(index);
        }
    }

    fn first(&self) -> Option<u16> {
        (self.length != 0).then_some(self.heap[0])
    }
}

/// Waits that an event or an expired deadline signaled, in signal order,
/// for the scheduler's next completion pass. A preset result, when an event
/// already decided the outcome, completes the wait without a recheck.
struct Signals {
    list: List,
    links: Links,
    signaled: [bool; THREAD_SLOTS],
    has_preset: [bool; THREAD_SLOTS],
    preset: [i64; THREAD_SLOTS],
    /// The tick of each signal: for a syscall's event, its trap-exit tick,
    /// since no tick is delivered while the handler runs with IF=0.
    at: [u64; THREAD_SLOTS],
    /// The CPU that signaled each wait, for the SMP battery's count of wakes
    /// across CPUs (boot-smoke).
    #[cfg(feature = "boot-smoke")]
    from: [u8; THREAD_SLOTS],
}

/// One signaled wait, taken for completion.
#[derive(Clone, Copy)]
pub(super) struct Signal {
    pub(super) slot: u16,
    pub(super) preset: Option<i64>,
    pub(super) at: u64,
    /// The CPU that signaled it (boot-smoke).
    #[cfg(feature = "boot-smoke")]
    pub(super) from: u8,
}

impl Signals {
    const fn new() -> Self {
        Self {
            list: List::EMPTY,
            links: Links::new(),
            signaled: [false; THREAD_SLOTS],
            has_preset: [false; THREAD_SLOTS],
            preset: [0; THREAD_SLOTS],
            at: [0; THREAD_SLOTS],
            #[cfg(feature = "boot-smoke")]
            from: [0; THREAD_SLOTS],
        }
    }

    /// Signal a wait once, and ask this CPU's running syscall, if any, to
    /// reschedule at its trap exit.
    /// Only a recheck may repeat; events that decide a result skip waits
    /// that are already signaled.
    fn signal(&mut self, slot: u16, preset: Option<i64>) {
        let index = usize::from(slot);
        if self.signaled[index] {
            assert!(preset.is_none(), "a decided wait was signaled again");
            return;
        }
        self.signaled[index] = true;
        self.has_preset[index] = preset.is_some();
        self.preset[index] = preset.unwrap_or(0);
        self.at[index] = timer::ticks();
        #[cfg(feature = "boot-smoke")]
        {
            self.from[index] = cpudata::current_cpu() as u8;
        }
        self.links.push(&mut self.list, slot);
        cpudata::set_need_resched();
    }

    fn take(&mut self, slot: u16) -> Signal {
        let index = usize::from(slot);
        assert!(self.signaled[index]);
        self.links.remove(&mut self.list, slot);
        self.signaled[index] = false;
        Signal {
            slot,
            preset: self.has_preset[index].then_some(self.preset[index]),
            at: self.at[index],
            #[cfg(feature = "boot-smoke")]
            from: self.from[index],
        }
    }
}

/// Threads that came back on an AP with a request only the BSP may complete
/// (spec 0051, point 5): a page-in or VM call (`Memory`), `Creating`, or an
/// exit whose kernel stack must be freed. The BSP's pass completes them in
/// FIFO order. A listed thread is on no CPU; finishing its process takes it
/// off the list.
struct BspWork {
    list: List,
    links: Links,
    listed: [bool; THREAD_SLOTS],
}

/// Every blocked thread's queues. All fields start zero, so the static lands
/// in .bss.
struct Waits {
    lists: [List; LISTS],
    links: Links,
    /// Each thread's event list index plus one, or zero when on none.
    list_of: [u16; THREAD_SLOTS],
    /// A futex waiter's word address.
    address: [u64; THREAD_SLOTS],
    deadlines: Deadlines,
    signals: Signals,
}

impl Waits {
    const fn new() -> Self {
        Self {
            lists: [List::EMPTY; LISTS],
            links: Links::new(),
            list_of: [0; THREAD_SLOTS],
            address: [0; THREAD_SLOTS],
            deadlines: Deadlines::new(),
            signals: Signals::new(),
        }
    }

    /// Take a thread off its event queue and the deadline heap, if on them.
    fn release(&mut self, slot: u16) {
        let index = usize::from(slot);
        if let Some(list) = usize::from(self.list_of[index]).checked_sub(1) {
            self.links.remove(&mut self.lists[list], slot);
            self.list_of[index] = 0;
            self.publish_input_waiters(list);
        }
        if self.deadlines.queued[index] {
            self.deadlines.remove(slot);
        }
    }

    /// Mirror the device-input queue's length into the atomic that a CPL3
    /// input IRQ reads, after list `list` changed (0030-D2).
    fn publish_input_waiters(&self, list: usize) {
        if list == INPUT_LIST {
            input::set_waiters(usize::from(self.lists[INPUT_LIST].length));
        }
    }

    fn is_empty(&self) -> bool {
        self.lists.iter().all(|list| list.length == 0)
            && self.list_of.iter().all(|&list| list == 0)
            && self.deadlines.length == 0
            && self.signals.list.length == 0
    }
}

/// Print `MIRROR_SCHED_READY` once, before the first session, in every
/// flavor. The tables are statics, so setup only checks that they are empty.
pub(super) fn report_ready() {
    assert!(
        !REPORTED.swap(true, Ordering::SeqCst),
        "scheduler reported twice"
    );
    assert_drained();
    serial::println!(
        "MIRROR_SCHED_READY processes={} threads={} max_threads={} regions={}",
        PROCESS_SLOTS,
        THREAD_SLOTS,
        thread_abi::MAX_THREADS,
        vm_abi::MAX_REGIONS
    );
}

/// Check that no process, thread, queued thread or wait remains, as before
/// the first session and after each session's drain: a stale link can then
/// never wake a reused slot.
pub(super) fn assert_drained() {
    assert_eq!(PROCESSES.count(), 0, "process left in the table");
    assert_eq!(THREADS.count(), 0, "thread left in the table");
    assert_eq!(input::waiters(), 0, "device-input waiter left counted");
    assert_eq!(
        RUN_QUEUE.with(|queue| queue.length),
        0,
        "thread left queued"
    );
    assert!(WAITS.with(|waits| waits.is_empty()), "wait left queued");
    assert!(!bsp_work_pending(), "thread left on the BSP work list");
    assert_eq!(PHASE.load(Ordering::SeqCst), CLOSED, "a session is open");
    assert_eq!(
        PARTICIPANTS.load(Ordering::SeqCst),
        0,
        "an AP is in a session"
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

/// Remove a thread, which must be on no CPU, from the table, the run queue
/// and every wait queue. Its kernel stack, unless its exit already freed it,
/// goes to the reuse pool; only the scheduler and kernel setup remove a
/// thread that still holds one (0050-D2). The rest of the entry is simply
/// dropped.
///
/// # Safety
/// `slot` is occupied and no reference to its entry is live. Its owner drops
/// the slot from its member list.
pub(super) unsafe fn remove_thread(slot: u16) {
    // SAFETY: The caller guarantees an occupied entry that nothing references;
    // this read ends at once.
    let on_cpu = unsafe { (*thread_entry(slot)).on_cpu };
    assert!(!on_cpu, "removed a thread that is on a CPU");
    detach(slot);
    // SAFETY: The caller guarantees an occupied, unreferenced entry.
    let thread = unsafe { THREADS.remove(usize::from(slot)) };
    if let Some(stack) = thread.stack {
        super::release_stack(stack);
    }
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

/// 0054-D5's kill rank: background first, then agent, then normal and test;
/// the protected class is never a candidate.
fn kill_rank(class: MemoryClass) -> Option<u8> {
    match class {
        MemoryClass::Background => Some(0),
        MemoryClass::Agent => Some(1),
        MemoryClass::Normal | MemoryClass::Test => Some(2),
        MemoryClass::Protected => None,
    }
}

/// Select the next OOM victim (0054-D5): for a non-protected requester, only
/// strictly lower kill classes; for a protected or kernel requester, every
/// non-protected class. The lowest kill class first; within a class, the
/// largest footprint — owned pages plus owned tables plus the 0059-D3 charge,
/// each counted once — then the newest PID. Only live processes are
/// candidates. The requester's slot is never borrowed here: its caller holds
/// that reference.
pub(super) fn select_victim(requester: Option<(usize, MemoryClass)>) -> Option<usize> {
    let requester_rank = requester.and_then(|(_, class)| kill_rank(class));
    let skip = requester.map(|(slot, _)| slot);
    let mut best: Option<(usize, u8, u64, u64)> = None;
    for slot in process_slots() {
        if Some(slot) == skip {
            continue;
        }
        // SAFETY: the scheduler holds no other reference to this entry.
        let process = unsafe { process(slot) };
        if process.finished() {
            continue;
        }
        let Some(rank) = kill_rank(process.memory_class) else {
            continue;
        };
        if let Some(requester_rank) = requester_rank
            && rank >= requester_rank
        {
            continue;
        }
        let footprint = process.space.owned_page_count() as u64
            + process.space.owned_table_count() as u64
            + process.surface_frames;
        let better = match best {
            None => true,
            Some((_, best_rank, best_footprint, best_pid)) => {
                rank < best_rank
                    || (rank == best_rank && footprint > best_footprint)
                    || (rank == best_rank && footprint == best_footprint && process.pid > best_pid)
            }
        };
        if better {
            best = Some((slot, rank, footprint, process.pid));
        }
    }
    best.map(|(slot, _, _, _)| slot)
}

/// 0054-D5's selection self-test (boot-smoke): drive victim selection over
/// synthetic class/charge entries for the class order, the footprint and PID
/// tie-breaks, and the protected exclusion. Identical images make the base
/// footprints equal, so the charge assignments decide them.
#[cfg(feature = "boot-smoke")]
pub(super) fn selection_self_test(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
) {
    let baseline = super::FrameBaseline::take(frames);
    const CLASSES: [MemoryClass; 5] = [
        MemoryClass::Background,
        MemoryClass::Background,
        MemoryClass::Agent,
        MemoryClass::Normal,
        MemoryClass::Protected,
    ];
    let charges = [0u64, 4, 9, 99, 1000];
    let mut slots = [0usize; 5];
    for (index, (class, charge)) in CLASSES.into_iter().zip(charges).enumerate() {
        slots[index] = super::load_process(
            kernel,
            frames,
            bytes,
            image,
            direct_map,
            0,
            MemoryClass::Test,
        )
        .expect("selection test process");
        // SAFETY: the fresh entry has never run and nothing else references it.
        let process = unsafe { process(slots[index]) };
        process.memory_class = class;
        process.surface_frames = charge;
    }
    // A normal requester takes the lowest eligible class, and within it the
    // largest footprint: the second background entry's 4 beats the first's 0.
    assert_eq!(
        select_victim(Some((slots[3], MemoryClass::Normal))),
        Some(slots[1])
    );
    // The PID tie-break: equal footprints take the newer entry.
    // SAFETY: as above; nothing else references the entry.
    unsafe { process(slots[0]) }.surface_frames = 4;
    assert_eq!(
        select_victim(Some((slots[3], MemoryClass::Normal))),
        Some(slots[1])
    );
    // The footprint rule: an older but larger entry wins.
    // SAFETY: as above.
    unsafe { process(slots[0]) }.surface_frames = 5;
    assert_eq!(
        select_victim(Some((slots[3], MemoryClass::Normal))),
        Some(slots[0])
    );
    // A background requester has no lower class, and no requester is its own
    // victim.
    assert_eq!(
        select_victim(Some((slots[0], MemoryClass::Background))),
        None
    );
    assert_eq!(
        select_victim(Some((slots[1], MemoryClass::Background))),
        None
    );
    // A protected or kernel requester draws from every non-protected class,
    // still in S18's order: background first, and within it the larger
    // footprint (5 beats 4).
    assert_eq!(
        select_victim(Some((slots[4], MemoryClass::Protected))),
        Some(slots[0])
    );
    assert_eq!(select_victim(None), Some(slots[0]));
    // The protected entry is never a candidate, whatever its footprint.
    assert_ne!(select_victim(None), Some(slots[4]));
    // Finished entries are excluded: with both backgrounds done, the agent
    // dies before normal; with everything done, no candidate remains.
    // SAFETY: the entry has never run and nothing else references it.
    unsafe { process(slots[0]) }.finish(Outcome::Exit(0));
    // SAFETY: as above.
    unsafe { process(slots[1]) }.finish(Outcome::Exit(0));
    assert_eq!(select_victim(None), Some(slots[2]));
    // SAFETY: as above.
    unsafe { process(slots[2]) }.finish(Outcome::Exit(0));
    assert_eq!(select_victim(None), Some(slots[3]));
    // SAFETY: as above.
    unsafe { process(slots[3]) }.finish(Outcome::Exit(0));
    assert_eq!(select_victim(None), None);
    for slot in slots {
        // SAFETY: the process never ran, and nothing references it.
        let process = unsafe { process(slot) };
        if !process.finished() {
            process.finish(Outcome::Exit(0));
        }
        process.release_threads();
        // SAFETY: it never ran and nothing references it.
        let mut process = unsafe { discard(slot) };
        // SAFETY: its root has never been active, and nothing references its
        // tree.
        unsafe { process.space.destroy(frames) }.expect("selection teardown");
    }
    // Return the five pooled kernel stacks before the accounting check.
    super::drain_stacks(kernel, frames);
    assert_drained();
    // 0050-D2's pooled kernel stacks are not a leak: the baseline accounts
    // them.
    baseline.assert_kept(frames, "selection test frame leak");
}

/// Append a ready thread to the run queue.
pub(super) fn enqueue(slot: u16) {
    append(slot, false);
}

/// Append a thread that the current completion pass woke. Until the next
/// pass these dispatch before older entries (0030-D3).
pub(super) fn enqueue_woken(slot: u16) {
    append(slot, true);
}

/// Put an interrupted thread that a wake displaced with quantum left back at
/// the head of the run queue (0030-D3). The current pass's woken threads
/// still dispatch first, by their precedence, and the displaced thread then
/// resumes its turn ahead of every older entry. At the tail it would fall
/// behind the threads queued before it ran once the next pass began, which
/// reordered a woken set whenever a deadline displaced one of its threads
/// before that thread's first instruction.
fn requeue_displaced(slot: u16) {
    let index = usize::from(slot);
    RUN_QUEUE.with(|queue| {
        assert!(!queue.queued[index], "thread queued twice");
        if queue.length == 0 {
            queue.tail = slot;
        } else {
            queue.next[index] = queue.head;
        }
        queue.head = slot;
        queue.queued[index] = true;
        queue.woken_in[index] = 0;
        queue.length += 1;
    });
}

fn append(slot: u16, woken: bool) {
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
        queue.woken_in[index] = if woken { queue.pass } else { 0 };
        queue.length += 1;
    });
}

/// Start a completion pass: threads woken earlier lose their precedence.
fn begin_pass() {
    RUN_QUEUE.with(|queue| {
        queue.pass = queue
            .pass
            .checked_add(1)
            .expect("scheduler passes exhausted");
    });
}

/// Whether an event signaled a wait since this CPU's last dispatch, cleared
/// by this read. The syscall that signaled then reschedules instead of
/// returning.
pub(super) fn take_reschedule() -> bool {
    cpudata::take_need_resched()
}

/// Whether a timed wait's deadline has been reached, so the tick that
/// reached it reschedules (0030-D3).
pub(super) fn deadline_due(now: u64) -> bool {
    WAITS.with(|waits| {
        waits
            .deadlines
            .first()
            .is_some_and(|slot| deadline_reached(now, waits.deadlines.deadline[usize::from(slot)]))
    })
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

/// Dequeue the first thread for which `runnable` holds, preferring those
/// that the current pass queued as woken. One scan of at most the whole
/// queue (THREAD_SLOTS entries); `runnable` sees each entry at most once.
fn dequeue_first(mut runnable: impl FnMut(u16) -> bool) -> Option<u16> {
    RUN_QUEUE.with(|queue| {
        // The first runnable older entry and its predecessor.
        let mut fallback = None;
        let mut previous = None;
        let mut current = queue.head;
        for _ in 0..queue.length {
            let woken = queue.woken_in[usize::from(current)] == queue.pass;
            if (woken || fallback.is_none()) && runnable(current) {
                if woken {
                    unlink(queue, previous, current);
                    return Some(current);
                }
                fallback = Some((previous, current));
            }
            previous = Some(current);
            current = queue.next[usize::from(current)];
        }
        let (previous, slot) = fallback?;
        unlink(queue, previous, slot);
        Some(slot)
    })
}

/// Attach a thread that has just blocked to its event queue, if it has one,
/// and to the deadline heap when its wait is timed.
pub(super) fn block(slot: u16, queue: Option<Queue>, deadline: Option<u64>) {
    WAITS.with(|waits| {
        let index = usize::from(slot);
        assert!(
            waits.list_of[index] == 0
                && !waits.deadlines.queued[index]
                && !waits.signals.signaled[index],
            "thread blocked twice"
        );
        if let Some(queue) = queue {
            let list = queue.list();
            if let Queue::Futex { address, .. } = queue {
                waits.address[index] = address;
            }
            waits.links.push(&mut waits.lists[list], slot);
            waits.list_of[index] = list as u16 + 1;
            waits.publish_input_waiters(list);
        }
        if let Some(deadline) = deadline {
            waits.deadlines.insert(slot, deadline);
        }
    });
}

/// Signal one blocked thread's wait, with its result when the event decided
/// it. The thread stays on its queues until the scheduler completes it.
pub(super) fn signal(slot: u16, preset: Option<i64>) {
    WAITS.with(|waits| waits.signals.signal(slot, preset));
}

/// Signal every wait on `queue`, in FIFO order, for a recheck.
pub(super) fn signal_queue(queue: Queue) {
    WAITS.with(|waits| {
        let Waits {
            lists,
            links,
            signals,
            ..
        } = waits;
        for slot in links.slots(lists[queue.list()]) {
            signals.signal(slot, None);
        }
    });
}

/// Decide an explicit wake of `process`'s waits on `address`, in FIFO order:
/// up to `count` waits whose deadline has not passed get result 0, and each
/// passed one gets ETIMEDOUT without counting. Returns (woken, timed out).
pub(super) fn wake_address(process: u16, address: u64, count: u64, now: u64) -> (u64, u64) {
    let list = Queue::Futex { process, address }.list();
    WAITS.with(|waits| {
        let Waits {
            lists,
            links,
            address: keys,
            deadlines,
            signals,
            ..
        } = waits;
        let (mut woken, mut expired) = (0, 0);
        for slot in links.slots(lists[list]) {
            let index = usize::from(slot);
            if keys[index] != address || signals.signaled[index] {
                continue;
            }
            if deadlines.queued[index] && deadline_reached(now, deadlines.deadline[index]) {
                signals.signal(slot, Some(-110));
                expired += 1;
            } else if woken < count {
                signals.signal(slot, Some(0));
                woken += 1;
            }
        }
        (woken, expired)
    })
}

/// Cancel `process`'s undecided address waits in `start..end` with EFAULT,
/// in FIFO order, and return how many were cancelled.
pub(super) fn cancel_addresses(process: u16, start: u64, end: u64) -> u64 {
    let list = Queue::Futex {
        process,
        address: start,
    }
    .list();
    WAITS.with(|waits| {
        let Waits {
            lists,
            links,
            address: keys,
            signals,
            ..
        } = waits;
        let mut cancelled = 0;
        for slot in links.slots(lists[list]) {
            let index = usize::from(slot);
            if (start..end).contains(&keys[index]) && !signals.signaled[index] {
                signals.signal(slot, Some(-14));
                cancelled += 1;
            }
        }
        cancelled
    })
}

/// Take a completed wait off its event queue and the deadline heap.
pub(super) fn release(slot: u16) {
    WAITS.with(|waits| {
        assert!(!waits.signals.signaled[usize::from(slot)]);
        waits.release(slot);
    });
}

/// Take a thread off the run queue, every wait queue and the BSP work list,
/// as a finishing process does for each of its threads.
pub(super) fn detach(slot: u16) {
    unqueue(slot);
    WAITS.with(|waits| {
        if waits.signals.signaled[usize::from(slot)] {
            waits.signals.take(slot);
        }
        waits.release(slot);
    });
    BSP_WORK.with(|work| {
        if core::mem::take(&mut work.listed[usize::from(slot)]) {
            work.links.remove(&mut work.list, slot);
        }
    });
}

/// Hand a thread that came back on this AP with a request only the BSP may
/// complete to the BSP's work list (spec 0051, point 5). The caller then
/// kicks the BSP.
pub(super) fn push_bsp_work(slot: u16) {
    assert_ne!(cpudata::current_cpu(), 0, "the BSP handed itself work");
    BSP_WORK.with(|work| {
        let index = usize::from(slot);
        assert!(!work.listed[index], "a thread handed to the BSP twice");
        work.links.push(&mut work.list, slot);
        work.listed[index] = true;
    });
}

/// The oldest thread on the BSP work list, taken off it.
fn take_bsp_work() -> Option<u16> {
    BSP_WORK.with(|work| {
        if work.list.length == 0 {
            return None;
        }
        let slot = work.list.head;
        work.links.remove(&mut work.list, slot);
        work.listed[usize::from(slot)] = false;
        Some(slot)
    })
}

/// Whether a thread waits on the BSP work list.
fn bsp_work_pending() -> bool {
    BSP_WORK.with(|work| work.list.length != 0)
}

/// Whether a finished process waits only for the BSP's loop top to release
/// its threads: none is on a CPU any more (spec 0051, point 8).
fn teardown_pending() -> bool {
    process_slots().any(|slot| {
        // SAFETY: The scheduler holds no other process reference.
        let process = unsafe { process(slot) };
        process.finished() && !process.zombie() && process.quiescent()
    })
}

/// Signal every timed wait whose deadline has been reached, earliest first.
fn expire(now: u64) {
    WAITS.with(|waits| {
        for _ in 0..THREAD_SLOTS {
            let Some(slot) = waits.deadlines.first() else {
                return;
            };
            if !deadline_reached(now, waits.deadlines.deadline[usize::from(slot)]) {
                return;
            }
            waits.deadlines.remove(slot);
            waits.signals.signal(slot, None);
        }
    });
}

/// The oldest signaled wait that this CPU may complete, if any: its thread
/// is on no CPU, and its process may run on this CPU. A thread that parked
/// inside its own handler stays on its CPU until that CPU's scheduler has
/// stored its context; its signals wait for a later pass (spec 0051, point
/// 2). A pinned process's waits are left to the CPUs it may run on (point
/// 6).
fn next_signal() -> Option<Signal> {
    let me = 1 << cpudata::current_cpu();
    WAITS.with(|waits| {
        let signals = &mut waits.signals;
        let slot = signals
            .links
            .slots(signals.list)
            .find(|&slot| completable(slot, me))?;
        Some(signals.take(slot))
    })
}

/// Whether a CPU in `cpus` may complete thread `slot`'s signaled wait: the
/// thread is on no CPU and its process may run on one of them.
fn completable(slot: u16, cpus: u64) -> bool {
    // SAFETY: A signaled thread and its process are occupied, and these
    // reads of single fields hold no reference past them.
    unsafe {
        let thread = thread_entry(slot);
        !(*thread).on_cpu && (*PROCESSES.entry(usize::from((*thread).process))).cpus & cpus != 0
    }
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
    /// An SMP battery session (spec 0051, step 5) ends once its process is a
    /// zombie. With `panic`, the BSP panics in its pass instead, as soon as a
    /// battery thread runs on every AP that runs userspace (Panic).
    #[cfg(feature = "boot-smoke")]
    Battery { process: usize, panic: bool },
}

/// Everything one session shares, owned by its seeding function. `run`
/// publishes it; a process may use the parts its `Access` allows.
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
            // Its threads ran on every CPU, so it ends once the loop top has
            // released them, when none is on a CPU any more.
            // SAFETY: As above.
            #[cfg(feature = "boot-smoke")]
            End::Battery { process: slot, .. } => unsafe { process(slot) }.zombie(),
        }
    }
}

/// Scheduling counts for `MIRROR_SCHEDULER_OK`, from the CPU's pass
/// statistics in its `CpuData`.
#[derive(Clone, Copy, Default)]
#[cfg_attr(
    not(feature = "boot-smoke"),
    expect(dead_code, reason = "only init's smoke pair reports the counts")
)]
pub(super) struct Stats {
    /// Dispatches that changed process.
    pub(super) switches: usize,
    pub(super) idle_ticks: u64,
}

/// Publish `session` as the running one, with the kernel tables it borrows.
fn publish<'a, 'fb>(session: &mut Session<'a, 'fb>, kernel: &PageTables) {
    let pointer = core::ptr::from_mut(session);
    assert!(
        SESSION
            .compare_exchange(
                core::ptr::null_mut(),
                pointer.cast(),
                Ordering::SeqCst,
                Ordering::SeqCst
            )
            .is_ok(),
        "a session is already published"
    );
    KERNEL.store(core::ptr::from_ref(kernel).cast_mut(), Ordering::SeqCst);
}

/// Revoke the published session, once no AP is in it.
fn unpublish() {
    assert_eq!(PHASE.load(Ordering::SeqCst), CLOSED);
    SESSION.store(core::ptr::null_mut(), Ordering::SeqCst);
    KERNEL.store(core::ptr::null_mut(), Ordering::SeqCst);
}

/// Whether no session is published.
pub(super) fn session_unpublished() -> bool {
    SESSION.load(Ordering::SeqCst).is_null()
}

/// Lend the running session to `operation`, under SCHED. Its borrow cannot
/// escape: the closure must accept any lifetimes.
pub(super) fn with_session<R>(operation: impl FnOnce(&mut Session<'_, '_>) -> R) -> R {
    assert_held();
    let pointer = SESSION.load(Ordering::SeqCst).cast::<Session<'_, '_>>();
    assert!(!pointer.is_null(), "no active session");
    assert!(
        !SESSION_BUSY.swap(true, Ordering::Acquire),
        "reentrant session access"
    );
    // SAFETY: `run` published this session from a live exclusive borrow and
    // revokes it before that borrow ends, after every AP left it. SCHED
    // excludes other CPUs, and the busy flag excludes a second loan here:
    // `run` itself reaches the session only through this function. The
    // closure cannot retain the borrow.
    let result = operation(unsafe { &mut *pointer });
    SESSION_BUSY.store(false, Ordering::Release);
    result
}

/// A thread that came back from user mode still ready, to be queued again
/// after the next completion pass. This CPU keeps its claim (`on_cpu`)
/// until then.
#[derive(Clone, Copy)]
struct Returned {
    slot: u16,
    /// A wake displaced it with quantum left (0030-D3), so it dispatches
    /// before older entries, behind the first eligible thread that pass wakes.
    displaced: bool,
}

/// Release this CPU's claim on a thread that came back ready and queue it: a
/// displaced one at the head, behind this pass's woken threads by their
/// precedence, any other at the tail. The BSP may have released SCHED since
/// the thread came back, and another CPU may have finished its process
/// meanwhile; the claim kept that process from being torn down, and the
/// thread now only leaves this CPU, for the BSP's next loop top to tear the
/// process down (spec 0051, point 8).
fn requeue(thread: Returned) {
    if !release_claim(thread.slot) {
        return;
    }
    if thread.displaced {
        requeue_displaced(thread.slot);
    } else {
        enqueue(thread.slot);
    }
}

/// Clear this CPU's claim on thread `slot`, which came back ready, and
/// return whether it is still ready rather than finished.
fn release_claim(slot: u16) -> bool {
    let me = cpudata::current_cpu();
    // SAFETY: The claim keeps the thread's process from looking quiescent, so
    // its entry is still occupied. The scheduler holds no other reference to
    // it, and this borrow ends with the function.
    let thread = unsafe { &mut *thread_entry(slot) };
    assert!(
        thread.on_cpu && usize::from(thread.cpu) == me,
        "released a thread this CPU does not hold"
    );
    thread.on_cpu = false;
    match thread.state {
        State::Ready => true,
        State::Finished => {
            // Only the BSP tears processes down.
            if me != 0 {
                smp::kick(0);
            }
            false
        }
        state => panic!("a held ready thread became {state:?}"),
    }
}

/// The CPUs in `mask`, lowest first.
fn cpus_in(mask: u64) -> impl Iterator<Item = usize> {
    (0..u64::BITS as usize).filter(move |&cpu| mask & 1 << cpu != 0)
}

/// Run one session until its end rule holds, on the BSP with SCHED held by
/// the caller. The APs that run userspace join it (`run_ap`). On return no
/// thread runs on any CPU, no session is published, and the session's
/// processes, finished or not, stay in the table for the caller to drain.
pub(super) fn run(
    session: &mut Session<'_, '_>,
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    direct_map: u64,
) -> Stats {
    assert!(!arch::interrupts_enabled());
    assert_held();
    assert!(!interrupts::user_context_active());
    assert_eq!(cpudata::current_cpu(), 0, "a session runs on the BSP");
    // SAFETY: IF=0 and no user context is active. The BSP's guarded RSP0
    // stack is mapped in the shared upper half of every process root.
    unsafe { interrupts::set_kernel_stack(smp::rsp0_placeholder()) };
    cpudata::reset_pass_stats();
    publish(session, kernel);
    open_session();
    let mut returned: Option<Returned> = None;
    loop {
        assert!(!arch::interrupts_enabled());
        assert_idle_cpu();
        // 0059-D1: the loop top between dispatches is a heap safe point (BSP,
        // IF=0, kernel root active, no IRQ-needed lock held, no syscall frame
        // live). Growth may wait for other CPUs, so SCHED is released; the
        // thread in `returned` stays claimed across it (`requeue`).
        unlocked(|| heap::ensure(kernel, frames, direct_map));
        let ended = with_session(|session| {
            // A finished process's endpoints close before any peer's receive
            // is checked, so a blocked peer observes closure (readme, IPC).
            // Once no thread of it is on a CPU and no CPU has its root
            // loaded, every thread context is saved, so the process gives up
            // its thread entries and stays a zombie until reaped (0030-D5;
            // spec 0051, point 8).
            for slot in process_slots() {
                // SAFETY: The scheduler holds no other process reference.
                let process = unsafe { process(slot) };
                release_handles(process, &mut session.ipc);
                if process.finished() && !process.zombie() && process.quiescent() {
                    process.release_threads();
                }
            }
            if let Some(runtime) = &mut session.runtime {
                runtime.reclaim_finished(frames, &mut session.ipc);
            }
            // 0059-D4 (boot-smoke): the shell session's charge peaks, sampled
            // one loop top after any launch, close or reclaim.
            #[cfg(feature = "boot-smoke")]
            if let End::Shell { shell } = session.end {
                let runtime = session.runtime.as_mut().expect("shell runtime");
                // SAFETY: The scheduler holds no other process reference.
                runtime.sample_charge_peaks(unsafe { process(shell) });
            }
            session.ended()
        });
        if ended {
            // The thread this CPU still holds kept its process from being
            // torn down above. Queue it, or drop it if its process finished,
            // and end at the next loop top, after that teardown; its queue
            // position no longer matters. The caller finishes the session's
            // remaining processes, which unqueues it.
            if let Some(thread) = returned.take() {
                requeue(thread);
                continue;
            }
            crate::perf::end_session();
            close_session();
            unpublish();
            let (switches, idle_ticks) = cpudata::pass_stats();
            return Stats {
                switches: usize::try_from(switches).expect("switch count"),
                idle_ticks,
            };
        }
        // BSP/IF=0 and the allocator is borrowed only here, outside dispatch.
        // Check the end rule first so a finished session gets no more samples.
        crate::perf::sample_memory(frames);
        // What the APs handed over completes before the pass begins: a
        // release may drop SCHED for its shootdowns, and the pass's woken
        // threads keep their precedence only within one hold (0030-D3).
        complete_bsp_work(kernel, frames);
        let now = timer::ticks();
        begin_pass();
        let end = with_session(|session| {
            complete_waits(session, now);
            session.end
        });
        // Queue the returned thread before a shell close can finish its
        // process and take it off the queue.
        if let Some(thread) = returned.take() {
            requeue(thread);
        }
        match end {
            #[cfg(feature = "boot-smoke")]
            End::Pair { .. } => {}
            End::Shell { shell } => {
                with_session(|session| complete_shell_requests(session, frames, shell));
            }
            #[cfg(feature = "boot-smoke")]
            End::Battery { process, panic } => {
                if panic {
                    panic_while_aps_run(process);
                }
            }
        }
        let selected = select(now);
        kick_idle(now);
        let Some(slot) = selected else {
            // Work an AP handed over while a release dropped SCHED, and a
            // process whose last thread left another CPU meanwhile, wait for
            // the next pass and loop top instead of a halt.
            if !bsp_work_pending() && !teardown_pending() {
                idle();
            }
            continue;
        };
        // SAFETY: The thread is occupied and nothing references it here.
        let owner = usize::from(unsafe { (*thread_entry(slot)).process });
        cpudata::note_dispatch(owner as u32);
        returned = dispatch(kernel, Some(&mut *frames), owner, slot);
        #[cfg(feature = "boot-smoke")]
        if let End::Pair {
            first,
            second,
            scenario,
        } = end
        {
            check_budget(owner, first, second, scenario);
        }
    }
}

/// Open the published session to the APs that run userspace, and kick each
/// so that it joins at once rather than at its next tick (spec 0051, point
/// 5).
fn open_session() {
    assert_eq!(PHASE.load(Ordering::SeqCst), CLOSED, "a session is open");
    assert_eq!(
        PARTICIPANTS.load(Ordering::SeqCst),
        0,
        "an AP is in a session"
    );
    PHASE.store(OPEN, Ordering::SeqCst);
    for cpu in cpus_in(smp::userspace_mask() & !1) {
        smp::kick(cpu);
    }
}

/// The session-end barrier (spec 0051, point 9): no AP joins any more, each
/// AP in the session is kicked, and the BSP waits outside SCHED, bounded,
/// until every one has returned its thread and left for its idle loop. Then
/// no thread is on any CPU and no CPU has a process root loaded, which the
/// shell's close, the frame baselines and 0058a's restart rely on.
fn close_session() {
    PHASE.store(CLOSING, Ordering::SeqCst);
    for cpu in cpus_in(PARTICIPANTS.load(Ordering::SeqCst)) {
        smp::kick(cpu);
    }
    // An AP needs SCHED to leave, and never waits for this CPU on its way.
    let left = unlocked(|| {
        let mut bound = clock::SpinBound::new(BARRIER_WAIT_NS);
        while PARTICIPANTS.load(Ordering::Acquire) != 0 {
            if bound.expired() {
                return false;
            }
            core::hint::spin_loop();
        }
        true
    });
    assert!(
        left,
        "session barrier timeout: participants {:#x}",
        PARTICIPANTS.load(Ordering::SeqCst)
    );
    PHASE.store(CLOSED, Ordering::SeqCst);
    for slot in process_slots() {
        // SAFETY: The scheduler holds no other process reference.
        let process = unsafe { process(slot) };
        assert!(
            process.threads().all(|thread| !thread.on_cpu),
            "a thread is on a CPU after the session barrier"
        );
        assert_eq!(
            process.space.loaded_cpus(),
            0,
            "a CPU has a process root loaded after the session barrier"
        );
    }
}

/// Join the open session from this AP's idle loop and run its threads until
/// the session closes (spec 0051, point 5), with IF=0 and no lock held. An
/// AP outside `smp::userspace_mask`, or a wake outside an open session,
/// returns at once.
pub(super) fn run_ap() {
    let me = cpudata::current_cpu();
    let bit = 1_u64 << me;
    assert!(!arch::interrupts_enabled());
    if me == 0 || smp::userspace_mask() & bit == 0 || PHASE.load(Ordering::Acquire) != OPEN {
        return;
    }
    let guard = lock();
    // PHASE changes only under SCHED, so this check is exact: once the BSP
    // starts its barrier, no AP joins.
    if PHASE.load(Ordering::SeqCst) != OPEN {
        return;
    }
    assert_eq!(
        PARTICIPANTS.fetch_or(bit, Ordering::SeqCst) & bit,
        0,
        "an AP joined a session twice"
    );
    let kernel = KERNEL.load(Ordering::SeqCst);
    assert!(!kernel.is_null(), "an open session without kernel tables");
    // SAFETY: `run` published these tables with the session from a live
    // shared borrow, and its barrier waits for this CPU to leave before the
    // borrow ends. APs only read the root through them.
    let kernel = unsafe { &*kernel };
    // SAFETY: IF=0 and no user context is active; the placeholder is this
    // AP's own kernel stack top, mapped in every process root.
    unsafe { interrupts::set_kernel_stack(smp::rsp0_placeholder()) };
    let mut returned: Option<Returned> = None;
    while PHASE.load(Ordering::SeqCst) == OPEN {
        assert!(!arch::interrupts_enabled());
        assert_idle_cpu();
        let now = timer::ticks();
        begin_pass();
        with_session(|session| complete_waits(session, now));
        if let Some(thread) = returned.take() {
            requeue(thread);
        }
        let selected = select(now);
        kick_idle(now);
        let Some(slot) = selected else {
            idle();
            continue;
        };
        // SAFETY: The thread is occupied and nothing references it here.
        let owner = usize::from(unsafe { (*thread_entry(slot)).process });
        cpudata::note_dispatch(owner as u32);
        returned = dispatch(kernel, None, owner, slot);
    }
    if let Some(thread) = returned.take() {
        requeue(thread);
    }
    drop(guard);
    // Back to the idle loop, holding nothing: the BSP's barrier may end.
    PARTICIPANTS.fetch_and(!bit, Ordering::SeqCst);
}

/// Check that this CPU's scheduler runs and completes for no thread and is
/// not inside a user context, as at each loop top.
fn assert_idle_cpu() {
    assert!(!interrupts::user_context_active());
    assert_eq!(current(), None, "a thread is still current at the loop top");
}

/// Name the thread this CPU runs, or whose wait or request its pass
/// completes, in its `CpuData`, where `Process::thread` and the handlers find
/// it (spec 0051, point 2). Nothing may be current already.
pub(super) fn set_current(process: usize, thread: u16) {
    assert_held();
    assert_eq!(current(), None, "this CPU already has a current thread");
    let process = u32::try_from(process).expect("process slot");
    cpudata::set_running(Some((process, u32::from(thread))));
}

/// End `set_current`.
pub(super) fn clear_current() {
    assert!(current().is_some(), "no current thread to clear");
    cpudata::set_running(None);
}

/// The process and thread slots this CPU runs or completes for, if any.
pub(super) fn current() -> Option<(usize, u16)> {
    cpudata::running().map(|(process, thread)| {
        (
            usize::try_from(process).expect("process slot"),
            u16::try_from(thread).expect("thread slot"),
        )
    })
}

/// Complete the signaled waits that this CPU may complete in signal order
/// (0030-D2), after every return from user mode and every idle wakeup. On
/// the BSP, expired deadlines and device input are signaled first: only the
/// BSP's clock reaches deadlines, and only it drains input (spec 0051,
/// points 3 and 6). A completion signals nothing, so each thread is taken at
/// most once.
fn complete_waits(session: &mut Session<'_, '_>, now: u64) {
    if cpudata::current_cpu() == 0 {
        expire(now);
        // The flag is cleared, with IF=0, before the device-input waiters are
        // rechecked, so input that arrives later sets it again. Drain work
        // that a PS/2 or vmmouse bound deferred continues in the waiters'
        // recheck.
        let arrived = input::take_arrival();
        if arrived || crate::ps2::drain_pending() {
            signal_queue(Queue::Input);
        }
    }
    for _ in 0..THREAD_SLOTS {
        let Some(signal) = next_signal() else {
            return;
        };
        let slot = signal.slot;
        // SAFETY: A signaled thread is occupied, since finishing a process
        // detaches its threads, and the scheduler holds no reference to it.
        let owner = usize::from(unsafe { (*thread_entry(slot)).process });
        set_current(owner, slot);
        // SAFETY: The scheduler holds no other process reference.
        let process = unsafe { process(owner) };
        assert!(!process.finished(), "finished process left a signaled wait");
        let input = session.input.as_mut().filter(|_| process.access.input);
        let windows = session
            .runtime
            .as_mut()
            .filter(|_| process.access.windows)
            .map(|runtime| &mut runtime.windows);
        complete_wait(process, signal, &mut session.ipc, input, windows, now);
        clear_current();
    }
    // Only a thread still on a CPU, or one that another CPU must complete,
    // keeps a signal past the pass.
    assert!(
        next_signal().is_none(),
        "a completion signaled another wait"
    );
}

/// Complete what the APs handed the BSP (spec 0051, point 5), as each
/// thread's own scheduler would at the end of its slice: a page-in or VM
/// call, a creation, or an exit's stack free and its joiner's signal. The
/// BSP claims each thread for the completion, as a dispatch does. At most
/// one pass over the table: work handed over while a release dropped SCHED
/// may wait for the next pass.
fn complete_bsp_work(kernel: &PageTables, frames: &mut FrameAllocator<'_>) {
    assert_eq!(cpudata::current_cpu(), 0, "an AP completed BSP work");
    for _ in 0..THREAD_SLOTS {
        let Some(slot) = take_bsp_work() else {
            return;
        };
        let owner = {
            // SAFETY: A listed thread is occupied, and the scheduler holds no
            // reference to it; this borrow ends with the block.
            let thread = unsafe { &mut *thread_entry(slot) };
            assert!(!thread.on_cpu, "a handed-over thread is on a CPU");
            thread.on_cpu = true;
            thread.cpu = 0;
            usize::from(thread.process)
        };
        set_current(owner, slot);
        finish_slice(kernel, frames, owner);
        clear_current();
        // finish_slice leaves a ready thread claimed, and a thread whose
        // process another CPU finished during a release on no CPU.
        // SAFETY: As above; finish_slice's borrows have ended, and the entry
        // stays until a later pass reaps it.
        if unsafe { (*thread_entry(slot)).state } == State::Ready {
            requeue(Returned {
                slot,
                displaced: false,
            });
        }
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
        let (state, slot) = (thread.state, thread.slot);
        if !matches!(state, State::Launching(_) | State::Closing(_)) {
            continue;
        }
        set_current(shell_slot, slot);
        let result = match state {
            State::Launching(kind) => runtime
                .launch(frames, shell, kind)
                .unwrap_or_else(|error| error as u64),
            // A close whose app still has a thread on a CPU stays pending;
            // the close kicked that CPU, which kicks this one back once the
            // thread has left (spec 0051, point 8).
            State::Closing(pid) => {
                match runtime.close(frames, shell, &mut session.ipc, pid, Outcome::Terminated) {
                    Some(result) => result.map_or_else(|error| error as u64, |()| 0),
                    None => {
                        clear_current();
                        continue;
                    }
                }
            }
            _ => unreachable!("a shell request that is neither launch nor close"),
        };
        let thread = shell.thread_mut();
        thread.context.frame.rax = result;
        thread.wake();
        clear_current();
    }
}

/// Dequeue the first ready thread whose process may run on this CPU now and
/// which is on no CPU. The CPU policy stays the gate, so a throttled service
/// never resumes, and the process's CPU mask pins it (spec 0051, point 6).
fn select(now: u64) -> Option<u16> {
    let me = 1 << cpudata::current_cpu();
    dequeue_first(|slot| {
        // SAFETY: Queued slots are occupied, and the scheduler holds no
        // reference to this thread or its process; both borrows end here.
        let (state, owner, on_cpu) = unsafe {
            let thread = &*thread_entry(slot);
            (thread.state, usize::from(thread.process), thread.on_cpu)
        };
        // Its CPU has not stored its context yet (spec 0051, point 2).
        if on_cpu {
            return false;
        }
        assert_eq!(state, State::Ready, "queued thread is not ready");
        // SAFETY: As above.
        let process = unsafe { process(owner) };
        assert!(!process.finished(), "finished process left a queued thread");
        process.cpus & me != 0
            && match &mut process.policy {
                CpuPolicy::Probe => true,
                CpuPolicy::Shell(policy) | CpuPolicy::Application(policy) => policy.eligible(now),
            }
    })
}

/// The CPUs that could take work this pass left (spec 0051, point 3): those
/// a queued, eligible thread's process may run on, and those that may
/// complete a signaled wait that this CPU had to leave.
fn waiting_work(now: u64) -> (u64, u64) {
    let ready = RUN_QUEUE.with(|queue| {
        let mut cpus = 0;
        let mut current = queue.head;
        for _ in 0..queue.length {
            // SAFETY: Queued slots and their processes are occupied, and
            // these reads of single fields hold no reference past them. The
            // policy is copied, so the eligibility check changes nothing.
            let (on_cpu, process_cpus, mut policy) = unsafe {
                let thread = thread_entry(current);
                let process = PROCESSES.entry(usize::from((*thread).process));
                ((*thread).on_cpu, (*process).cpus, (*process).policy)
            };
            let eligible = match &mut policy {
                CpuPolicy::Probe => true,
                CpuPolicy::Shell(policy) | CpuPolicy::Application(policy) => policy.eligible(now),
            };
            if !on_cpu && eligible {
                cpus |= process_cpus;
            }
            current = queue.next[usize::from(current)];
        }
        cpus
    });
    let signaled = WAITS.with(|waits| {
        let signals = &waits.signals;
        signals
            .links
            .slots(signals.list)
            .filter(|&slot| completable(slot, u64::MAX))
            // SAFETY: As in `completable`.
            .map(|slot| unsafe {
                (*PROCESSES.entry(usize::from((*thread_entry(slot)).process))).cpus
            })
            .fold(0, |cpus, process| cpus | process)
    });
    (ready, signaled)
}

/// After a pass, kick each idle CPU that could take the work it left, and
/// one CPU that may complete a signaled wait this CPU could not, even a busy
/// one when none is idle: its slice yields to the wake, as on one CPU
/// (0030-D3). An AP that has not joined the open session counts as idle.
fn kick_idle(now: u64) {
    let cpus = smp::userspace_mask();
    let me = cpudata::current_cpu();
    let others = cpus & !(1 << me);
    if others == 0 {
        return;
    }
    let participants = PARTICIPANTS.load(Ordering::SeqCst);
    let idle = cpus_in(others)
        .filter(|&cpu| cpudata::is_idle(cpu) || (cpu != 0 && participants & 1 << cpu == 0))
        .fold(0, |mask, cpu| mask | 1 << cpu);
    let (ready, signaled) = waiting_work(now);
    let mut targets = (ready | signaled) & idle;
    let urgent = signaled & others;
    if urgent != 0 && urgent & idle == 0 {
        targets |= 1 << urgent.trailing_zeros();
    }
    for cpu in cpus_in(targets) {
        smp::kick(cpu);
    }
}

/// Run one thread until it returns to the scheduler. A thread that comes back
/// still ready is returned, for the next pass to queue again. Only slot
/// indices cross the user entry; no process reference is held across it
/// (spec 0051, point 2). The BSP passes its frame allocator and completes
/// the thread's request itself; an AP passes none and hands requests that
/// need it to the BSP (point 5).
fn dispatch(
    kernel: &PageTables,
    frames: Option<&mut FrameAllocator<'_>>,
    owner: usize,
    slot: u16,
) -> Option<Returned> {
    {
        // SAFETY: The selected thread is occupied, and the scheduler holds no
        // reference to it; this borrow ends with the block.
        let thread = unsafe { &mut *thread_entry(slot) };
        assert_eq!(usize::from(thread.process), owner);
        assert!(!thread.on_cpu, "dispatched a thread that is on a CPU");
        // resume_process clears it once the thread's context is stored.
        thread.on_cpu = true;
        thread.cpu = cpudata::current_cpu() as u8;
    }
    // This pass completed the waits that scheduler work signaled, such as
    // closing a finished process's endpoints. Only the slice's own signals,
    // or another CPU's kick, may reschedule it; both come under SCHED after
    // this clear, so neither is lost.
    let _ = cpudata::take_need_resched();
    set_current(owner, slot);
    match frames {
        Some(frames) => resume_process(kernel, frames, owner, slot),
        None => resume_on_ap(kernel, owner, slot),
    }
    clear_current();
    // A preempted, yielding, displaced or completed thread rejoins the queue
    // after the next completion pass, behind the threads it wakes. Until then
    // this CPU holds it, still claimed, so its process cannot be torn down
    // while the BSP releases SCHED at its loop top or for a release.
    // SAFETY: The thread stays occupied while this CPU holds SCHED: its
    // process is torn down, or a joiner reaps it, only in a later hold.
    let thread = unsafe { &*thread_entry(slot) };
    let ready = thread.state == State::Ready;
    assert_eq!(
        thread.on_cpu, ready,
        "a returned thread's claim does not match its state"
    );
    ready.then_some(Returned {
        slot,
        displaced: thread.displaced,
    })
}

/// Halt until an interrupt when no queued thread may run, with SCHED
/// released. On the BSP some thread must be waiting on time or input, be
/// throttled or run on another CPU, or a finished process must still have a
/// thread on another CPU, whose scheduler kicks the BSP once it has left;
/// otherwise nothing could wake. An AP halts whenever it has no thread it
/// may run. Only while this is the one CPU that runs user threads must
/// idling charge no user tick (spec 0051, point 9).
fn idle() {
    if cpudata::current_cpu() == 0 {
        assert!(!bsp_work_pending(), "the BSP halted with work handed over");
        assert!(
            process_slots().any(|slot| {
                // SAFETY: The scheduler holds no other process reference.
                let process = unsafe { process(slot) };
                if process.finished() {
                    return !process.quiescent();
                }
                process.threads().any(|thread| {
                    thread.on_cpu
                        || matches!(
                            thread.state,
                            State::Ready
                                | State::Running
                                | State::Sleeping(_)
                                | State::Receiving(_)
                                | State::InputWaiting(_)
                                | State::Waiting(_)
                        )
                })
            }),
            "every live thread is blocked without a waker"
        );
    }
    let charged = || -> u64 {
        process_slots()
            // SAFETY: As above.
            .map(|slot| unsafe { process(slot) }.charged_ticks)
            .sum()
    };
    let alone = smp::userspace_cpus() == 1;
    let before_charge = alone.then(charged);
    let before = timer::ticks();
    // Set under SCHED, so a CPU that queues work later sees it and kicks.
    cpudata::set_idle(true);
    unlocked(|| {
        // SAFETY: No user context or lock is live, and the kernel root is
        // active. STI's interrupt shadow closes the check-to-HLT wakeup race;
        // a CPL0 IRQ only acknowledges and counts, and CLI restores IF=0
        // before SCHED is retaken and any scheduler state touched again.
        unsafe { core::arch::asm!("sti", "hlt", "cli", options(nostack)) };
    });
    cpudata::set_idle(false);
    cpudata::add_idle_ticks(timer::ticks().wrapping_sub(before));
    if let Some(before_charge) = before_charge {
        assert_eq!(charged(), before_charge, "idle charged user CPU ticks");
    }
}

/// Panic (spec 0051, step 5) once a thread of battery process `slot` runs
/// on every AP that runs userspace, announcing the BSP as the panicking CPU:
/// the crash must then stop APs that run user code. With no such AP it
/// panics at once.
#[cfg(feature = "boot-smoke")]
fn panic_while_aps_run(slot: usize) {
    let aps = smp::userspace_mask() & !1;
    // SAFETY: The scheduler holds no other process reference.
    let running = unsafe { process(slot) }
        .threads()
        .filter(|thread| thread.on_cpu)
        .fold(0_u64, |cpus, thread| cpus | 1 << thread.cpu);
    if running & aps == aps {
        serial::println!("MIRROR_TEST_EXPECT_PANIC cpu=0");
        crate::crash::planted_panic();
    }
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
