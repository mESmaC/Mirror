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
    ACTIVE, ACTIVE_INPUT, ACTIVE_IPC, ACTIVE_SURFACE, ACTIVE_WINDOWS, CpuPolicy, MemoryClass,
    Outcome, Process, ShellRuntime, State, Thread, complete_wait, deadline_reached,
    release_handles, resume_process, thread_abi, vm_abi,
};
#[cfg(feature = "boot-smoke")]
use crate::elf;
use crate::{
    arch, heap, input, interrupts,
    ipc::{self, Ipc},
    memory::FrameAllocator,
    paging::PageTables,
    serial, stacks, surface, timer, windows,
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
    /// The scheduler's current completion pass, counted from 1.
    pass: u64,
    /// The pass that queued each thread as just woken, or 0. Dispatch takes
    /// the current pass's threads first (0030-D3).
    woken_in: [u64; THREAD_SLOTS],
}

static PROCESSES: Table<Process, PROCESS_SLOTS, { PROCESS_SLOTS / 64 }> = Table::new();
static THREADS: Table<Thread, THREAD_SLOTS, { THREAD_SLOTS / 64 }> = Table::new();
static RUN_QUEUE: Exclusive<RunQueue> = Exclusive::new(RunQueue {
    head: 0,
    tail: 0,
    length: 0,
    next: [0; THREAD_SLOTS],
    queued: [false; THREAD_SLOTS],
    pass: 0,
    woken_in: [0; THREAD_SLOTS],
});
static WAITS: Exclusive<Waits> = Exclusive::new(Waits::new());
static REPORTED: AtomicBool = AtomicBool::new(false);
/// Set when an event signals a wait during a user slice, so the syscall that
/// signaled returns to the scheduler instead of to its caller (0030-D3).
/// Dispatch clears it.
static RESCHEDULE: AtomicBool = AtomicBool::new(false);

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
}

/// One signaled wait, taken for completion.
#[derive(Clone, Copy)]
pub(super) struct Signal {
    pub(super) slot: u16,
    pub(super) preset: Option<i64>,
    pub(super) at: u64,
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
        }
    }

    /// Signal a wait once, and ask for a reschedule at the next trap exit.
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
        self.links.push(&mut self.list, slot);
        RESCHEDULE.store(true, Ordering::Relaxed);
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
        }
    }
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

/// Remove a thread from the table, the run queue and every wait queue. Its
/// kernel stack, unless its exit already freed it, goes to the reuse pool;
/// only the scheduler and kernel setup remove a thread that still holds one
/// (0050-D2). The rest of the entry is simply dropped.
///
/// # Safety
/// `slot` is occupied and no reference to its entry is live. Its owner drops
/// the slot from its member list.
pub(super) unsafe fn remove_thread(slot: u16) {
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

/// The slot whose entry is `process`, by pointer identity (0054-D5's skip
/// rule). No table entry is borrowed, so the caller's own reference stays
/// unique.
pub(super) fn slot_of(process: &Process) -> usize {
    let pointer = core::ptr::from_ref(process);
    process_slots()
        .find(|&slot| core::ptr::eq(PROCESSES.entry(slot).cast_const(), pointer))
        .expect("live process not in the table")
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

/// Whether an event signaled a wait since the last dispatch, cleared by this
/// read. The syscall that signaled then reschedules instead of returning.
pub(super) fn take_reschedule() -> bool {
    RESCHEDULE.swap(false, Ordering::Relaxed)
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

/// Take a thread off the run queue and every wait queue, as a finishing
/// process does for each of its threads.
pub(super) fn detach(slot: u16) {
    unqueue(slot);
    WAITS.with(|waits| {
        if waits.signals.signaled[usize::from(slot)] {
            waits.signals.take(slot);
        }
        waits.release(slot);
    });
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

/// The oldest signaled wait, if any.
fn next_signal() -> Option<Signal> {
    WAITS.with(|waits| {
        let signals = &mut waits.signals;
        (signals.list.length != 0).then(|| signals.take(signals.list.head))
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

/// A thread that came back from user mode still ready, to be queued again
/// after the next completion pass.
#[derive(Clone, Copy)]
struct Returned {
    slot: u16,
    /// A wake displaced it with quantum left (0030-D3), so it dispatches
    /// before older entries, behind the first eligible thread that pass wakes.
    displaced: bool,
}

/// Run one session until its end rule holds. On return no thread runs, no
/// session pointer is published, and the session's processes, finished or
/// not, stay in the table for the caller to drain.
pub(super) fn run(
    session: &mut Session<'_, '_>,
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    direct_map: u64,
) -> Stats {
    assert!(!arch::interrupts_enabled());
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    // SAFETY: IF=0 and no user context is active. The BSP's guarded RSP0
    // stack is mapped in the shared upper half of every process root.
    unsafe { interrupts::set_kernel_stack(stacks::bsp_rsp0_top()) };
    let mut stats = Stats::default();
    let mut last = None;
    let mut returned: Option<Returned> = None;
    loop {
        assert!(!arch::interrupts_enabled());
        assert_unpublished();
        // 0059-D1: the loop top between dispatches is a heap safe point (BSP,
        // IF=0, kernel root active, no IRQ-needed lock held, no syscall frame
        // live).
        heap::ensure(kernel, frames, direct_map);
        // A finished process's endpoints close before any peer's receive is
        // checked, so a blocked peer observes closure (readme, IPC). Every
        // thread context was saved on its way back here, so the process then
        // gives up its thread entries and stays a zombie until reaped
        // (0030-D5).
        for slot in process_slots() {
            // SAFETY: The scheduler holds no other process reference.
            let process = unsafe { process(slot) };
            release_handles(process, &mut session.ipc);
            if process.finished() && !process.zombie() {
                process.release_threads();
            }
        }
        if let Some(runtime) = &mut session.runtime {
            runtime.reclaim_finished(frames, &mut session.ipc);
        }
        // 0059-D4 (boot-smoke): the shell session's charge peaks, sampled one
        // loop top after any launch, close or reclaim.
        #[cfg(feature = "boot-smoke")]
        if let End::Shell { shell } = session.end {
            let runtime = session.runtime.as_mut().expect("shell runtime");
            // SAFETY: The scheduler holds no other process reference.
            runtime.sample_charge_peaks(unsafe { process(shell) });
        }
        if session.ended() {
            // Only a finished process is reclaimed above, and a ready thread's
            // process is live, so its slot is still occupied. The caller
            // finishes the session's remaining processes, which unqueues it.
            if let Some(thread) = returned.take() {
                enqueue(thread.slot);
            }
            return stats;
        }
        let now = timer::ticks();
        begin_pass();
        complete_waits(session, now);
        // Queue the returned thread before a shell close can finish its
        // process and take it off the queue: a displaced one at the head,
        // behind this pass's woken threads by their precedence, any other at
        // the tail.
        if let Some(thread) = returned.take() {
            if thread.displaced {
                requeue_displaced(thread.slot);
            } else {
                enqueue(thread.slot);
            }
        }
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
        returned = dispatch(session, kernel, frames, owner, slot);
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

/// Complete the signaled waits in signal order (0030-D2), after every return
/// from user mode and every idle wakeup. Expired deadlines are signaled
/// first. A completion signals nothing, so each thread is taken at most once.
fn complete_waits(session: &mut Session<'_, '_>, now: u64) {
    expire(now);
    // The flag is cleared, with IF=0, before the device-input waiters are
    // rechecked, so input that arrives later sets it again. Drain work that
    // a PS/2 or vmmouse bound deferred continues in the waiters' recheck.
    let arrived = input::take_arrival();
    if arrived || crate::ps2::drain_pending() {
        signal_queue(Queue::Input);
    }
    for _ in 0..THREAD_SLOTS {
        let Some(signal) = next_signal() else {
            return;
        };
        let slot = signal.slot;
        // SAFETY: A signaled thread is occupied, since finishing a process
        // detaches its threads, and the scheduler holds no reference to it.
        let owner = usize::from(unsafe { (*thread_entry(slot)).process });
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
    }
    assert!(
        next_signal().is_none(),
        "a completion signaled another wait"
    );
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

/// Run one thread until it returns to the scheduler. A thread that comes back
/// still ready is returned, for the next pass to queue again.
fn dispatch(
    session: &mut Session<'_, '_>,
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    owner: usize,
    slot: u16,
) -> Option<Returned> {
    // SAFETY: The thread's owner is occupied, and the scheduler holds no other
    // reference to it. resume_process publishes it only across user entry.
    let process = unsafe { process(owner) };
    process.current = process
        .members
        .iter()
        .position(|&member| member == slot)
        .expect("queued thread has no owner");
    let access = process.access;
    // This pass completed the waits that scheduler work signaled, such as
    // closing a finished process's endpoints. Only the slice's own signals
    // may reschedule it.
    RESCHEDULE.store(false, Ordering::Relaxed);
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
    // A preempted, yielding, displaced or completed thread rejoins the queue
    // after the next completion pass, behind the threads it wakes.
    let thread = process.thread();
    (thread.state == State::Ready).then_some(Returned {
        slot,
        displaced: thread.displaced,
    })
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
