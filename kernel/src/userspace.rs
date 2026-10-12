//! Native processes, their sessions and syscalls, with finite probes and
//! periodically limited services. The scheduler and its process and thread
//! tables are in `sched` (kernel/src/sched.rs, spec 0030a). IRQ/syscall
//! handlers return to the scheduler before another thread resumes. User
//! buffers are checked and copied through the direct map before use.

// A child module, so the scheduler shares these private types (spec 0030a).
#[path = "sched.rs"]
mod sched;

#[cfg(feature = "boot-smoke")]
use core::sync::atomic::AtomicBool;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::{
    arch,
    capability::{self, CapError, Capability, HandleTable, Object},
    context::Context,
    elf, framebuffer, heap, input,
    interrupts::{self, TrapFrame},
    ipc::{Ipc, IpcError, MESSAGE_SIZE},
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PageTables, PagingError, UserAddressSpace, UserPermissions},
    ps2, serial, surface, timer, vm, windows,
};
use mirror_clock_protocol as clock_abi;
use mirror_thread_protocol as thread_abi;
use mirror_vm_protocol as vm_abi;

const STACK_BASE: u64 = 0x7fff_0000;
const STACK_TOP: u64 = 0x8000_0000;
const MAX_WRITE: usize = 256;
#[cfg(feature = "boot-smoke")]
const CHECKS_PASSED: &[u8] = b"Mirror init: userspace checks passed\n";
#[cfg(feature = "boot-smoke")]
const IPC_CHECKS_PASSED: &[u8] = b"Mirror init: IPC checks passed\n";
#[cfg(feature = "boot-smoke")]
const SURFACE_CHECKS_PASSED: &[u8] = b"Mirror init: surface checks passed\n";
#[cfg(feature = "boot-smoke")]
const SURFACE_DENIED_CHECKS: &[u8] = b"Mirror init: surface denied\n";
#[cfg(feature = "boot-smoke")]
const PROCESS_COUNT: usize = 2;
const QUANTUM_TICKS: u64 = 2;
// Finite init probes include register/preemption loops and 20 thread creations,
// plus 15 more in role 0's capacity check and 8 in its wait-queue check (spec
// 0030a). Leave headroom for the complete debug workload, including IRQs
// delivered on syscall return. This finite lifetime allowance is shared by all
// its threads; it does not change the independent recurring budgets of
// ordinary services.
const USER_TICK_BUDGET: u64 = 128;
const MAX_SLEEP_TICKS: u64 = 1000;
const RECEIVE_TIMEOUT: u64 = 100;
const SERVICE_ALLOWANCE: u64 = 32;
const SERVICE_PERIOD: u64 = 100;

// Only the BSP accesses this slot, with IF=0. It is populated immediately before
// entry and cleared after the synchronous return; it never owns the context.
static ACTIVE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_IPC: AtomicPtr<Ipc> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_SURFACE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_INPUT: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_WINDOWS: AtomicPtr<windows::Session> = AtomicPtr::new(core::ptr::null_mut());
static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static NEXT_TID: AtomicU64 = AtomicU64::new(1);
/// Arms acceptance test 4's charge leak probe for the next run_pair drain
/// (0059; boot-smoke only, one-shot).
#[cfg(feature = "boot-smoke")]
static CHARGE_PROBE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Exit(u64),
    Fault {
        vector: u64,
        error: u64,
        address: u64,
        rip: u64,
    },
    InvalidStack(u64),
    InvalidContext {
        rip: u64,
        cs: u64,
        ss: u64,
    },
    InvalidTls {
        fs: u64,
        gs: u64,
        fs_selector: u64,
        gs_selector: u64,
    },
    BudgetExhausted,
    SessionEnded,
    Terminated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ready,
    Running,
    Sleeping(u64),
    Receiving(PendingReceive),
    InputWaiting(PendingInput),
    Launching(u64),
    Closing(u64),
    Memory(MemoryRequest),
    Joining(u64),
    Waiting(PendingWait),
    Exited(u64),
    Finished,
}

/// A thread-table entry (spec 0030a). It belongs to the process in slot
/// `process`, which reaches it through its member list.
struct Thread {
    id: u64,
    /// The owning process's table slot.
    process: u16,
    /// This thread's own table slot, its run-queue key.
    slot: u16,
    context: Context,
    state: State,
    joiner: Option<u64>,
    charged_ticks: u64,
    sleep_charge: u64,
    quantum_left: u64,
    /// A wake displaced this thread with quantum left, so its next dispatch
    /// keeps `quantum_left` instead of starting a new quantum (0030-D3).
    displaced: bool,
    /// The wake whose latency the kernel times for init's battery.
    #[cfg(feature = "boot-smoke")]
    probe: Option<LatencyProbe>,
    /// When the timed wake became due, until the thread's next dispatch.
    #[cfg(feature = "boot-smoke")]
    due: Option<u64>,
}

/// A wake that role 0 of init's battery asks the kernel to time (spec 0030a,
/// acceptance 4). Only the thread that writes the oracle line is flagged.
#[cfg(feature = "boot-smoke")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LatencyProbe {
    /// The next sleep: from its deadline to the sleeper's dispatch.
    Sleep,
    /// The next blocked receive: from the sending syscall's trap exit to the
    /// receiver's dispatch.
    Receive,
    /// The next displacement by a wake: the thread must resume with the
    /// rest of its quantum.
    Spin,
}

impl Thread {
    fn new(entry: u64, stack: u64, argument: u64, process: usize, slot: u16) -> Self {
        let id = NEXT_TID.fetch_add(1, Ordering::Relaxed);
        assert!((1..=i64::MAX as u64).contains(&id), "thread IDs exhausted");
        Self {
            id,
            process: u16::try_from(process).expect("process slot"),
            slot,
            context: Context::new(entry, stack, argument),
            state: State::Ready,
            joiner: None,
            charged_ticks: 0,
            sleep_charge: 0,
            quantum_left: QUANTUM_TICKS,
            displaced: false,
            #[cfg(feature = "boot-smoke")]
            probe: None,
            #[cfg(feature = "boot-smoke")]
            due: None,
        }
    }

    /// Mark a blocked thread ready and append it to the run queue as woken
    /// in this completion pass. New threads start ready and are queued by
    /// their creator.
    fn wake(&mut self) {
        assert!(
            !matches!(
                self.state,
                State::Ready | State::Running | State::Exited(_) | State::Finished
            ),
            "woke a thread that was not waiting: {:?}",
            self.state
        );
        self.state = State::Ready;
        sched::enqueue_woken(self.slot);
    }
}

/// The session objects a process may use while it runs: the surface (both
/// processes of init's pair, gated by the handle; the shell), physical input
/// (the shell) and the app windows (the shell and its apps).
#[derive(Clone, Copy, Default)]
struct Access {
    surface: bool,
    input: bool,
    windows: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryRequest {
    Operation,
    Fault {
        address: u64,
        error: u64,
    },
    SyscallBuffer {
        address: u64,
        length: usize,
        writable: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingReceive {
    handle: u64,
    address: u64,
    capacity: usize,
    deadline: u64,
    /// The channel whose queue the receiver waits on.
    channel: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingInput {
    handle: u64,
    address: u64,
    deadline: u64,
    /// Device input or one app's routed input.
    queue: sched::Queue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingWait {
    address: u64,
    deadline: Option<u64>,
}

impl PendingWait {
    fn expired(self, now: u64) -> bool {
        self.deadline
            .is_some_and(|deadline| deadline_reached(now, deadline))
    }
}

#[derive(Clone, Copy, Debug)]
struct ServicePolicy {
    window_start: u64,
    used: u64,
}

impl ServicePolicy {
    fn new(now: u64) -> Self {
        Self {
            window_start: now,
            used: 0,
        }
    }

    fn refresh(&mut self, now: u64) {
        let elapsed = now.wrapping_sub(self.window_start);
        if (SERVICE_PERIOD..(1_u64 << 63)).contains(&elapsed) {
            // Advance complete anchored periods. Sleeping through many periods
            // earns one allowance, never accumulated credit or a shifted window.
            self.window_start = now.wrapping_sub(elapsed % SERVICE_PERIOD);
            self.used = 0;
        }
    }

    fn eligible(&mut self, now: u64) -> bool {
        self.refresh(now);
        self.used < SERVICE_ALLOWANCE
    }

    fn charge(&mut self, now: u64) -> bool {
        self.refresh(now);
        assert!(self.used < SERVICE_ALLOWANCE, "throttled service resumed");
        self.used += 1;
        self.used == SERVICE_ALLOWANCE
    }
}

#[derive(Clone, Copy, Debug)]
enum CpuPolicy {
    Probe,
    Shell(ServicePolicy),
    Application(ServicePolicy),
}

// Fields gated on `boot-smoke` are bookkeeping that only init's self-test
// battery reads. Fields gated on `test-trace` feed only trace markers. Neither
// exists in a default kernel (spec 0010).
struct Process {
    pid: u64,
    space: UserAddressSpace,
    vm: vm::Vm,
    direct_map: u64,
    access: Access,
    #[cfg(feature = "boot-smoke")]
    vm_checks: bool,
    #[cfg(feature = "boot-smoke")]
    demand_faults: u64,
    #[cfg(feature = "boot-smoke")]
    copy_faults: u64,
    /// Thread-table slots of this process's threads by member index, or
    /// NO_SLOT. The index is the process-local position that thread
    /// creation fills lowest-first, up to MAX_THREADS.
    members: [u16; thread_abi::MAX_THREADS],
    /// The member index of the thread being run or completed.
    current: usize,
    #[cfg(feature = "boot-smoke")]
    thread_checks: bool,
    #[cfg(feature = "boot-smoke")]
    threads_created: u64,
    #[cfg(feature = "boot-smoke")]
    threads_peak: usize,
    #[cfg(feature = "boot-smoke")]
    capacity_checks: bool,
    #[cfg(feature = "boot-smoke")]
    wait_queue_checks: bool,
    #[cfg(feature = "boot-smoke")]
    threads_joined: u64,
    #[cfg(feature = "boot-smoke")]
    threads_exited: u64,
    #[cfg(feature = "boot-smoke")]
    threads_cancelled: u64,
    #[cfg(feature = "boot-smoke")]
    thread_preemptions: u64,
    #[cfg(feature = "boot-smoke")]
    sync_checks: bool,
    #[cfg(feature = "boot-smoke")]
    sync_waits: u64,
    #[cfg(feature = "boot-smoke")]
    sync_wakes: u64,
    #[cfg(feature = "boot-smoke")]
    sync_timeouts: u64,
    #[cfg(feature = "boot-smoke")]
    sync_invalidations: u64,
    #[cfg(feature = "boot-smoke")]
    sync_cancelled: u64,
    #[cfg(feature = "boot-smoke")]
    tls_checks: bool,
    #[cfg(feature = "boot-smoke")]
    clock_checks: bool,
    #[cfg(feature = "boot-smoke")]
    clock_unavailable: bool,
    #[cfg(feature = "boot-smoke")]
    fpu_checks: bool,
    #[cfg(feature = "boot-smoke")]
    fpu_preemption_start: Option<usize>,
    #[cfg(feature = "boot-smoke")]
    fpu_preemptions: usize,
    /// Ticks from a flagged sleeper's deadline to its dispatch.
    #[cfg(feature = "boot-smoke")]
    wake_lateness: Option<u64>,
    /// Ticks from a flagged receive's sender trap exit to its dispatch.
    #[cfg(feature = "boot-smoke")]
    ipc_lateness: Option<u64>,
    /// A flagged spinner resumed with the rest of its quantum after a wake
    /// displaced it.
    #[cfg(feature = "boot-smoke")]
    spin_kept_quantum: bool,
    outcome: Option<Outcome>,
    #[cfg(feature = "boot-smoke")]
    checks_passed: bool,
    #[cfg(feature = "boot-smoke")]
    bad_pointers: usize,
    #[cfg(feature = "boot-smoke")]
    unknown_calls: usize,
    dispatches: usize,
    #[cfg(feature = "boot-smoke")]
    yields: usize,
    #[cfg(feature = "boot-smoke")]
    sleeps: usize,
    charged_ticks: u64,
    policy: CpuPolicy,
    throttles: u64,
    preemptions: usize,
    #[cfg(feature = "boot-smoke")]
    post_checks_preemptions: usize,
    handles: HandleTable,
    boot_handles: [u64; 6],
    handles_closed: bool,
    /// The surface pixel frames charged to this process (0059-D3; the shell
    /// carries its apps' surfaces). Maintained in every flavor so 0054a can
    /// consume the counters; boot-smoke only: the zero-at-reap check.
    surface_frames: u64,
    #[cfg(feature = "boot-smoke")]
    ipc_checks: bool,
    ipc_sent: usize,
    ipc_received: usize,
    #[cfg(feature = "boot-smoke")]
    ipc_waits: usize,
    #[cfg(feature = "boot-smoke")]
    ipc_closed_wakes: usize,
    #[cfg(feature = "boot-smoke")]
    ipc_timeouts: usize,
    #[cfg(feature = "boot-smoke")]
    surface_checks: bool,
    #[cfg(feature = "boot-smoke")]
    surface_denied_checks: bool,
    #[cfg(feature = "boot-smoke")]
    surface_info_reads: u64,
    surface_tiles: u64,
    surface_pixels: u64,
    #[cfg(feature = "boot-smoke")]
    foreign_surface: u64,
    #[cfg(feature = "boot-smoke")]
    foreign_surface_denials: u8,
    #[cfg(feature = "test-trace")]
    desktop_ready: bool,
    desktop_checks: bool,
    #[cfg(feature = "test-trace")]
    desktop_revision: u64,
    input_events: u64,
    input_lost: u64,
    input_waits: u64,
    input_timeouts: u64,
}

impl Process {
    /// The member thread at `index`, if any.
    fn member(&self, index: usize) -> Option<&Thread> {
        let slot = self.members[index];
        // SAFETY: A member slot is an occupied thread entry that only this
        // process reaches, through these accessors, which tie the borrow to
        // the process's own. The scheduler reads entries directly only while
        // it holds no process reference.
        (slot != sched::NO_SLOT).then(|| unsafe { &*sched::thread_entry(slot) })
    }

    /// The member thread at `index`, if any, mutably.
    fn member_mut(&mut self, index: usize) -> Option<&mut Thread> {
        let slot = self.members[index];
        // SAFETY: As in `member`; `&mut self` makes this the only borrow.
        (slot != sched::NO_SLOT).then(|| unsafe { &mut *sched::thread_entry(slot) })
    }

    /// Every member thread, in member order.
    fn threads(&self) -> impl Iterator<Item = &Thread> {
        (0..thread_abi::MAX_THREADS).filter_map(|index| self.member(index))
    }

    /// Remove the member at `index` from this process and the thread table.
    fn remove_member(&mut self, index: usize) {
        let slot = core::mem::replace(&mut self.members[index], sched::NO_SLOT);
        assert_ne!(slot, sched::NO_SLOT, "removed an empty member");
        // SAFETY: The slot was this process's member, and no reference to
        // it is live: member borrows are tied to this `&mut self`.
        unsafe { sched::remove_thread(slot) };
    }

    fn thread(&self) -> &Thread {
        self.member(self.current).expect("current thread")
    }

    fn thread_mut(&mut self) -> &mut Thread {
        self.member_mut(self.current).expect("current thread")
    }

    fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    /// Record the outcome and finish every thread, taking each off the run
    /// queue and every wait queue. Threads keep their slots until the process
    /// is reaped.
    fn finish(&mut self, outcome: Outcome) {
        assert!(self.outcome.is_none());
        self.outcome = Some(outcome);
        for index in 0..thread_abi::MAX_THREADS {
            let Some(thread) = self.member_mut(index) else {
                continue;
            };
            let state = core::mem::replace(&mut thread.state, State::Finished);
            sched::detach(thread.slot);
            #[cfg(feature = "boot-smoke")]
            {
                if !matches!(state, State::Exited(_)) {
                    self.threads_cancelled += 1;
                }
                if matches!(state, State::Waiting(_)) {
                    self.sync_cancelled += 1;
                }
            }
            #[cfg(not(feature = "boot-smoke"))]
            let _ = state;
        }
    }

    /// 0059-D3 (boot-smoke): at reap, after the address space is destroyed,
    /// every charged frame must be back: no surface frames, no page tables.
    #[cfg(feature = "boot-smoke")]
    fn check_charges_zero(&self) {
        assert_eq!(self.surface_frames, 0, "surface frames leaked at reap");
        assert_eq!(
            self.space.owned_table_count(),
            0,
            "page-table frames leaked at reap"
        );
    }

    /// The reporting form of the reap check, for acceptance test 4's
    /// leak-repair probe (boot-smoke).
    #[cfg(feature = "boot-smoke")]
    fn charges_zero(&self) -> bool {
        self.surface_frames == 0 && self.space.owned_table_count() == 0
    }

    /// Release a finished process's thread entries, once no thread context
    /// is in use. The zombie keeps only its outcome, counters, handles and
    /// address space until its session validates and reaps it (0030-D5).
    fn release_threads(&mut self) {
        assert!(self.finished(), "released a live process's threads");
        for index in 0..thread_abi::MAX_THREADS {
            if self.members[index] != sched::NO_SLOT {
                self.remove_member(index);
            }
        }
    }

    /// A finished process that holds no thread entry.
    fn zombie(&self) -> bool {
        self.finished() && self.members.iter().all(|&slot| slot == sched::NO_SLOT)
    }
}

/// Print `MIRROR_SCHED_READY` once, before the first session, in every
/// flavor (spec 0030a).
pub fn report_scheduler() {
    sched::report_ready();
}

/// Run init's self-test battery. Only smoke kernels run init: default and
/// trace kernels start the shell as the first userspace process instead
/// (spec 0010-D3).
#[cfg(feature = "boot-smoke")]
pub fn run_init(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    direct_map: u64,
    scenario: u64,
    mut display: Option<&mut framebuffer::Driver<'_>>,
    desktop_follows: bool,
) {
    assert!(!arch::interrupts_enabled());
    let image = elf::parse(bytes).expect("invalid native init ELF");
    capability::self_test();
    crate::ipc::self_test();
    service_policy_self_test();
    assert!(!deadline_reached(4, 5));
    assert!(deadline_reached(5, 5));
    assert!(deadline_reached(6, 5));
    assert!(!deadline_reached(u64::MAX - 2, 3));
    assert!(!deadline_reached(2, 3));
    assert!(deadline_reached(3, u64::MAX - 2));
    test_load_rollback(kernel, frames, bytes, &image, direct_map);
    vm::self_test(kernel, frames, direct_map);
    table_bounds_self_test(kernel, frames, bytes, &image, direct_map);
    run_pair(
        kernel,
        frames,
        bytes,
        &image,
        direct_map,
        scenario,
        display.as_deref_mut(),
    );
    // Relaunch from the same ELF after teardown. Fresh BSS and successful entry
    // exercise frame reuse and continuation recovery after both exit and fault.
    // 0059-D1: a session's seeding is a heap safe point.
    heap::ensure(kernel, frames, direct_map);
    // 0059 test 2's second step: the relaunch pair holds the heap near-full at
    // its first loop top, so its next loop-top ensure grows mid-session.
    #[cfg(feature = "boot-smoke")]
    heap::arm_hold_across_session();
    // 0059 acceptance test 4: when no desktop session follows, this pair's
    // drain is the boot's last, so it runs the charge leak probe.
    if !desktop_follows {
        CHARGE_PROBE.store(true, Ordering::Relaxed);
    }
    run_pair(kernel, frames, bytes, &image, direct_map, 0, display);
}

#[cfg(feature = "boot-smoke")]
fn test_load_rollback(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
) {
    let before = frames.stats().unwrap().allocated_frames;
    // SAFETY: Same exclusive, inactive process construction as run_pair.
    let mut space = unsafe { UserAddressSpace::new(kernel, frames) }.unwrap();
    // Force a real map failure after ELF pages have already been installed.
    map_initialized(
        &mut space,
        frames,
        direct_map,
        STACK_BASE,
        UserPermissions::READ_WRITE,
        &[],
    )
    .unwrap();
    assert_eq!(
        load(&mut space, frames, bytes, image, direct_map),
        Err(PagingError::AlreadyMapped)
    );
    assert!(
        space.owned_page_count() > 1,
        "load failed before installing image pages"
    );
    // SAFETY: This root has never been active and no references escaped load.
    unsafe { space.destroy(frames) }.unwrap();
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before,
        "partial load leaked"
    );
    serial::println!("MIRROR_USER_LOAD_ROLLBACK_OK");
}

/// Fill the process and thread tables, check that both refuse more with
/// EAGAIN, and check that tearing every entry down restores the frame
/// baseline exactly (spec 0030a, acceptance 5). Case 1 fills both tables
/// with 64 processes of four threads, finishes them into zombies, which give
/// up every thread entry but keep their process slots, and reaps them. Case 2
/// fills only the thread table, with 16 processes of sixteen threads, so a
/// process create fails for want of a main-thread entry before it allocates
/// anything. No entry ever runs: threads are created ready and queued.
#[cfg(feature = "boot-smoke")]
fn table_bounds_self_test(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
) {
    let allocated =
        |frames: &FrameAllocator<'_>| frames.stats().expect("recycling required").allocated_frames;
    sched::assert_drained();
    let baseline = allocated(frames);
    let start = thread_abi::ThreadStart {
        entry: image.entry,
        stack: STACK_TOP - 8,
        argument: 0,
        fs: 0,
        gs: 0,
    };
    let mut slots = [0; sched::PROCESS_SLOTS];

    // Case 1: both tables full.
    for slot in &mut slots {
        *slot = load_process(kernel, frames, bytes, image, direct_map, 0)
            .expect("table-bounds process");
        // SAFETY: A fresh slot that nothing else references.
        let process = unsafe { sched::process(*slot) };
        for _ in 1..sched::THREAD_SLOTS / sched::PROCESS_SLOTS {
            add_thread(process, *slot, &start).expect("table-bounds thread");
        }
    }
    assert!(
        !sched::has_room(1, 0) && !sched::has_room(0, 1),
        "the tables did not fill"
    );
    let full = allocated(frames);
    let refused = load_process(kernel, frames, bytes, image, direct_map, 0);
    assert_eq!(
        refused.map_err(|error| error.errno()),
        Err(-11),
        "a full process table took a process"
    );
    // SAFETY: An occupied slot that nothing else references.
    let process = unsafe { sched::process(slots[0]) };
    assert_eq!(
        add_thread(process, slots[0], &start),
        Err(-11),
        "a full thread table took a thread"
    );
    assert_eq!(allocated(frames), full, "a refused create allocated frames");
    for &slot in &slots {
        // SAFETY: As above; one entry is borrowed at a time.
        let process = unsafe { sched::process(slot) };
        process.finish(Outcome::Terminated);
        process.release_threads();
        assert!(process.zombie());
    }
    assert!(
        sched::has_room(0, sched::THREAD_SLOTS) && !sched::has_room(1, 0),
        "zombies kept thread entries or gave up process slots"
    );
    let refused = load_process(kernel, frames, bytes, image, direct_map, 0);
    assert_eq!(
        refused.map_err(|error| error.errno()),
        Err(-11),
        "a table of zombies took a process"
    );
    for &slot in &slots {
        // SAFETY: No reference to the zombie is live.
        let mut zombie = unsafe { sched::reap(slot) };
        assert_eq!(zombie.outcome, Some(Outcome::Terminated));
        // SAFETY: Its root has never been active, and nothing references
        // its tree.
        unsafe { zombie.space.destroy(frames) }.expect("table-bounds teardown");
    }
    sched::assert_drained();
    assert_eq!(allocated(frames), baseline, "table-bounds frame leak");

    // Case 2: only the thread table full.
    let count = sched::THREAD_SLOTS / thread_abi::MAX_THREADS;
    for slot in &mut slots[..count] {
        *slot = load_process(kernel, frames, bytes, image, direct_map, 0)
            .expect("table-bounds process");
        // SAFETY: A fresh slot that nothing else references.
        let process = unsafe { sched::process(*slot) };
        for _ in 1..thread_abi::MAX_THREADS {
            add_thread(process, *slot, &start).expect("table-bounds thread");
        }
        assert_eq!(
            add_thread(process, *slot, &start),
            Err(-11),
            "a full process took a thread"
        );
    }
    let free = sched::PROCESS_SLOTS - count;
    assert!(
        sched::has_room(free, 0) && !sched::has_room(free + 1, 0) && !sched::has_room(0, 1),
        "only the thread table should be full"
    );
    let full = allocated(frames);
    let refused = load_process(kernel, frames, bytes, image, direct_map, 0);
    assert_eq!(
        refused.map_err(|error| error.errno()),
        Err(-11),
        "a full thread table took a process"
    );
    assert_eq!(allocated(frames), full, "a refused create allocated frames");
    for &slot in &slots[..count] {
        // SAFETY: The process never ran, and nothing references it.
        let mut process = unsafe { sched::discard(slot) };
        // SAFETY: Its root has never been active, and nothing references
        // its tree.
        unsafe { process.space.destroy(frames) }.expect("table-bounds teardown");
    }
    sched::assert_drained();
    assert_eq!(allocated(frames), baseline, "table-bounds frame leak");
    serial::println!(
        "MIRROR_SCHED_BOUNDS_OK processes={} threads={}",
        sched::PROCESS_SLOTS,
        sched::THREAD_SLOTS
    );
}

#[derive(Debug)]
enum CreateError {
    /// The process or thread table has no room (EAGAIN, 0030-D4).
    TableFull,
    Paging(#[expect(dead_code, reason = "load panics print it through Debug")] PagingError),
}

impl CreateError {
    /// The error a launch returns to its caller.
    fn errno(&self) -> i64 {
        match self {
            Self::TableFull => -11,
            Self::Paging(_) => -12,
        }
    }
}

/// Load a process into the process table, with its main thread queued, and
/// return its slot. Room for both entries is checked before anything is
/// allocated; on failure no entry exists and every frame the load took is
/// released. The caller seeds handles, policy and access before it runs.
fn create_process(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
    argument: u64,
) -> Result<usize, CreateError> {
    let slot = load_process(kernel, frames, bytes, image, direct_map, argument)?;
    // SAFETY: The slot was just filled, and nothing else references it.
    let process = unsafe { sched::process(slot) };
    serial::println!(
        "Native process: pid={} entry={:#x} root={:#x} pages={} tables={}",
        process.pid,
        image.entry,
        process.space.root_physical_address(),
        process.space.owned_page_count(),
        process.space.owned_table_count()
    );
    Ok(slot)
}

/// `create_process` without its report, which the table-bounds self-test
/// would otherwise print for every entry it fills.
fn load_process(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
    argument: u64,
) -> Result<usize, CreateError> {
    if !sched::has_room(1, 1) {
        return Err(CreateError::TableFull);
    }
    // SAFETY: The BSP is alone with IF=0, kernel root active, and the shared
    // upper mappings and unique allocator remain live throughout this run.
    // The kernel tables live in continue_boot's frame, which never returns.
    let mut space =
        unsafe { UserAddressSpace::new(kernel, frames) }.map_err(CreateError::Paging)?;
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    assert!(
        (1..=i64::MAX as u64).contains(&pid),
        "process IDs exhausted"
    );
    if let Err(error) = load(&mut space, frames, bytes, image, direct_map) {
        // SAFETY: This root has never been active; mapped frames belong to it.
        unsafe { space.destroy(frames) }.expect("failed-load cleanup");
        return Err(CreateError::Paging(error));
    }
    let slot = sched::insert_process(Process {
        pid,
        space,
        vm: vm::Vm::new(),
        direct_map,
        access: Access::default(),
        #[cfg(feature = "boot-smoke")]
        vm_checks: false,
        #[cfg(feature = "boot-smoke")]
        demand_faults: 0,
        #[cfg(feature = "boot-smoke")]
        copy_faults: 0,
        members: [sched::NO_SLOT; thread_abi::MAX_THREADS],
        current: 0,
        #[cfg(feature = "boot-smoke")]
        thread_checks: false,
        #[cfg(feature = "boot-smoke")]
        threads_created: 1,
        #[cfg(feature = "boot-smoke")]
        threads_peak: 1,
        #[cfg(feature = "boot-smoke")]
        capacity_checks: false,
        #[cfg(feature = "boot-smoke")]
        wait_queue_checks: false,
        #[cfg(feature = "boot-smoke")]
        threads_joined: 0,
        #[cfg(feature = "boot-smoke")]
        threads_exited: 0,
        #[cfg(feature = "boot-smoke")]
        threads_cancelled: 0,
        #[cfg(feature = "boot-smoke")]
        thread_preemptions: 0,
        #[cfg(feature = "boot-smoke")]
        sync_checks: false,
        #[cfg(feature = "boot-smoke")]
        sync_waits: 0,
        #[cfg(feature = "boot-smoke")]
        sync_wakes: 0,
        #[cfg(feature = "boot-smoke")]
        sync_timeouts: 0,
        #[cfg(feature = "boot-smoke")]
        sync_invalidations: 0,
        #[cfg(feature = "boot-smoke")]
        sync_cancelled: 0,
        #[cfg(feature = "boot-smoke")]
        tls_checks: false,
        #[cfg(feature = "boot-smoke")]
        clock_checks: false,
        #[cfg(feature = "boot-smoke")]
        clock_unavailable: false,
        #[cfg(feature = "boot-smoke")]
        fpu_checks: false,
        #[cfg(feature = "boot-smoke")]
        fpu_preemption_start: None,
        #[cfg(feature = "boot-smoke")]
        fpu_preemptions: 0,
        #[cfg(feature = "boot-smoke")]
        wake_lateness: None,
        #[cfg(feature = "boot-smoke")]
        ipc_lateness: None,
        #[cfg(feature = "boot-smoke")]
        spin_kept_quantum: false,
        outcome: None,
        #[cfg(feature = "boot-smoke")]
        checks_passed: false,
        #[cfg(feature = "boot-smoke")]
        bad_pointers: 0,
        #[cfg(feature = "boot-smoke")]
        unknown_calls: 0,
        dispatches: 0,
        #[cfg(feature = "boot-smoke")]
        yields: 0,
        #[cfg(feature = "boot-smoke")]
        sleeps: 0,
        charged_ticks: 0,
        policy: CpuPolicy::Probe,
        throttles: 0,
        preemptions: 0,
        #[cfg(feature = "boot-smoke")]
        post_checks_preemptions: 0,
        handles: HandleTable::new(),
        boot_handles: [0; 6],
        handles_closed: false,
        surface_frames: 0,
        #[cfg(feature = "boot-smoke")]
        ipc_checks: false,
        ipc_sent: 0,
        ipc_received: 0,
        #[cfg(feature = "boot-smoke")]
        ipc_waits: 0,
        #[cfg(feature = "boot-smoke")]
        ipc_closed_wakes: 0,
        #[cfg(feature = "boot-smoke")]
        ipc_timeouts: 0,
        #[cfg(feature = "boot-smoke")]
        surface_checks: false,
        #[cfg(feature = "boot-smoke")]
        surface_denied_checks: false,
        #[cfg(feature = "boot-smoke")]
        surface_info_reads: 0,
        surface_tiles: 0,
        surface_pixels: 0,
        #[cfg(feature = "boot-smoke")]
        foreign_surface: 0,
        #[cfg(feature = "boot-smoke")]
        foreign_surface_denials: 0,
        #[cfg(feature = "test-trace")]
        desktop_ready: false,
        desktop_checks: false,
        #[cfg(feature = "test-trace")]
        desktop_revision: 0,
        input_events: 0,
        input_lost: 0,
        input_waits: 0,
        input_timeouts: 0,
    });
    let thread = sched::insert_thread(|thread| {
        Thread::new(image.entry, STACK_TOP - 8, argument, slot, thread)
    });
    // SAFETY: The slot was just filled, and nothing else references it.
    let process = unsafe { sched::process(slot) };
    process.members[0] = thread;
    sched::enqueue(thread);
    Ok(slot)
}

#[cfg(feature = "boot-smoke")]
fn run_pair(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
    scenario: u64,
    display: Option<&mut framebuffer::Driver<'_>>,
) {
    // The framebuffer borrow belongs to this whole session, never to a user
    // mapping or handle. Only syscall handlers may reborrow it during resume.
    let surface = display
        .map(surface::Session::new)
        .transpose()
        .expect("surface session");
    let before = frames.stats().expect("recycling required").allocated_frames;
    let heap_before = heap::stats().size;
    let first = create_process(kernel, frames, bytes, image, direct_map, scenario)
        .expect("load first native process");
    let second = match create_process(kernel, frames, bytes, image, direct_map, 1 << 8) {
        Ok(second) => second,
        Err(error) => {
            // SAFETY: The first process has never run and nothing references
            // it; the failed creation already released its own frames.
            let mut process = unsafe { sched::discard(first) };
            // SAFETY: Its root has never been active, and no handler or
            // other reference reaches its tree.
            unsafe { process.space.destroy(frames) }.expect("peer-load cleanup");
            assert_eq!(frames.stats().unwrap().allocated_frames, before);
            panic!("could not load peer: {error:?}");
        }
    };
    // 0059-D4: the pair's charge peak is set at seeding; nothing here charges
    // surfaces. Printed at the drain (boot-smoke).
    #[cfg(feature = "boot-smoke")]
    let tables_peak: usize = [first, second]
        .into_iter()
        .map(|slot| {
            // SAFETY: Seeding holds one process reference at a time.
            unsafe { sched::process(slot) }.space.owned_table_count()
        })
        .sum();
    let mut ipc = Ipc::new();
    for (index, slot) in [first, second].into_iter().enumerate() {
        // SAFETY: Seeding holds one process reference at a time.
        let process = unsafe { sched::process(slot) };
        // Each process can send only to its peer's inbound channel. Console
        // access is an explicit bootstrap capability, independent of IPC rights.
        let grants = [
            (Object::Console, capability::WRITE),
            (Object::Channel(1 - index), capability::SEND),
            (Object::Channel(index), capability::RECEIVE),
        ];
        for (slot, (object, rights)) in grants.into_iter().enumerate() {
            let handle = process
                .handles
                .insert(object, rights)
                .expect("bootstrap capability");
            ipc.grant(Capability { object, rights });
            process.boot_handles[slot] = handle;
        }
        assert_eq!(process.handles.len(), 3);
        // Both processes see the surface session; only the handle grants use.
        process.access.surface = surface.is_some();
    }
    if surface.is_some() {
        // SAFETY: Seeding holds one process reference at a time.
        let owner = unsafe { sched::process(first) };
        let handle = owner
            .handles
            .insert(Object::Surface, capability::WRITE)
            .expect("bootstrap surface capability");
        owner.boot_handles[3] = handle;
        assert_eq!(owner.handles.len(), 4);
        // Trusted verification knows the owner's token but grants no entry to
        // the peer. Its real IPC fixture must receive and try this exact token.
        // SAFETY: As above; the owner's borrow has ended.
        unsafe { sched::process(second) }.foreign_surface = handle;
    }
    {
        // SAFETY: Two distinct occupied slots, and no other reference.
        let (owner, peer) = unsafe { (sched::process(first), sched::process(second)) };
        assert_eq!(peer.handles.len(), 3);
        assert_eq!(peer.boot_handles[3], 0);
        assert!(owner.boot_handles[4] == 0 && peer.boot_handles[4] == 0);
        assert_ne!(owner.pid, peer.pid);
        assert_ne!(
            owner.space.root_physical_address(),
            peer.space.root_physical_address()
        );
        assert_ne!(
            owner.space.translate(STACK_BASE).unwrap().unwrap().physical,
            peer.space.translate(STACK_BASE).unwrap().unwrap().physical
        );
    }
    #[cfg(feature = "boot-smoke")]
    let held = heap::test_hold_begin();
    let mut session = sched::Session {
        ipc,
        surface,
        input: None,
        runtime: None,
        end: sched::End::Pair {
            first,
            second,
            scenario,
        },
    };
    let stats = sched::run(&mut session, kernel, frames, direct_map);
    let sched::Session { ipc, surface, .. } = session;
    assert!(ACTIVE_SURFACE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_INPUT.load(Ordering::SeqCst).is_null());
    assert!(
        ipc.is_drained(),
        "IPC messages or endpoint references leaked"
    );
    // Both processes are zombies now: finished, with their handles closed and
    // their thread entries released. Validation reads their outcomes and
    // counters in the table; only then are they reaped (0030-D5).
    {
        // SAFETY: Two distinct occupied slots. The scheduler has returned and
        // ACTIVE is clear, so nothing else references either entry until
        // these shared borrows end with this block, before the reap below.
        let [owner, peer] = unsafe { [sched::process(first), sched::process(second)] };
        let processes: [&Process; 2] = [owner, peer];
        assert!(
            processes
                .iter()
                .all(|process| process.zombie() && process.handles.is_empty()),
            "a finished process kept its threads or handles"
        );
        validate_pair(processes, scenario, stats, surface);
    }
    // SAFETY: Validation's borrows have ended, ACTIVE is clear, and nothing
    // references either entry; reap checks that each finished.
    let mut zombies = unsafe { [sched::reap(first), sched::reap(second)] };
    sched::assert_drained();
    for zombie in &mut zombies {
        // SAFETY: All contexts are finished, ACTIVE is clear, the kernel root
        // is active, and no handler retains a reference into either user tree.
        unsafe { zombie.space.destroy(frames) }.expect("release user address space");
        #[cfg(feature = "boot-smoke")]
        zombie.check_charges_zero();
    }
    // 0059 test 2: the held block returns before the frame check, and the
    // relaunch pair's mid-session growth is exactly one step.
    #[cfg(feature = "boot-smoke")]
    {
        heap::test_hold_end(held);
        if held.is_some() {
            assert_eq!(
                heap::stats().size - heap_before,
                heap::MAX_GROW_PAGES * PAGE_SIZE as usize,
                "the relaunch pair must grow exactly once"
            );
        }
    }
    // Each session ends at its own frame baseline.
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        // 0050-D2's shared rule, heap term: heap pages committed this session
        // are not a leak (a mid-session growth is 0059's test 2's case).
        before + (heap::stats().size - heap_before) / PAGE_SIZE as usize,
        "user frame leak"
    );
    serial::println!("MIRROR_USER_CLEANUP_OK allocated={}", before);
    serial::println!("MIRROR_VM_CLEANUP_OK allocated={}", before);
    // 0059 acceptance test 4: once per boot, the last session's drain leaks
    // one charge, requires the reap check to report it, repairs it, and only
    // then prints the marker. Armed before the relaunch pair when no desktop
    // session follows, making this drain the boot's last.
    if CHARGE_PROBE.swap(false, Ordering::Relaxed) {
        let zombie = &mut zombies[0];
        zombie.surface_frames = 1;
        assert!(!zombie.charges_zero(), "reap check missed a leaked charge");
        zombie.surface_frames = 0;
        assert!(zombie.charges_zero(), "a repaired charge must pass");
    }
    serial::println!(
        "MIRROR_HEAP_CHARGE_OK tables_peak={} surfaces_peak=0",
        tables_peak
    );
}

/// Validate a finished init pair's zombies, which stay in the table until
/// this returns, and report the pair's markers.
#[cfg(feature = "boot-smoke")]
fn validate_pair(
    processes: [&Process; 2],
    scenario: u64,
    stats: sched::Stats,
    surface: Option<surface::Session<'_, '_>>,
) {
    assert_eq!(
        processes[1].surface_tiles, 0,
        "peer painted without authority"
    );
    // Dropping the session releases its display borrow when this branch ends.
    if let Some(session) = surface {
        assert!(
            processes[0].surface_checks,
            "owner surface checks did not complete: {:?}",
            processes[0].outcome
        );
        assert!(
            processes[1].surface_denied_checks,
            "peer surface checks did not complete: {:?}",
            processes[1].outcome
        );
        let (tiles, pixels) = session.totals();
        let info = session.info();
        let width = u64::from(u32::from_le_bytes(info[4..8].try_into().unwrap()));
        let height = u64::from(u32::from_le_bytes(info[8..12].try_into().unwrap()));
        let edge = surface::MAX_TILE as u64;
        // Native init paints the complete owned rectangle, plus three one-pixel
        // edge/read-only-source/duplicate-authority probes before its marker.
        assert_eq!(tiles, width.div_ceil(edge) * height.div_ceil(edge) + 3);
        assert_eq!(pixels, width * height + 3);
        assert_eq!(tiles, processes[0].surface_tiles);
        assert_eq!(pixels, processes[0].surface_pixels);
        assert!(processes[0].surface_info_reads > 0);
        assert_eq!(processes[1].surface_info_reads, 0);
        assert_eq!(
            processes[1].foreign_surface_denials, 3,
            "both foreign surface syscalls must fail"
        );
        serial::println!(
            "MIRROR_USER_SURFACE_OK tiles={} pixels={} foreign_denied={}",
            tiles,
            pixels,
            processes[1].foreign_surface_denials.count_ones()
        );
    } else {
        assert_eq!(processes[0].surface_tiles, 0);
        assert_eq!(processes[0].surface_info_reads, 0);
        assert_eq!(processes[0].boot_handles[3], 0);
        serial::println!("MIRROR_USER_SURFACE_SKIPPED reason=no_display");
    }
    // All tables are empty and the published pointer is revoked before this
    // session releases its exclusive display borrow back to the kernel console.
    assert!(processes.iter().all(|process| process.handles.is_empty()));
    serial::println!("MIRROR_USER_SURFACE_CLEANUP_OK handles_reclaimed=1");
    validate_process(processes[0], scenario);
    validate_process(processes[1], 0);
    // Spec 0030a, acceptance 2: only role 0 fills its thread table, and the
    // kernel saw it hold every slot at once; role 1 keeps the 3-worker battery.
    assert!(
        processes[0].capacity_checks
            && processes[0].threads_peak == thread_abi::MAX_THREADS
            && !processes[1].capacity_checks
            && processes[1].threads_peak < thread_abi::MAX_THREADS,
        "thread capacity checks did not complete: peaks {} and {}",
        processes[0].threads_peak,
        processes[1].threads_peak
    );
    // Acceptance 3: only role 0 checks FIFO wake order, after its capacity
    // check. Each completion asserted that its blocked thread was not charged.
    assert!(
        processes[0].wait_queue_checks && !processes[1].wait_queue_checks,
        "wait queue checks did not complete"
    );
    assert_eq!(
        processes
            .iter()
            .map(|process| process.ipc_sent)
            .sum::<usize>(),
        3
    );
    assert_eq!(
        processes
            .iter()
            .map(|process| process.ipc_received)
            .sum::<usize>(),
        3
    );
    let waits: usize = processes.iter().map(|process| process.ipc_waits).sum();
    let timeouts: usize = processes.iter().map(|process| process.ipc_timeouts).sum();
    assert!(waits > 0, "no receive ever blocked");
    if scenario == 8 {
        assert!(
            processes[1].ipc_closed_wakes > 0,
            "exit did not wake peer receive"
        );
        serial::println!("MIRROR_IPC_PEER_CLOSED_OK");
    }
    if scenario == 9 {
        assert_eq!(timeouts, 1, "receive timeout was not observed exactly once");
        serial::println!("MIRROR_IPC_TIMEOUT_OK");
    } else {
        assert_eq!(timeouts, 0, "unexpected IPC timeout");
    }
    serial::println!(
        "MIRROR_IPC_OK messages=3 waits={} timeouts={} handles_reclaimed=1",
        waits,
        timeouts
    );
    // Acceptance 4: only role 0 times its wakes. A sleeper is dispatched by
    // its deadline's own tick, a receiver as its sender's syscall exits, and
    // the spinner they displace keeps the rest of its quantum. One tick of
    // slack covers an IRQ held across a tick boundary by a critical section.
    assert!(
        processes[1].wake_lateness.is_none()
            && processes[1].ipc_lateness.is_none()
            && !processes[1].spin_kept_quantum,
        "peer timed a wake"
    );
    let (Some(wake_lateness), Some(ipc_lateness)) =
        (processes[0].wake_lateness, processes[0].ipc_lateness)
    else {
        panic!(
            "wake preemption checks did not complete: sleep {:?}, receive {:?}",
            processes[0].wake_lateness, processes[0].ipc_lateness
        );
    };
    assert!(
        processes[0].spin_kept_quantum,
        "a displaced spinner lost its quantum"
    );
    assert!(
        wake_lateness <= 1 && ipc_lateness <= 1,
        "late wake: sleep {wake_lateness} ticks, receive {ipc_lateness} ticks"
    );
    serial::println!(
        "MIRROR_PREEMPT_OK wake_lateness={} ipc_lateness={}",
        wake_lateness,
        ipc_lateness
    );
    let yields: usize = processes.iter().map(|process| process.yields).sum();
    let sleeps: usize = processes.iter().map(|process| process.sleeps).sum();
    let preemptions: usize = processes.iter().map(|process| process.preemptions).sum();
    assert!(stats.switches >= 4, "processes did not alternate");
    serial::println!(
        "MIRROR_SCHEDULER_OK processes={} switches={} yields={} sleeps={} preemptions={} idle_ticks={}",
        PROCESS_COUNT,
        stats.switches,
        yields,
        sleeps,
        preemptions,
        stats.idle_ticks
    );
}

/// Publish disjoint I/O sessions through user execution and deferred syscall
/// completion. Revoke the process pointer before VM work; every handler has
/// unwound before scheduler-owned memory is accessed again.
fn resume_process(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    process: &mut Process,
    ipc: &mut Ipc,
    surface: Option<&mut surface::Session<'_, '_>>,
    input: Option<&mut input::Session<'_>>,
    windows: Option<&mut windows::Session>,
) {
    assert!(!arch::interrupts_enabled());
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_IPC.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_SURFACE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_INPUT.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_WINDOWS.load(Ordering::SeqCst).is_null());
    // The scheduler dequeued this thread and made it current.
    assert!(!process.finished());
    assert_eq!(process.thread().state, State::Ready);
    let thread = process.thread_mut();
    thread.state = State::Running;
    // A thread that a wake displaced resumes its interrupted quantum; any
    // other dispatch starts a new one (0030-D3).
    let resumed = core::mem::take(&mut thread.displaced);
    if resumed {
        assert!(
            (1..=QUANTUM_TICKS).contains(&thread.quantum_left),
            "displaced thread has no quantum left"
        );
    } else {
        thread.quantum_left = QUANTUM_TICKS;
    }
    #[cfg(feature = "boot-smoke")]
    record_latency(process, resumed);
    process.dispatches += 1;
    // Save a copy, not a reference into a context that handlers can mutate.
    let context = process.thread().context.frame;
    // The assembly continuation retains this pointer across handler calls.
    // Give it disjoint storage: a pointer derived through Option::as_mut into
    // the thread would alias handlers' exclusive reborrows of the Process.
    // The current slot cannot be joined/reaped until this call returns.
    let mut extended = core::mem::replace(
        &mut process.thread_mut().context.extended,
        crate::fpu::State::new(),
    );
    let process_pointer = core::ptr::from_mut(process);
    let surface_pointer = surface.map_or(core::ptr::null_mut(), |session| {
        core::ptr::from_mut(session).cast()
    });
    let input_pointer = input.map_or(core::ptr::null_mut(), |session| {
        core::ptr::from_mut(session).cast()
    });
    let windows_pointer = windows.map_or(core::ptr::null_mut(), core::ptr::from_mut);
    ACTIVE_WINDOWS.store(windows_pointer, Ordering::SeqCst);
    ACTIVE_SURFACE.store(surface_pointer, Ordering::SeqCst);
    ACTIVE_INPUT.store(input_pointer, Ordering::SeqCst);
    ACTIVE_IPC.store(core::ptr::from_mut(ipc), Ordering::SeqCst);
    ACTIVE.store(process_pointer.cast(), Ordering::SeqCst);
    // SAFETY: Single BSP and IF=0, disjoint exclusively owned session objects,
    // and shared upper mappings cover both kernel stacks. The raw pointers are
    // derived from fresh reborrows; the process is not accessed again until all
    // handlers unwind and its pointer is revoked. I/O sessions stay borrowed
    // until deferred syscall completion, before returning to their owners.
    unsafe {
        (*process_pointer)
            .space
            .activate()
            .expect("activate user root");
        interrupts::resume_user(&context, &raw mut extended);
    }
    ACTIVE.store(core::ptr::null_mut(), Ordering::SeqCst);
    // SAFETY: All handlers returned, the user is suspended, and IF remains clear.
    unsafe { kernel.activate() }.expect("restore kernel root");
    process.thread_mut().context.extended = extended;
    #[cfg(feature = "boot-smoke")]
    crate::fpu::assert_kernel_clean();
    // Session pointers remain exclusively borrowed until deferred copy/syscall
    // work completes. No user context or interrupt handler is active here.
    if let State::Memory(request) = process.thread().state {
        complete_memory(process, frames, request);
    }
    ACTIVE_IPC.store(core::ptr::null_mut(), Ordering::SeqCst);
    ACTIVE_SURFACE.store(core::ptr::null_mut(), Ordering::SeqCst);
    ACTIVE_INPUT.store(core::ptr::null_mut(), Ordering::SeqCst);
    ACTIVE_WINDOWS.store(core::ptr::null_mut(), Ordering::SeqCst);
    assert_ne!(
        process.thread().state,
        State::Running,
        "process returned without transition"
    );
}

/// At a dispatch, record a flagged wake's latency, or that a flagged spinner
/// resumed with the rest of its quantum (spec 0030a, acceptance 4). Each
/// flag times one event and is then cleared.
#[cfg(feature = "boot-smoke")]
fn record_latency(process: &mut Process, resumed: bool) {
    let now = timer::ticks();
    let thread = process.thread_mut();
    let probe = thread.probe;
    if probe == Some(LatencyProbe::Spin) && resumed && thread.quantum_left < QUANTUM_TICKS {
        thread.probe = None;
        process.spin_kept_quantum = true;
        return;
    }
    let Some(due) = thread.due.take() else {
        return;
    };
    thread.probe = None;
    let lateness = now.wrapping_sub(due);
    let slot = match probe {
        Some(LatencyProbe::Sleep) => &mut process.wake_lateness,
        Some(LatencyProbe::Receive) => &mut process.ipc_lateness,
        _ => unreachable!("timed a wake without its probe"),
    };
    assert!(slot.replace(lateness).is_none(), "wake timed twice");
}

/// Complete bounded VM work with the kernel root active and no interrupt frames
/// or published process borrow. Disjoint I/O sessions remain borrowed by resume.
fn complete_memory(process: &mut Process, frames: &mut FrameAllocator<'_>, request: MemoryRequest) {
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    assert!(!arch::interrupts_enabled());
    match request {
        MemoryRequest::Operation => {
            let frame = process.thread().context.frame;
            let result = match frame.rax {
                vm_abi::SYS_VM_RESERVE => process
                    .vm
                    .reserve(frame.rdi, frame.rsi, frame.rdx)
                    .map(|address| address as i64),
                vm_abi::SYS_VM_RELEASE if frame.rdx != 0 => Err(-22),
                vm_abi::SYS_VM_RELEASE => {
                    // SAFETY: The inactive tree belongs only to this suspended
                    // process; all its threads are suspended, with no DMA or references.
                    unsafe {
                        process
                            .vm
                            .release(&mut process.space, frames, frame.rdi, frame.rsi)
                    }
                    .map(|()| 0)
                }
                vm_abi::SYS_VM_PROTECT => {
                    // SAFETY: Kernel CR3 is active; all user threads are suspended.
                    unsafe {
                        process
                            .vm
                            .protect(&mut process.space, frame.rdi, frame.rsi, frame.rdx)
                    }
                    .map(|()| 0)
                }
                _ => unreachable!("invalid deferred VM operation"),
            };
            if result.is_ok() && frame.rax != vm_abi::SYS_VM_RESERVE {
                // Invalidate the old wait key before any sibling can run or
                // re-reserve this address. Failed transactional changes leave
                // waiters untouched. No backing page or raw pointer is pinned.
                invalidate_waits(
                    process,
                    frame.rdi,
                    frame.rsi,
                    frame.rax == vm_abi::SYS_VM_RELEASE || frame.rdx == vm_abi::PROT_NONE,
                );
            }
            process.thread_mut().context.frame.rax = result.unwrap_or_else(|error| error) as u64;
            process.thread_mut().state = State::Ready;
        }
        MemoryRequest::Fault { address, error } => {
            // SAFETY: The saved faulting instruction has not resumed. The
            // allocator is unique and all affected user mappings are inactive.
            let result = unsafe {
                process.vm.materialize(
                    &mut process.space,
                    frames,
                    process.direct_map,
                    address,
                    1,
                    error & 2 != 0,
                )
            };
            if result.is_ok() {
                #[cfg(feature = "boot-smoke")]
                {
                    process.demand_faults += 1;
                }
                process.thread_mut().state = State::Ready;
            } else {
                // Unresolvable demand faults retain the original diagnostic,
                // terminate this process and let the scheduler reclaim its VM.
                let frame = process.thread().context.frame;
                record_fault(process, &frame, address);
            }
        }
        MemoryRequest::SyscallBuffer {
            address,
            length,
            writable,
        } => {
            // SAFETY: Complete access validation preceded this deferral; repeat
            // it in materialize before allocating or performing syscall work.
            let result = unsafe {
                process.vm.materialize(
                    &mut process.space,
                    frames,
                    process.direct_map,
                    address,
                    length,
                    writable,
                )
            };
            if let Err(error) = result {
                process.thread_mut().context.frame.rax =
                    (if error == -12 { -12_i64 } else { -14_i64 }) as u64;
                process.thread_mut().state = State::Ready;
                return;
            }
            #[cfg(feature = "boot-smoke")]
            {
                process.copy_faults += 1;
            }
            let mut frame = process.thread().context.frame;
            let outcome = dispatch_syscall(process, &mut frame);
            process.thread_mut().context.frame = frame;
            if !outcome.finished {
                park(process, outcome.suspension.unwrap_or(State::Ready));
            }
        }
    }
}

/// Describe only authorized, bounded copy operations. Bad handles, wrong object
/// types and malformed lengths keep their existing error precedence and cannot
/// cause page allocation. Actual dispatch repeats all validation after page-in.
fn syscall_buffer(process: &Process, frame: &TrapFrame) -> Option<(u64, usize, bool)> {
    let require = |handle, rights| {
        process
            .handles
            .require(handle, rights)
            .ok()
            .map(|cap| cap.object)
    };
    let surface = |handle| {
        matches!(require(handle, capability::WRITE), Some(Object::Surface))
            || matches!(require(handle, capability::WRITE), Some(Object::AppSurface(pid)) if pid == process.pid)
    };
    let app = |handle| {
        matches!(
            require(handle, capability::WRITE),
            Some(Object::Application(_))
        )
    };
    let range = match frame.rax {
        1 if frame.rsi <= MAX_WRITE as u64
            && require(frame.rdx, capability::WRITE) == Some(Object::Console) =>
        {
            (frame.rdi, frame.rsi, false)
        }
        8 if (1..=MESSAGE_SIZE as u64).contains(&frame.rdx)
            && channel_for(process, frame.rdi, capability::SEND).is_ok() =>
        {
            (frame.rsi, frame.rdx, false)
        }
        9 | 10
            if (1..=MESSAGE_SIZE as u64).contains(&frame.rdx)
                && channel_for(process, frame.rdi, capability::RECEIVE).is_ok() =>
        {
            (frame.rsi, frame.rdx, true)
        }
        13 if frame.rdx == 24 && surface(frame.rdi) => (frame.rsi, frame.rdx, true),
        14 if (16..=windows::TILE_PACKET as u64).contains(&frame.rdx) && surface(frame.rdi) => {
            (frame.rsi, frame.rdx, false)
        }
        15 | 16
            if frame.rdx == input::EVENT_SIZE as u64
                && (require(frame.rdi, capability::RECEIVE) == Some(Object::Input)
                    || matches!(require(frame.rdi, capability::RECEIVE), Some(Object::AppInput(pid)) if pid == process.pid)) =>
        {
            (frame.rsi, frame.rdx, true)
        }
        18 if frame.rdx == windows::INFO_SIZE as u64 && app(frame.rdi) => {
            (frame.rsi, frame.rdx, true)
        }
        19 if frame.rdx == windows::EVENT_SIZE as u64 && app(frame.rdi) => {
            (frame.rsi, frame.rdx, false)
        }
        20 if (28..=windows::READ_PACKET as u64).contains(&frame.rdx) && app(frame.rdi) => {
            (frame.rsi, frame.rdx, true)
        }
        vm_abi::SYS_VM_STATS if frame.rsi == vm_abi::STATS_SIZE as u64 && frame.rdx == 0 => {
            (frame.rdi, frame.rsi, true)
        }
        thread_abi::SYS_TLS_GET
            if frame.rsi == thread_abi::TLS_BASES_SIZE as u64 && frame.rdx == 0 =>
        {
            (frame.rdi, frame.rsi, true)
        }
        clock_abi::SYS_CLOCK_GET
            if clock_abi::clock_arguments_valid(frame.rsi, frame.rdx)
                && crate::clock::is_available() =>
        {
            (frame.rdi, frame.rsi, true)
        }
        thread_abi::SYS_THREAD_CREATE
            if frame.rsi == thread_abi::THREAD_START_SIZE as u64 && frame.rdx == 0 =>
        {
            (frame.rdi, frame.rsi, false)
        }
        thread_abi::SYS_WAIT_U32
            if thread_abi::wait_arguments_valid(frame.rdi, frame.rsi, frame.rdx)
                && thread_abi::wait_word_address_valid(frame.rdi) =>
        {
            (frame.rdi, thread_abi::WAIT_WORD_SIZE, false)
        }
        _ => return None,
    };
    Some((range.0, range.1 as usize, range.2))
}

/// The shell session's application state. Apps are process-table entries;
/// `children` holds their slots by window index.
struct ShellRuntime<'a> {
    kernel: &'a PageTables,
    bytes: &'a [u8],
    image: elf::Image,
    direct_map: u64,
    children: [Option<usize>; windows::APP_COUNT],
    windows: windows::Session,
    /// The session's peak charged frames, sampled at each scheduler loop top
    /// (0059-D4; boot-smoke only).
    #[cfg(feature = "boot-smoke")]
    tables_peak: u64,
    #[cfg(feature = "boot-smoke")]
    surfaces_peak: u64,
}

impl ShellRuntime<'_> {
    /// 0059-D4 (boot-smoke): sample the session's charge peaks — the shell's
    /// own table and surface charges plus the live children's table charges.
    /// The scheduler calls this at each loop top, one iteration after any
    /// launch, close or reclaim.
    #[cfg(feature = "boot-smoke")]
    fn sample_charge_peaks(&mut self, desktop: &Process) {
        let children: u64 = self
            .children
            .iter()
            .flatten()
            .map(|&slot| {
                // SAFETY: The scheduler holds no other reference to the child.
                unsafe { sched::process(slot) }.space.owned_table_count() as u64
            })
            .sum();
        self.tables_peak = self
            .tables_peak
            .max(desktop.space.owned_table_count() as u64 + children);
        self.surfaces_peak = self.surfaces_peak.max(desktop.surface_frames);
    }

    fn launch(
        &mut self,
        frames: &mut FrameAllocator<'_>,
        desktop: &mut Process,
        kind: u64,
    ) -> Result<u64, i64> {
        let index = self
            .windows
            .applications
            .iter()
            .position(Option::is_none)
            .ok_or(-11)?;
        assert!(self.children[index].is_none());
        let before = frames.stats().unwrap().allocated_frames;
        let slot = create_process(
            self.kernel,
            frames,
            self.bytes,
            &self.image,
            self.direct_map,
            kind,
        )
        .map_err(|error| error.errno())?;
        // SAFETY: A fresh slot, distinct from the shell's, that nothing else
        // references; this borrow ends before any rollback removes it.
        let child = unsafe { sched::process(slot) };
        let pid = child.pid;
        let prepared = (|| {
            for (slot, object, rights) in [
                (0, Object::Console, capability::WRITE),
                (3, Object::AppSurface(pid), capability::WRITE),
                (4, Object::AppInput(pid), capability::RECEIVE),
            ] {
                child.boot_handles[slot] =
                    child.handles.insert(object, rights).map_err(cap_error)?;
            }
            // SAFETY: The kernel root is active on the sole BSP with IF=0.
            // The unique allocator and persistent HHDM own all acquired pages.
            let mut application =
                unsafe { windows::Application::new(pid, kind as u32, frames, self.direct_map) }?;
            // 0059-D3: the shell carries the app's surface frames.
            desktop.surface_frames += windows::SURFACE_PAGES as u64;
            match desktop
                .handles
                .insert(Object::Application(pid), capability::WRITE)
            {
                Ok(handle) => Ok((application, handle)),
                Err(error) => {
                    desktop.surface_frames -= windows::SURFACE_PAGES as u64;
                    // SAFETY: No application resource or handle was published.
                    unsafe { application.destroy(frames) };
                    Err(cap_error(error))
                }
            }
        })();
        let (application, handle) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                // SAFETY: The child has never run and its borrow has ended.
                let mut child = unsafe { sched::discard(slot) };
                // SAFETY: The child has never run, and failed resource creation
                // released all offscreen pages before this address-space teardown.
                unsafe { child.space.destroy(frames) }.expect("application-load rollback");
                assert_eq!(frames.stats().unwrap().allocated_frames, before);
                return Err(error);
            }
        };
        assert_ne!(
            child.space.root_physical_address(),
            desktop.space.root_physical_address()
        );
        child.policy = CpuPolicy::Application(ServicePolicy::new(timer::ticks()));
        child.access.windows = true;
        self.children[index] = Some(slot);
        self.windows.applications[index] = Some(application);
        self.windows.launches += 1;
        serial::println!("MIRROR_APP_STARTED pid={} kind={}", pid, kind);
        Ok(handle)
    }

    fn reclaim(&mut self, index: usize, frames: &mut FrameAllocator<'_>, ipc: &mut Ipc) {
        let slot = self.children[index]
            .take()
            .expect("reclaim live child slot");
        // SAFETY: The scheduler holds no reference to the child; reap checks
        // that it finished.
        let mut child = unsafe { sched::reap(slot) };
        let outcome = child.outcome.expect("child ended without outcome");
        release_handles(&mut child, ipc);
        assert!(child.handles.is_empty());
        let before = frames.stats().unwrap().allocated_frames;
        let owned = child.space.owned_page_count() + child.space.owned_table_count();
        // SAFETY: The scheduler restored its kernel root and cleared every
        // active pointer before considering a finished process for reclamation.
        unsafe { child.space.destroy(frames) }.expect("application address-space cleanup");
        #[cfg(feature = "boot-smoke")]
        child.check_charges_zero();
        assert_eq!(frames.stats().unwrap().allocated_frames + owned, before);
        self.windows.preemptions += child.preemptions;
        let (state, detail, reason) = match outcome {
            Outcome::Exit(status) => (2, status, "exit"),
            Outcome::Terminated => (2, 0, "closed"),
            Outcome::SessionEnded => (2, 0, "session_ended"),
            Outcome::Fault { vector, .. } => (3, vector, "fault"),
            Outcome::InvalidStack(_)
            | Outcome::InvalidContext { .. }
            | Outcome::InvalidTls { .. } => (3, 13, "fault"),
            Outcome::BudgetExhausted => (3, 0, "budget"),
        };
        if state == 3 {
            self.windows.faults += 1;
        }
        self.windows.applications[index]
            .as_mut()
            .expect("child resources")
            .finish(state, detail);
        serial::println!(
            "MIRROR_APP_RECLAIMED pid={} reason={} status={} frames_reclaimed=1",
            child.pid,
            reason,
            detail
        );
    }

    fn reclaim_finished(&mut self, frames: &mut FrameAllocator<'_>, ipc: &mut Ipc) {
        for index in 0..self.children.len() {
            if self.children[index].is_some_and(|slot| {
                // SAFETY: The scheduler holds no other reference to the child.
                unsafe { sched::process(slot) }.finished()
            }) {
                self.reclaim(index, frames, ipc);
            }
        }
    }

    fn close(
        &mut self,
        frames: &mut FrameAllocator<'_>,
        desktop: &mut Process,
        ipc: &mut Ipc,
        pid: u64,
        reason: Outcome,
    ) -> Result<(), i64> {
        let index = self
            .windows
            .applications
            .iter()
            .position(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|application| application.pid == pid)
            })
            .ok_or(-9)?;
        if let Some(slot) = self.children[index] {
            // SAFETY: The child's slot is distinct from the shell's, and this
            // borrow ends before reclaim removes the entry.
            let child = unsafe { sched::process(slot) };
            assert!(child.threads().all(|thread| thread.state != State::Running));
            if !child.finished() {
                child.finish(reason);
            }
            self.reclaim(index, frames, ipc);
        }
        // Revoke all duplicates before a slot can be reused for a fresh PID.
        desktop.handles.revoke_application(pid);
        let mut application = self.windows.applications[index].take().unwrap();
        let before = frames.stats().unwrap().allocated_frames;
        // 0059-D3: the shell's surface charge is released with the destroy.
        desktop.surface_frames -= windows::SURFACE_PAGES as u64;
        // SAFETY: No child or handler can access the removed resource, whose
        // pages have never been mapped into either application address space.
        unsafe { application.destroy(frames) };
        assert_eq!(
            frames.stats().unwrap().allocated_frames + windows::SURFACE_PAGES,
            before
        );
        if reason == Outcome::Terminated {
            self.windows.closes += 1;
        }
        serial::println!("MIRROR_APP_CLOSED pid={} surfaces_reclaimed=1", pid);
        Ok(())
    }
}

/// Record the current thread's suspension (0030-D2). A blocking state
/// snapshots the thread's charge and attaches it to its event queue and, for
/// a timed wait, the deadline heap. A joiner waits on its target's `joiner`
/// field, so an exit signals that joiner instead.
fn park(process: &mut Process, state: State) {
    let owner = process.thread().process;
    let (queue, deadline) = match state {
        State::Sleeping(deadline) => (None, Some(deadline)),
        State::Receiving(pending) => (
            Some(sched::Queue::Channel(pending.channel)),
            Some(pending.deadline),
        ),
        State::InputWaiting(pending) => (Some(pending.queue), Some(pending.deadline)),
        State::Waiting(pending) => (
            Some(sched::Queue::Futex {
                process: owner,
                address: pending.address,
            }),
            pending.deadline,
        ),
        State::Joining(_) => (None, None),
        State::Exited(_) => {
            if let Some(tid) = process.thread().joiner {
                let joiner = member_by_id(process, tid).expect("join waiter exists");
                sched::signal(process.members[joiner], None);
            }
            process.thread_mut().state = state;
            return;
        }
        _ => {
            process.thread_mut().state = state;
            return;
        }
    };
    let thread = process.thread_mut();
    thread.state = state;
    thread.sleep_charge = thread.charged_ticks;
    sched::block(thread.slot, queue, deadline);
}

/// Complete one signaled wait, in the scheduler with the kernel root active,
/// never in the handler that signaled it (0030-D2). A preset result decided
/// by the event completes an address wait as is. Otherwise the condition is
/// checked again: siblings may have changed a shared mapping or closed a
/// shared handle meanwhile, so the checked receive helpers repeat permission
/// and range validation. Readiness or closure beats a deadline reached in the
/// same check. A wait that still cannot complete stays queued.
fn complete_wait(
    process: &mut Process,
    signal: sched::Signal,
    ipc: &mut Ipc,
    input: Option<&mut input::Session<'_>>,
    windows: Option<&mut windows::Session>,
    now: u64,
) {
    let sched::Signal { slot, preset, at } = signal;
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    process.current = process
        .members
        .iter()
        .position(|&member| member == slot)
        .expect("signaled thread has no owner");
    let state = process.thread().state;
    assert_eq!(
        process.thread().charged_ticks,
        process.thread().sleep_charge,
        "blocked thread charged user CPU ticks"
    );
    assert!(
        preset.is_none() || matches!(state, State::Waiting(_)),
        "preset result for a {state:?} wait"
    );
    // The result for RAX, or None to keep the one the syscall saved.
    let result = match state {
        State::Sleeping(deadline) => {
            assert!(deadline_reached(now, deadline), "sleep signaled early");
            None
        }
        State::Waiting(pending) => Some(preset.unwrap_or_else(|| {
            assert!(pending.expired(now), "address wait signaled early");
            #[cfg(feature = "boot-smoke")]
            {
                process.sync_timeouts += 1;
            }
            -110
        })),
        State::Receiving(pending) => {
            let mut result = receive(
                process,
                ipc,
                pending.handle,
                pending.address,
                pending.capacity,
            );
            if result == -11 && deadline_reached(now, pending.deadline) {
                result = -110;
                #[cfg(feature = "boot-smoke")]
                {
                    process.ipc_timeouts += 1;
                }
            }
            if result == -11 {
                return;
            }
            #[cfg(feature = "boot-smoke")]
            if result == -32 {
                process.ipc_closed_wakes += 1;
            }
            Some(result)
        }
        State::InputWaiting(pending) => {
            // Resolve the capability afresh: it may have been closed by a
            // sibling since the wait began. Session availability is tied
            // to process authority, never to the thread that issued it.
            let mut result = match process.handles.require(pending.handle, capability::RECEIVE) {
                Ok(cap) => match cap.object {
                    Object::Input => receive_input(
                        process,
                        input.expect("input owner session"),
                        pending.handle,
                        pending.address,
                        input::EVENT_SIZE as u64,
                    ),
                    Object::AppInput(pid) if pid == process.pid => receive_app_input(
                        process,
                        windows.expect("application input session"),
                        pending.handle,
                        pending.address,
                        windows::EVENT_SIZE as u64,
                    ),
                    _ => -13,
                },
                Err(error) => cap_error(error),
            };
            if result == -11 && deadline_reached(now, pending.deadline) {
                result = -110;
                process.input_timeouts += 1;
            }
            if result == -11 {
                return;
            }
            Some(result)
        }
        State::Joining(tid) => {
            // The target's exit signaled this join after its context was
            // saved and its pointer released, so it is reaped here.
            let target = member_by_id(process, tid).expect("join target exists");
            let State::Exited(status) = process.member(target).expect("join target").state else {
                panic!("join signaled before its target exited");
            };
            process.thread_mut().context.frame.rdx = status;
            process.remove_member(target);
            #[cfg(feature = "boot-smoke")]
            {
                process.threads_joined += 1;
            }
            Some(0)
        }
        _ => panic!("signaled a thread that is not waiting: {state:?}"),
    };
    sched::release(slot);
    let thread = process.thread_mut();
    if let Some(result) = result {
        thread.context.frame.rax = result as u64;
    }
    // A flagged sleep is due at its deadline; a flagged receive that got a
    // message, at the sending syscall's trap exit, when it was signaled.
    #[cfg(feature = "boot-smoke")]
    {
        thread.due = match (thread.probe, state) {
            (Some(LatencyProbe::Sleep), State::Sleeping(deadline)) => Some(deadline),
            (Some(LatencyProbe::Receive), State::Receiving(_)) if result.is_some_and(|r| r > 0) => {
                Some(at)
            }
            _ => None,
        };
    }
    #[cfg(not(feature = "boot-smoke"))]
    let _ = at;
    thread.wake();
}

/// Signal this process's receive and input waits for a recheck, on `handle`
/// only or on any. A closed handle or a changed mapping can fail them.
fn signal_handle_waits(process: &Process, handle: Option<u64>) {
    for thread in process.threads() {
        let waited = match thread.state {
            State::Receiving(pending) => pending.handle,
            State::InputWaiting(pending) => pending.handle,
            _ => continue,
        };
        if handle.is_none_or(|handle| handle == waited) {
            sched::signal(thread.slot, None);
        }
    }
}

/// Compare and enqueue are one non-preemptible operation on the sole CPU.
/// Deferred page-in also completes with IF=0 before another user can run. An
/// SMP implementation must instead serialize the value check and wait queue
/// with wake/invalidation under the same lock; disabling local IRQs won't do.
fn wait_word(process: &mut Process, frame: &TrapFrame) -> Result<State, i64> {
    assert!(!arch::interrupts_enabled());
    if !thread_abi::wait_arguments_valid(frame.rdi, frame.rsi, frame.rdx) {
        return Err(-22);
    }
    if !thread_abi::wait_word_address_valid(frame.rdi) {
        return Err(-14);
    }
    let mut bytes = [0; thread_abi::WAIT_WORD_SIZE as usize];
    // Check ledger permissions too: PROT_NONE may retain physical backing.
    process
        .vm
        .preflight(&process.space, frame.rdi, bytes.len(), false)
        .map_err(|_| -14)?;
    // SAFETY: User execution is stopped on the BSP. The checked copy translates
    // every byte through owned user mappings, with no concurrent user writer.
    unsafe { process.space.copy_from_user(frame.rdi, &mut bytes) }.map_err(|_| -14)?;
    if u64::from(u32::from_le_bytes(bytes)) != frame.rsi {
        return Err(-11);
    }
    if frame.rdx == 0 {
        return Err(-110);
    }
    #[cfg(feature = "boot-smoke")]
    {
        process.sync_waits += 1;
    }
    Ok(State::Waiting(PendingWait {
        address: frame.rdi,
        deadline: (frame.rdx != thread_abi::WAIT_FOREVER)
            .then(|| timer::ticks().wrapping_add(frame.rdx)),
    }))
}

/// Wake only private wait keys in this process. Values are never read or
/// written: a wake on untouched but readable reserved memory allocates nothing.
/// Expired waits complete as timeouts and do not consume the caller's count.
/// Waiters are selected in FIFO order (0030-D6); the scheduler completes them.
fn wake_word(process: &mut Process, frame: &TrapFrame) -> Result<u64, i64> {
    assert!(!arch::interrupts_enabled());
    if !thread_abi::wake_arguments_valid(frame.rdi, frame.rsi, frame.rdx) {
        return Err(-22);
    }
    if !thread_abi::wait_word_address_valid(frame.rdi) {
        return Err(-14);
    }
    process
        .vm
        .preflight(
            &process.space,
            frame.rdi,
            thread_abi::WAIT_WORD_SIZE as usize,
            false,
        )
        .map_err(|_| -14)?;
    let (woken, expired) = sched::wake_address(
        process.thread().process,
        frame.rdi,
        frame.rsi,
        timer::ticks(),
    );
    #[cfg(feature = "boot-smoke")]
    {
        process.sync_wakes += woken;
        process.sync_timeouts += expired;
    }
    #[cfg(not(feature = "boot-smoke"))]
    let _ = expired;
    Ok(woken)
}

/// After a successful release or protection change of `length` bytes at
/// `address`, cancel the process's address waits in that range when the
/// change removed access, and recheck its receive and input waits, whose
/// buffers may no longer be writable.
fn invalidate_waits(process: &mut Process, address: u64, length: u64, cancel: bool) {
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    assert!(!arch::interrupts_enabled());
    let end = address.checked_add(length).expect("successful VM interval");
    if cancel {
        let cancelled = sched::cancel_addresses(process.thread().process, address, end);
        #[cfg(feature = "boot-smoke")]
        {
            process.sync_invalidations += cancelled;
        }
        #[cfg(not(feature = "boot-smoke"))]
        let _ = cancelled;
    }
    signal_handle_waits(process, None);
}

/// The member index of the thread with this ID, if it belongs to `process`.
fn member_by_id(process: &Process, tid: u64) -> Option<usize> {
    (0..thread_abi::MAX_THREADS)
        .find(|&index| process.member(index).is_some_and(|thread| thread.id == tid))
}

fn create_thread(process: &mut Process, frame: &TrapFrame) -> Result<u64, i64> {
    if frame.rsi != thread_abi::THREAD_START_SIZE as u64 || frame.rdx != 0 {
        return Err(-22);
    }
    let mut bytes = [0; thread_abi::THREAD_START_SIZE];
    // SAFETY: All threads are stopped on this BSP; copy checks the entire
    // descriptor before touching the local destination. Lazy pages were
    // resolved by the normal deferred syscall-buffer path.
    unsafe { process.space.copy_from_user(frame.rdi, &mut bytes) }.map_err(|_| -14)?;
    let start = thread_abi::ThreadStart::decode(&bytes).ok_or(-22)?;
    if !process
        .space
        .translate(start.entry)
        .map_err(|_| -14)?
        .is_some_and(|translation| translation.executable)
        || process
            .vm
            .preflight(&process.space, start.stack, 8, true)
            .is_err()
    {
        return Err(-14);
    }
    let owner = usize::from(process.thread().process);
    add_thread(process, owner, &start)
}

/// Add a ready, queued thread to `process`, which occupies table slot
/// `owner`, and return its ID. A full process or a full thread table both
/// refuse with EAGAIN (0030-D4).
fn add_thread(
    process: &mut Process,
    owner: usize,
    start: &thread_abi::ThreadStart,
) -> Result<u64, i64> {
    let index = process
        .members
        .iter()
        .position(|&member| member == sched::NO_SLOT)
        .ok_or(-11)?;
    if !sched::has_room(0, 1) {
        return Err(-11);
    }
    // A running thread's own entry is the process's current member; this
    // insert touches only a vacant slot and the table's occupancy bitmap.
    let slot = sched::insert_thread(|slot| {
        let mut thread = Thread::new(start.entry, start.stack, start.argument, owner, slot);
        thread.context.frame.fs_base = start.fs;
        thread.context.frame.gs_base = start.gs;
        thread
    });
    process.members[index] = slot;
    sched::enqueue(slot);
    let tid = process.member(index).expect("new thread").id;
    #[cfg(feature = "boot-smoke")]
    {
        process.threads_created += 1;
        let live = process.threads().count();
        process.threads_peak = process.threads_peak.max(live);
    }
    Ok(tid)
}

fn join_thread(process: &mut Process, frame: &mut TrapFrame) -> Result<Option<State>, i64> {
    if frame.rsi != 0 || frame.rdx != 0 {
        return Err(-22);
    }
    let current = process.thread().id;
    if frame.rdi == current {
        return Err(-35);
    }
    let index = member_by_id(process, frame.rdi).ok_or(-3)?;
    let target = process.member(index).expect("join target");
    if target.joiner.is_some() {
        return Err(-16);
    }
    if let State::Exited(status) = target.state {
        frame.rdx = status;
        process.remove_member(index);
        #[cfg(feature = "boot-smoke")]
        {
            process.threads_joined += 1;
        }
        return Ok(None);
    }
    // A bounded wait-for walk rejects cycles before claiming the target. All
    // joins are within one process; TIDs are never interpreted as slot indexes.
    let mut state = target.state;
    for _ in 0..thread_abi::MAX_THREADS {
        let State::Joining(tid) = state else {
            break;
        };
        if tid == current {
            return Err(-35);
        }
        state = process
            .threads()
            .find(|thread| thread.id == tid)
            .expect("join target exists")
            .state;
    }
    process.member_mut(index).expect("join target").joiner = Some(current);
    Ok(Some(State::Joining(frame.rdi)))
}

/// Supervise two native applications without granting them scanout or physical
/// input. Lifecycle operations execute in the scheduler after all syscall
/// frames unwind. The session ends when the shell finishes; its live apps are
/// then closed with SessionEnded (0030-D1).
pub fn run_shell(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    programs: [&[u8]; 2],
    direct_map: u64,
    display: &mut framebuffer::Driver<'_>,
    consumer: &mut ps2::Consumer,
    smoke: bool,
) {
    assert!(!arch::interrupts_enabled());
    #[cfg(feature = "boot-smoke")]
    windows::self_test(frames, direct_map);
    let before = frames.stats().unwrap().allocated_frames;
    let heap_before = heap::stats().size;
    let desktop_image = elf::parse(programs[0]).expect("invalid shell ELF");
    let image = elf::parse(programs[1]).expect("invalid application ELF");
    let shell = create_process(
        kernel,
        frames,
        programs[0],
        &desktop_image,
        direct_map,
        2 + u64::from(smoke),
    )
    .expect("load native shell");
    let runtime = ShellRuntime {
        kernel,
        bytes: programs[1],
        image,
        direct_map,
        children: [None, None],
        windows: windows::Session::new(smoke),
        #[cfg(feature = "boot-smoke")]
        tables_peak: 0,
        #[cfg(feature = "boot-smoke")]
        surfaces_peak: 0,
    };
    {
        // SAFETY: Seeding holds the only reference to the shell's entry.
        let desktop = unsafe { sched::process(shell) };
        desktop.policy = CpuPolicy::Shell(ServicePolicy::new(timer::ticks()));
        desktop.access = Access {
            surface: true,
            input: true,
            windows: true,
        };
        for (slot, object, rights) in [
            (0, Object::Console, capability::WRITE),
            (3, Object::Surface, capability::WRITE),
            (4, Object::Input, capability::RECEIVE),
            (5, Object::Supervisor, capability::WRITE),
        ] {
            desktop.boot_handles[slot] = desktop
                .handles
                .insert(object, rights)
                .expect("shell bootstrap capability");
        }
    }
    #[cfg(feature = "boot-smoke")]
    let runtime = app_load_rollback_test(runtime, frames, shell);
    let surface = surface::Session::desktop(display).expect("shell scanout");
    let (framebuffer, region) = surface.placement();
    let input = input::Session::new(consumer, framebuffer, region);
    let mut session = sched::Session {
        ipc: Ipc::new(),
        surface: Some(surface),
        input: Some(input),
        runtime: Some(runtime),
        end: sched::End::Shell { shell },
    };
    sched::run(&mut session, kernel, frames, direct_map);
    let sched::Session {
        mut ipc,
        surface,
        runtime,
        ..
    } = session;
    let surface = surface.expect("shell scanout");
    let mut runtime = runtime.expect("shell runtime");
    runtime.reclaim_finished(frames, &mut ipc);
    let pids = runtime
        .windows
        .applications
        .each_ref()
        .map(|application| application.as_ref().map(|application| application.pid));
    for pid in pids.into_iter().flatten() {
        // SAFETY: The scheduler has returned; nothing else references the
        // shell, and close reaches only the apps' own slots.
        let desktop = unsafe { sched::process(shell) };
        runtime
            .close(frames, desktop, &mut ipc, pid, Outcome::SessionEnded)
            .expect("shell child cancellation");
    }
    // The shell is a zombie now: finished, with its thread entries released.
    // Validation reads its outcome and counters in the table; only then is
    // it reaped (0030-D5).
    let outcome = {
        // SAFETY: The scheduler has returned and every app has been reaped,
        // so nothing else references the shell's entry until this borrow
        // ends with this block, before the reap below.
        let desktop = unsafe { sched::process(shell) };
        release_handles(desktop, &mut ipc);
        assert!(desktop.zombie(), "the finished shell kept its threads");
        assert!(desktop.handles.is_empty() && ipc.is_drained());
        assert!(
            ACTIVE.load(Ordering::SeqCst).is_null()
                && ACTIVE_WINDOWS.load(Ordering::SeqCst).is_null()
        );
        assert_eq!(
            surface.totals(),
            (desktop.surface_tiles, desktop.surface_pixels)
        );
        let outcome = desktop.outcome.expect("shell ended without outcome");
        if smoke {
            validate_shell(desktop, outcome, &runtime.windows);
        }
        outcome
    };
    // SAFETY: The validation borrow has ended, and nothing references the
    // shell's entry; reap checks that it finished.
    let mut desktop = unsafe { sched::reap(shell) };
    sched::assert_drained();
    // SAFETY: The shell and children are stopped; all active pointers are clear
    // and no resource retains a reference to the shell's private address space.
    unsafe { desktop.space.destroy(frames) }.expect("shell address-space cleanup");
    #[cfg(feature = "boot-smoke")]
    desktop.check_charges_zero();
    // Each session ends at its own frame baseline.
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        // 0050-D2's shared rule, heap term (0059).
        before + (heap::stats().size - heap_before) / PAGE_SIZE as usize,
        "shell frame leak"
    );
    // 0059 acceptance test 4: the last session's drain leaks one charge,
    // requires the reap check to report it, repairs it, and only then prints
    // the marker.
    #[cfg(feature = "boot-smoke")]
    {
        desktop.surface_frames = 1;
        assert!(!desktop.charges_zero(), "reap check missed a leaked charge");
        desktop.surface_frames = 0;
        assert!(desktop.charges_zero(), "a repaired charge must pass");
    }
    #[cfg(feature = "boot-smoke")]
    serial::println!(
        "MIRROR_HEAP_CHARGE_OK tables_peak={} surfaces_peak={}",
        runtime.tables_peak,
        runtime.surfaces_peak
    );
    serial::println!(
        "MIRROR_SHELL_CLEANUP_OK apps_reclaimed=1 outcome={:?}",
        outcome
    );
}

/// Validate the smoke shell's zombie and its session's app lifecycle counts.
fn validate_shell(desktop: &Process, outcome: Outcome, windows: &windows::Session) {
    assert_eq!(outcome, Outcome::Exit(0), "shell smoke failed");
    assert!(desktop.desktop_checks, "shell checks marker missing");
    assert!(
        windows.launches >= 3 && windows.closes >= 2,
        "application lifecycle checks missing"
    );
    assert!(windows.faults >= 1, "application fault containment missing");
    assert!(windows.preemptions > 0, "applications were never preempted");
    serial::println!(
        "MIRROR_SHELL_TESTS_OK launches={} closes={} faults={} preemptions={}",
        windows.launches,
        windows.closes,
        windows.faults,
        windows.preemptions
    );
}

/// Exhaust the supervisor's remaining handle slots after loading the shell,
/// then force a launch to roll back its root, both buffers and its table
/// entries.
#[cfg(feature = "boot-smoke")]
fn app_load_rollback_test<'a>(
    mut runtime: ShellRuntime<'a>,
    frames: &mut FrameAllocator<'_>,
    shell: usize,
) -> ShellRuntime<'a> {
    // SAFETY: Seeding holds the only reference to the shell's entry, and the
    // failed launch reaches only the slot it creates and discards.
    let desktop = unsafe { sched::process(shell) };
    let baseline = frames.stats().unwrap().allocated_frames;
    let mut temporary = [0; 8];
    let mut count = 0;
    loop {
        match desktop
            .handles
            .insert(Object::Supervisor, capability::WRITE)
        {
            Ok(handle) => {
                temporary[count] = handle;
                count += 1;
            }
            Err(CapError::TableFull) => break,
            Err(error) => panic!("rollback test handle failure: {error:?}"),
        }
    }
    assert_eq!(runtime.launch(frames, desktop, 0), Err(-24));
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
    assert!(runtime.children.iter().all(Option::is_none));
    assert!(runtime.windows.applications.iter().all(Option::is_none));
    assert_eq!(
        sched::process_slots().count(),
        1,
        "rollback left a table entry"
    );
    for handle in &temporary[..count] {
        desktop.handles.close(*handle).unwrap();
    }
    serial::println!("MIRROR_APP_LOAD_ROLLBACK_OK");
    runtime
}

#[cfg(feature = "boot-smoke")]
fn service_policy_self_test() {
    for start in [0, 17, u64::MAX - 20] {
        let mut policy = ServicePolicy::new(start);
        for tick in 1..=SERVICE_ALLOWANCE {
            assert!(policy.eligible(start.wrapping_add(tick)));
            assert_eq!(
                policy.charge(start.wrapping_add(tick)),
                tick == SERVICE_ALLOWANCE
            );
        }
        // Repeated scheduling attempts, including wakes/yields, cannot refill.
        for tick in SERVICE_ALLOWANCE..SERVICE_PERIOD {
            assert!(!policy.eligible(start.wrapping_add(tick)));
            assert_eq!(policy.used, SERVICE_ALLOWANCE);
        }
        assert!(policy.eligible(start.wrapping_add(SERVICE_PERIOD)));
        assert_eq!(policy.used, 0);
        assert!(!policy.charge(start.wrapping_add(SERVICE_PERIOD)));
        assert_eq!(policy.used, 1);
        assert!(policy.eligible(start.wrapping_add(3 * SERVICE_PERIOD + 7)));
        assert_eq!(policy.window_start, start.wrapping_add(3 * SERVICE_PERIOD));
        assert_eq!(policy.used, 0); // Sleeping earns no stacked allowance.
    }
    serial::println!("MIRROR_SERVICE_POLICY_TESTS_OK");
}

fn deadline_reached(now: u64, deadline: u64) -> bool {
    // Delays are at most 1000 ticks, safely within half the counter range.
    now.wrapping_sub(deadline) < (1 << 63)
}

/// Close a finished process's handles once, releasing its IPC endpoints.
fn release_handles(process: &mut Process, ipc: &mut Ipc) {
    if process.finished() && !process.handles_closed {
        while let Some(capability) = process.handles.take_one() {
            ipc.release(capability);
            endpoint_released(capability);
        }
        process.handles_closed = true;
    }
}

/// A released channel reference can close the channel or discard its
/// messages, so its receivers recheck their waits.
fn endpoint_released(capability: Capability) {
    if let Object::Channel(index) = capability.object {
        sched::signal_queue(sched::Queue::Channel(index));
    }
}

#[cfg(feature = "boot-smoke")]
fn validate_process(process: &Process, scenario: u64) {
    let available = crate::clock::is_available();
    assert!(
        if available {
            process.clock_checks && !process.clock_unavailable
        } else {
            process.clock_unavailable && !process.clock_checks
        },
        "clock syscall checks did not complete: {:?}",
        process.outcome
    );
    serial::println!(
        "MIRROR_USER_CLOCK_OK pid={} available={}",
        process.pid,
        u8::from(available)
    );
    assert!(
        process.sync_checks
            && process.sync_waits > 0
            && process.sync_wakes > 0
            && process.sync_timeouts > 0
            && process.sync_invalidations >= 2,
        "synchronization checks did not complete: {:?}",
        process.outcome
    );
    serial::println!(
        "MIRROR_USER_SYNC_OK pid={} waits={} wakes={} timeouts={} invalidated={} cancelled={}",
        process.pid,
        process.sync_waits,
        process.sync_wakes,
        process.sync_timeouts,
        process.sync_invalidations,
        process.sync_cancelled
    );
    assert!(
        process.thread_checks
            && process.threads_created > 1
            && process.threads_joined > 0
            && process.thread_preemptions > 0,
        "thread checks did not complete: {:?}",
        process.outcome
    );
    serial::println!(
        "MIRROR_USER_THREADS_OK pid={} created={} joined={} exited={} cancelled={} preemptions={}",
        process.pid,
        process.threads_created,
        process.threads_joined,
        process.threads_exited,
        process.threads_cancelled,
        process.thread_preemptions
    );
    assert!(
        process.tls_checks,
        "TLS checks did not complete: {:?}",
        process.outcome
    );
    serial::println!("MIRROR_USER_TLS_OK pid={}", process.pid);
    assert!(
        process.fpu_checks && process.fpu_preemptions > 0,
        "FPU isolation/preemption checks did not complete: {:?}",
        process.outcome
    );
    serial::println!(
        "MIRROR_USER_FPU_OK pid={} preemptions={}",
        process.pid,
        process.fpu_preemptions
    );
    assert!(
        process.vm_checks,
        "VM checks did not complete: {:?}",
        process.outcome
    );
    assert!(
        process.demand_faults > 0 && process.copy_faults > 0,
        "missing demand page/copy faults"
    );
    serial::println!(
        "MIRROR_USER_VM_OK pid={} demand_faults={} copy_faults={}",
        process.pid,
        process.demand_faults,
        process.copy_faults
    );
    assert!(
        process.ipc_checks,
        "IPC checks did not complete: {:?}",
        process.outcome
    );
    assert!(
        process.checks_passed,
        "init checks did not complete: {:?}",
        process.outcome
    );
    assert_eq!(process.bad_pointers, 4);
    assert_eq!(process.unknown_calls, 1);
    assert!(process.dispatches > 1, "context did not resume");
    assert!(process.yields >= 5, "yield checks did not complete");
    assert!(process.sleeps >= 4, "sleep checks did not complete");
    assert!(
        process.preemptions > 0,
        "user computation was never preempted"
    );
    match (scenario, process.outcome.expect("missing process outcome")) {
        (0 | 8 | 9, Outcome::Exit(0)) => {}
        (18, Outcome::Exit(0)) => {
            assert_eq!(
                process.threads_cancelled, 0,
                "last thread exit cancelled live siblings"
            );
            serial::println!("MIRROR_THREAD_LAST_EXIT_OK");
        }
        (19, Outcome::Exit(0)) => {
            assert!(
                process.sync_cancelled > 0,
                "process exit did not cancel an address waiter"
            );
            assert!(
                process.threads_cancelled >= 3,
                "process exit did not cancel siblings"
            );
            serial::println!("MIRROR_THREAD_PROCESS_EXIT_OK");
        }
        (6, Outcome::InvalidStack(0x0000_8000_0000_0000)) => {
            serial::println!("MIRROR_USER_RETURN_REJECTED rsp=0x800000000000");
        }
        (7, Outcome::BudgetExhausted) => {
            assert_eq!(process.charged_ticks, USER_TICK_BUDGET);
            assert!(
                process.threads_cancelled >= 3,
                "CPU allowance was not tested across sibling threads"
            );
            serial::println!(
                "MIRROR_USER_BUDGET_CONTAINED pid={} ticks={}",
                process.pid,
                process.charged_ticks
            );
        }
        (
            probe,
            Outcome::Fault {
                vector,
                error,
                address,
                rip,
            },
        ) => {
            let expected = match probe {
                1 => (14, 5, 0xffff_ffff_8000_0000),
                2 => (14, 7, 0x400000),
                3 => {
                    assert!((STACK_BASE..STACK_TOP).contains(&address));
                    assert_eq!(rip, address);
                    (14, 0x15, address)
                }
                4 => (14, 4, STACK_BASE - PAGE_SIZE),
                5 => (13, 0, address), // #GP leaves CR2 unchanged.
                10 => (14, 7, 0x1_0200_0000),
                11 => {
                    assert_eq!(rip, address);
                    (14, 0x15, 0x1_0200_0000)
                }
                12 | 13 => (14, 4, 0x1_0200_0000),
                14 => (16, 0, address), // #MF leaves CR2 unchanged.
                15 => (19, 0, address), // #XM leaves CR2 unchanged.
                16 => (6, 0, address),  // AVX is unavailable without OSXSAVE.
                17 => (6, 0, address),  // Direct FS/GS base writes stay disabled.
                20 => {
                    assert!(
                        process.sync_cancelled > 0,
                        "worker fault did not cancel an address waiter"
                    );
                    assert!(
                        process.threads_cancelled >= 3,
                        "worker fault did not cancel siblings"
                    );
                    serial::println!("MIRROR_THREAD_FAULT_OK");
                    (6, 0, address)
                }
                21 => (6, 0, address), // SYSCALL raises #UD while EFER.SCE is clear.
                22 => (13, 0, address), // UMIP: SGDT at CPL3 raises #GP(0).
                _ => panic!("unexpected probe {probe}"),
            };
            assert_eq!((vector, error, address), expected, "wrong user fault");
            serial::println!(
                "MIRROR_USER_FAULT_CONTAINED vector={} error={:#x} cr2={:#x}",
                vector,
                error,
                address
            );
        }
        (_, outcome) => panic!("unexpected init outcome: {outcome:?}"),
    }
    serial::println!("MIRROR_USERSPACE_OK scenario={}", scenario);
}

fn load(
    space: &mut UserAddressSpace,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
) -> Result<(), PagingError> {
    for segment in image.segments() {
        let permissions = if segment.executable {
            UserPermissions::READ_EXECUTE
        } else if segment.writable {
            UserPermissions::READ_WRITE
        } else {
            UserPermissions::READ_ONLY
        };
        let pages = segment.memory_size.div_ceil(PAGE_SIZE);
        for page in 0..pages {
            let offset = (page * PAGE_SIZE) as usize;
            let length = segment
                .file_size
                .saturating_sub(offset)
                .min(PAGE_SIZE as usize);
            let source = if length == 0 {
                &[]
            } else {
                &bytes[segment.file_offset + offset..segment.file_offset + offset + length]
            };
            map_initialized(
                space,
                frames,
                direct_map,
                segment.virtual_address + page * PAGE_SIZE,
                permissions,
                source,
            )?;
        }
    }
    for address in (STACK_BASE..STACK_TOP).step_by(PAGE_SIZE as usize) {
        map_initialized(
            space,
            frames,
            direct_map,
            address,
            UserPermissions::READ_WRITE,
            &[],
        )?;
    }
    assert!(space.translate(STACK_BASE - PAGE_SIZE)?.is_none());
    assert!(space.translate(STACK_TOP)?.is_none());
    Ok(())
}

fn map_initialized(
    space: &mut UserAddressSpace,
    frames: &mut FrameAllocator<'_>,
    direct_map: u64,
    address: u64,
    permissions: UserPermissions,
    source: &[u8],
) -> Result<(), PagingError> {
    let frame = frames.allocate().ok_or(PagingError::OutOfFrames)?;
    let result = (|| {
        let destination = direct_map
            .checked_add(frame)
            .ok_or(PagingError::DirectMapOverflow)? as *mut u8;
        // SAFETY: This fresh, unique usable frame has a full HHDM mapping. The
        // validated file slice is at most one page and cannot alias the frame.
        unsafe {
            destination.write_bytes(0, PAGE_SIZE as usize);
            core::ptr::copy_nonoverlapping(source.as_ptr(), destination, source.len());
            space.map_page(address, frame, permissions, frames)
        }
    })();
    if result.is_err() {
        // SAFETY: map_page transfers ownership only on success; no Rust
        // references, user mapping or hardware access survive this failure.
        unsafe { frames.deallocate(frame) }.expect("release unmapped user frame");
    }
    result
}

fn with_active(frame: &TrapFrame, operation: impl FnOnce(&mut Process)) {
    assert_eq!(frame.cs & 3, 3, "userspace handler requires CPL3");
    assert!(!arch::interrupts_enabled());
    let pointer = ACTIVE.load(Ordering::SeqCst).cast::<Process>();
    assert!(!pointer.is_null(), "no active process");
    // SAFETY: The owning scheduler suspends its stack while this is published. Only
    // this BSP, with IF=0, accesses it; no handler retains a reference or reenters
    // userspace. Kernel/NMI faults never invoke this function. Closure references
    // cannot escape this call, and the borrowed kernel root remains live.
    unsafe {
        let process = &mut *pointer;
        assert_eq!(process.thread().state, State::Running);
        operation(process);
    }
}

fn with_ipc<T>(operation: impl FnOnce(&mut Ipc) -> T) -> T {
    assert!(!arch::interrupts_enabled());
    let pointer = ACTIVE_IPC.load(Ordering::SeqCst);
    assert!(!pointer.is_null(), "no IPC session");
    // SAFETY: resume_process exclusively publishes this disjoint object through
    // user entry and deferred completion. Handlers and completions run with IF=0
    // on the sole CPU, and neither reenter this helper nor retain references.
    unsafe { operation(&mut *pointer) }
}

fn with_surface<T>(operation: impl FnOnce(&mut surface::Session<'_, '_>) -> T) -> T {
    assert!(!arch::interrupts_enabled());
    let pointer = ACTIVE_SURFACE
        .load(Ordering::SeqCst)
        .cast::<surface::Session<'_, '_>>();
    assert!(!pointer.is_null(), "no active surface session");
    // SAFETY: The scheduler owns this session and its exclusive display borrow
    // until every process finishes. resume_process derives this pointer after
    // ending scheduler borrows, publishes through user/deferred syscall work,
    // and clears it before accessing the session again. No other CPU touches
    // the display, and no process/IPC field aliases this distinct session.
    unsafe { operation(&mut *pointer) }
}

fn with_input<T>(operation: impl FnOnce(&mut input::Session<'_>) -> T) -> T {
    assert!(!arch::interrupts_enabled());
    let pointer = ACTIVE_INPUT
        .load(Ordering::SeqCst)
        .cast::<input::Session<'_>>();
    assert!(!pointer.is_null(), "no active input session");
    // SAFETY: The desktop scheduler owns this separate stack-local session and
    // its exclusive Consumer borrow. resume_process publishes a fresh raw
    // reborrow through user entry/deferred completion and revokes it before
    // the outer scheduler accesses the session again.
    // Device IRQs mutate queues, never this staged session; IF=0 excludes races.
    unsafe { operation(&mut *pointer) }
}

fn with_windows<T>(operation: impl FnOnce(&mut windows::Session) -> T) -> T {
    assert!(!arch::interrupts_enabled());
    let pointer = ACTIVE_WINDOWS.load(Ordering::SeqCst);
    assert!(!pointer.is_null(), "no active application session");
    // SAFETY: resume_process publishes this disjoint scheduler-owned resource
    // session through entry and deferred completion. Both run on one BSP with
    // IF=0; the closure neither retains references nor reenters this helper.
    unsafe { operation(&mut *pointer) }
}

fn cap_error(error: CapError) -> i64 {
    match error {
        CapError::BadHandle => -9,
        CapError::Denied => -13,
        CapError::InvalidRights => -22,
        CapError::TableFull | CapError::IdsExhausted => -24,
    }
}

fn ipc_error(error: IpcError) -> i64 {
    match error {
        IpcError::WouldBlock => -11,
        IpcError::Closed => -32,
        IpcError::MessageSize => -90,
    }
}

fn channel_for(process: &Process, handle: u64, rights: u64) -> Result<usize, i64> {
    let capability = process.handles.require(handle, rights).map_err(cap_error)?;
    match capability.object {
        Object::Channel(index) => Ok(index),
        _ => Err(-13),
    }
}

fn receive_input(
    process: &mut Process,
    session: &mut input::Session<'_>,
    handle: u64,
    address: u64,
    length: u64,
) -> i64 {
    match process.handles.require(handle, capability::RECEIVE) {
        Ok(capability) if capability.object == Object::Input => {}
        Ok(_) => return -13,
        Err(error) => return cap_error(error),
    }
    if length != input::EVENT_SIZE as u64 {
        return -22;
    }
    // Validate before even peeking/staging an event, and repeat this on wake.
    // A failed copy must not consume input or expose a partial wire record.
    if process
        .space
        .validate_writable(address, input::EVENT_SIZE)
        .is_err()
    {
        return -14;
    }
    let Some(event) = session.peek() else {
        return -11;
    };
    // SAFETY: The user is suspended with immutable mappings on the BSP. This
    // owned encoded snapshot cannot alias the checked destination user pages.
    if unsafe { process.space.copy_to_user(address, &event) }.is_err() {
        return -14;
    }
    session.consume();
    let kind = u32::from_le_bytes(event[4..8].try_into().unwrap());
    if process.input_events == 0 {
        assert_eq!(kind, input::KIND_LOST, "missing initial input barrier");
    }
    process.input_events += 1;
    if kind == input::KIND_LOST {
        process.input_lost += 1;
    }
    input::EVENT_SIZE as i64
}

/// Init's console-text oracles. Only smoke kernels run init (spec 0010-D3).
#[cfg(feature = "boot-smoke")]
fn match_init_oracles(process: &mut Process, message: &[u8]) {
    process.checks_passed |= message == CHECKS_PASSED;
    process.vm_checks |= message == b"Mirror init: VM checks passed\n";
    process.tls_checks |= message == b"Mirror init: TLS checks passed\n";
    process.clock_checks |= message == b"Mirror init: clock checks passed\n";
    process.clock_unavailable |= message == b"Mirror init: clock unavailable\n";
    process.thread_checks |= message == b"Mirror init: thread checks passed\n";
    process.capacity_checks |= message == b"Mirror init: thread capacity checks passed\n";
    process.wait_queue_checks |= message == b"Mirror init: wait queue checks passed\n";
    process.sync_checks |= message == b"Mirror init: synchronization checks passed\n";
    if message == b"Mirror init: FPU preemption begin\n" {
        process.fpu_preemption_start = Some(process.preemptions);
    }
    if message == b"Mirror init: FPU checks passed\n" {
        process.fpu_checks = true;
        process.fpu_preemptions = process
            .fpu_preemption_start
            .map_or(0, |start| process.preemptions.saturating_sub(start));
    }
    process.ipc_checks |= message == IPC_CHECKS_PASSED;
    process.surface_checks |= message == SURFACE_CHECKS_PASSED;
    process.surface_denied_checks |= message == SURFACE_DENIED_CHECKS;
    // Spec 0030a, acceptance 4: the writing thread asks for its next wake,
    // or its next displacement, to be timed.
    let probe = match message {
        b"Mirror init: preempt sleep\n" => Some(LatencyProbe::Sleep),
        b"Mirror init: preempt receive\n" => Some(LatencyProbe::Receive),
        b"Mirror init: preempt spin\n" => Some(LatencyProbe::Spin),
        _ => None,
    };
    if probe.is_some() {
        process.thread_mut().probe = probe;
    }
}

/// The shell's console-text oracles and their markers. Only trace kernels
/// match console text (spec 0010-D4).
#[cfg(feature = "test-trace")]
fn service_message(process: &mut Process, message: &[u8]) {
    if !matches!(process.policy, CpuPolicy::Shell(_)) {
        return;
    }
    if message == b"Mirror shell: ready\n" && !process.desktop_ready {
        process.desktop_ready = true;
        let info = with_surface(|session| session.info());
        let width = u32::from_le_bytes(info[4..8].try_into().unwrap());
        let height = u32::from_le_bytes(info[8..12].try_into().unwrap());
        serial::println!(
            "MIRROR_SHELL_READY pid={} width={} height={}",
            process.pid,
            width,
            height
        );
    } else if message == b"Mirror shell: frame\n" {
        process.desktop_revision += 1;
        serial::println!(
            "MIRROR_SHELL_FRAME revision={} tiles={} input_events={}",
            process.desktop_revision,
            process.surface_tiles,
            process.input_events
        );
    } else if message == b"Mirror shell: checks passed\n" {
        process.desktop_checks = true;
    }
}

fn require_surface(process: &Process, handle: u64) -> Result<Option<u64>, i64> {
    match process.handles.require(handle, capability::WRITE) {
        Ok(capability) if capability.object == Object::Surface => Ok(None),
        Ok(Capability {
            object: Object::AppSurface(pid),
            ..
        }) if pid == process.pid => Ok(Some(pid)),
        Ok(_) => Err(-13),
        Err(error) => Err(cap_error(error)),
    }
}

/// Init's peer must try, and be denied, its owner's exact surface token. Record
/// each surface operation that rejected that token as a bad handle.
#[cfg(feature = "boot-smoke")]
fn note_foreign_surface(process: &mut Process, handle: u64, error: i64, operation: u8) {
    if error == cap_error(CapError::BadHandle) && handle != 0 && handle == process.foreign_surface {
        process.foreign_surface_denials |= operation;
    }
}

fn surface_info(process: &mut Process, handle: u64, address: u64, length: u64) -> i64 {
    let application = match require_surface(process, handle) {
        Ok(application) => application,
        Err(error) => {
            #[cfg(feature = "boot-smoke")]
            note_foreign_surface(process, handle, error, 1);
            return error;
        }
    };
    if length != surface::INFO_SIZE as u64 {
        return -22;
    }
    let bytes = if let Some(pid) = application {
        match with_windows(|session| {
            session
                .get(pid)
                .map(|_| windows::Application::surface_info())
        }) {
            Ok(bytes) => bytes,
            Err(error) => return error,
        }
    } else {
        with_surface(|session| session.info())
    };
    // SAFETY: This user is suspended, mappings are immutable on the sole BSP,
    // and the complete fixed-size destination is checked before any write.
    // The owned kernel info bytes cannot alias an owned user physical page.
    if unsafe { process.space.copy_to_user(address, &bytes) }.is_err() {
        return -14;
    }
    #[cfg(feature = "boot-smoke")]
    {
        process.surface_info_reads += 1;
    }
    surface::INFO_SIZE as i64
}

fn surface_present(process: &mut Process, handle: u64, address: u64, length: u64) -> i64 {
    let application = match require_surface(process, handle) {
        Ok(application) => application,
        Err(error) => {
            #[cfg(feature = "boot-smoke")]
            note_foreign_surface(process, handle, error, 2);
            return error;
        }
    };
    if !(16..=surface::MAX_PACKET as u64).contains(&length) {
        return -22;
    }
    let length = length as usize;
    let mut packet = [0_u8; surface::MAX_PACKET];
    // SAFETY: No user or other CPU runs during the checked complete copy. This
    // disjoint fixed-size kernel buffer is the sole source passed to the driver;
    // no user pointer or partial packet can survive into framebuffer mutation.
    if unsafe { process.space.copy_from_user(address, &mut packet[..length]) }.is_err() {
        return -14;
    }
    if let Some(pid) = application {
        return match with_windows(|session| session.get_mut(pid)?.present(&packet[..length])) {
            Ok(()) => length as i64,
            Err(error) => error,
        };
    }
    match with_surface(|session| session.present(&packet[..length])) {
        Ok(pixels) => {
            process.surface_tiles += 1;
            process.surface_pixels += pixels as u64;
            length as i64
        }
        Err(_) => -22,
    }
}

fn application_pid(process: &Process, handle: u64) -> Result<u64, i64> {
    match process
        .handles
        .require(handle, capability::WRITE)
        .map_err(cap_error)?
    {
        Capability {
            object: Object::Application(pid),
            ..
        } => Ok(pid),
        _ => Err(-13),
    }
}

fn application_info(process: &mut Process, handle: u64, address: u64, length: u64) -> i64 {
    let pid = match application_pid(process, handle) {
        Ok(pid) => pid,
        Err(error) => return error,
    };
    if length != windows::INFO_SIZE as u64 {
        return -22;
    }
    let info = match with_windows(|session| session.get(pid).map(windows::Application::info)) {
        Ok(info) => info,
        Err(error) => return error,
    };
    // SAFETY: The user is suspended with immutable private mappings; the API
    // validates all destination pages before copying this distinct stack record.
    if unsafe { process.space.copy_to_user(address, &info) }.is_err() {
        -14
    } else {
        length as i64
    }
}

fn application_event(process: &mut Process, handle: u64, address: u64, length: u64) -> i64 {
    let pid = match application_pid(process, handle) {
        Ok(pid) => pid,
        Err(error) => return error,
    };
    if length != windows::EVENT_SIZE as u64 {
        return -22;
    }
    let mut event = [0; windows::EVENT_SIZE];
    // SAFETY: The entire checked user record is snapshotted into kernel memory
    // before interpreting it; no retained user pointer can race event delivery.
    if unsafe { process.space.copy_from_user(address, &mut event) }.is_err() {
        return -14;
    }
    match with_windows(|session| {
        windows::validate_event(&event, session.smoke && cfg!(feature = "boot-smoke"))?;
        let index = session.index(pid)?;
        session.get_mut(pid)?.enqueue(event).map(|()| index)
    }) {
        Ok(index) => {
            sched::signal_queue(sched::Queue::App(index));
            0
        }
        Err(error) => error,
    }
}

fn application_read(process: &mut Process, handle: u64, address: u64, length: u64) -> i64 {
    let pid = match application_pid(process, handle) {
        Ok(pid) => pid,
        Err(error) => return error,
    };
    if !(28..=windows::READ_PACKET as u64).contains(&length) {
        return -22;
    }
    let length = length as usize;
    // Validate the complete in/out destination before even snapshotting its
    // header. Revision or geometry failures leave every user byte unchanged.
    if process.space.validate_writable(address, length).is_err() {
        return -14;
    }
    let mut packet = [0; windows::READ_PACKET];
    // SAFETY: Immutable user mappings, full range checked above, distinct packet.
    if unsafe { process.space.copy_from_user(address, &mut packet[..24]) }.is_err() {
        return -14;
    }
    if let Err(error) = with_windows(|session| session.get(pid)?.read(&mut packet[..length])) {
        return error;
    }
    // SAFETY: The same immutable range was fully validated before the read;
    // only kernel-owned front-buffer bytes and the original header are copied.
    if unsafe { process.space.copy_to_user(address, &packet[..length]) }.is_err() {
        -14
    } else {
        length as i64
    }
}

fn receive_app_input(
    process: &mut Process,
    session: &mut windows::Session,
    handle: u64,
    address: u64,
    length: u64,
) -> i64 {
    let pid = match process.handles.require(handle, capability::RECEIVE) {
        Ok(Capability {
            object: Object::AppInput(pid),
            ..
        }) if pid == process.pid => pid,
        Ok(_) => return -13,
        Err(error) => return cap_error(error),
    };
    if length != windows::EVENT_SIZE as u64 {
        return -22;
    }
    if process
        .space
        .validate_writable(address, windows::EVENT_SIZE)
        .is_err()
    {
        return -14;
    }
    let application = match session.get_mut(pid) {
        Ok(application) => application,
        Err(error) => return error,
    };
    let Some(event) = application.peek() else {
        return -11;
    };
    // SAFETY: The destination has been fully checked and no task can mutate its
    // mappings during this copy; only a successfully copied event is consumed.
    if unsafe { process.space.copy_to_user(address, &event) }.is_err() {
        return -14;
    }
    application.consume();
    process.input_events += 1;
    if u32::from_le_bytes(event[4..8].try_into().unwrap()) == 3 {
        process.input_lost += 1;
    }
    windows::EVENT_SIZE as i64
}

fn send(process: &mut Process, ipc: &mut Ipc, handle: u64, address: u64, length: u64) -> i64 {
    let channel = match channel_for(process, handle, capability::SEND) {
        Ok(channel) => channel,
        Err(error) => return error,
    };
    if !(1..=MESSAGE_SIZE as u64).contains(&length) {
        return -90;
    }
    let mut buffer = [0_u8; MESSAGE_SIZE];
    // SAFETY: The sole CPU suspended this user, its mappings are immutable,
    // and the kernel buffer cannot alias an owned user frame. No references
    // into the user address space escape the checked copy.
    if unsafe {
        process
            .space
            .copy_from_user(address, &mut buffer[..length as usize])
    }
    .is_err()
    {
        return -14;
    }
    match ipc.send(channel, &buffer[..length as usize]) {
        Ok(length) => {
            process.ipc_sent += 1;
            sched::signal_queue(sched::Queue::Channel(channel));
            length as i64
        }
        Err(error) => ipc_error(error),
    }
}

fn receive(
    process: &mut Process,
    ipc: &mut Ipc,
    handle: u64,
    address: u64,
    capacity: usize,
) -> i64 {
    let channel = match channel_for(process, handle, capability::RECEIVE) {
        Ok(channel) => channel,
        Err(error) => return error,
    };
    if !(1..=MESSAGE_SIZE).contains(&capacity) {
        return -90;
    }
    // Check the caller's entire advertised buffer even when the queue is empty,
    // before blocking or consuming anything. Repeat on wake to avoid retaining
    // authority or trusting an old validation across a scheduling boundary.
    if process.space.validate_writable(address, capacity).is_err() {
        return -14;
    }
    let message = match ipc.peek(channel) {
        Ok(message) => message,
        Err(error) => return ipc_error(error),
    };
    if message.length > capacity {
        return -90;
    }
    // SAFETY: User execution is suspended, mappings cannot race on the BSP,
    // and this kernel-owned message copy cannot alias destination user pages.
    // copy_to_user prevalidates every page, including effective write access.
    if unsafe {
        process
            .space
            .copy_to_user(address, &message.bytes[..message.length])
    }
    .is_err()
    {
        return -14;
    }
    ipc.consume(channel);
    process.ipc_received += 1;
    message.length as i64
}

struct SyscallResult {
    finished: bool,
    suspension: Option<State>,
}

pub(crate) fn syscall(frame: &mut TrapFrame) {
    // Reserve/protect/release always run after handler frames have unwound.
    if (vm_abi::SYS_VM_RESERVE..=vm_abi::SYS_VM_PROTECT).contains(&frame.rax) {
        suspend(frame, State::Memory(MemoryRequest::Operation));
        return;
    }
    let mut pending = None;
    with_active(frame, |process| {
        if let Some((address, length, writable)) = syscall_buffer(process, frame)
            && process
                .vm
                .preflight(&process.space, address, length, writable)
                == Ok(true)
        {
            pending = Some(MemoryRequest::SyscallBuffer {
                address,
                length,
                writable,
            });
        }
    });
    if let Some(request) = pending {
        suspend(frame, State::Memory(request));
        return;
    }
    let mut outcome = SyscallResult {
        finished: false,
        suspension: None,
    };
    // The frame is copied because with_active validates its immutable selectors
    // before borrowing the process; dispatch may then update the syscall result.
    let context = *frame;
    with_active(&context, |process| {
        outcome = dispatch_syscall(process, frame)
    });
    if outcome.finished {
        interrupts::return_from_user(frame);
    } else if let Some(state) = outcome.suspension {
        suspend(frame, state);
    } else if sched::take_reschedule() {
        // The call signaled a wait: the threads it woke run first (0030-D3).
        displace(frame);
    }
}

fn dispatch_syscall(process: &mut Process, frame: &mut TrapFrame) -> SyscallResult {
    let mut result = -38_i64;
    let mut finished = false;
    let mut suspension = None;
    (|| {
        match frame.rax {
            1 => {
                match process.handles.require(frame.rdx, capability::WRITE) {
                    Ok(capability) if capability.object == Object::Console => {}
                    Ok(_) => {
                        result = -13;
                        return;
                    }
                    Err(error) => {
                        result = cap_error(error);
                        return;
                    }
                }
                if frame.rsi > MAX_WRITE as u64 {
                    result = -22;
                    return;
                }
                let length = frame.rsi as usize;
                if length == 0 {
                    result = 0;
                    return;
                }
                let mut buffer = [0_u8; MAX_WRITE];
                // SAFETY: Tables are immutable while active, the process owns
                // its pages, and no other CPU can race validation/copy. The API
                // checks the entire user range before touching the destination.
                if unsafe {
                    process
                        .space
                        .copy_from_user(frame.rdi, &mut buffer[..length])
                }
                .is_err()
                {
                    #[cfg(feature = "boot-smoke")]
                    {
                        process.bad_pointers += 1;
                    }
                    result = -14;
                    return;
                }
                // The diagnostic console ABI accepts UTF-8 only. Trace kernels
                // echo it, escaping controls so an application cannot
                // manufacture kernel result log lines, and match it against
                // the test oracles. Default kernels keep every check and the
                // result but print and match nothing: their serial output
                // never carries user content (spec 0010-D4).
                result = match core::str::from_utf8(&buffer[..length]) {
                    #[cfg(feature = "test-trace")]
                    Ok(message) => {
                        serial::println!("user: {}", message.escape_debug());
                        #[cfg(feature = "boot-smoke")]
                        match_init_oracles(process, &buffer[..length]);
                        service_message(process, &buffer[..length]);
                        length as i64
                    }
                    #[cfg(not(feature = "test-trace"))]
                    Ok(_) => length as i64,
                    Err(_) => -22,
                };
            }
            2 => result = timer::ticks() as i64,
            clock_abi::SYS_CLOCK_GET => {
                if !clock_abi::clock_arguments_valid(frame.rsi, frame.rdx) {
                    result = -22;
                } else if let Some(snapshot) = crate::clock::snapshot() {
                    let bytes = snapshot.encode();
                    // SAFETY: The suspended process owns its mappings. The full
                    // checked destination is writable before any byte is copied;
                    // demand-zero destinations were materialized by the scheduler.
                    result = match unsafe { process.space.copy_to_user(frame.rdi, &bytes) } {
                        Ok(()) => bytes.len() as i64,
                        Err(_) => -14,
                    };
                } else {
                    // Source availability is checked before touching user memory.
                    result = -19;
                }
            }
            3 => {
                process.finish(Outcome::Exit(frame.rdi));
                finished = true;
            }
            4 => {
                result = 0;
                #[cfg(feature = "boot-smoke")]
                {
                    process.yields += 1;
                }
                suspension = Some(State::Ready);
            }
            5 => {
                if frame.rdi > MAX_SLEEP_TICKS {
                    result = -22;
                } else {
                    result = 0;
                    if frame.rdi == 0 {
                        #[cfg(feature = "boot-smoke")]
                        {
                            process.yields += 1;
                        }
                        suspension = Some(State::Ready);
                    } else {
                        #[cfg(feature = "boot-smoke")]
                        {
                            process.sleeps += 1;
                        }
                        suspension = Some(State::Sleeping(timer::ticks().wrapping_add(frame.rdi)));
                    }
                }
            }
            6 => result = process.pid as i64,
            7 => {
                result = match process.boot_handles.get(frame.rdi as usize) {
                    Some(&handle) => match process.handles.get(handle) {
                        Ok(_) => handle as i64,
                        Err(error) => cap_error(error),
                    },
                    None => -9,
                };
            }
            8 => result = with_ipc(|ipc| send(process, ipc, frame.rdi, frame.rsi, frame.rdx)),
            9 | 10 => {
                result =
                    with_ipc(|ipc| receive(process, ipc, frame.rdi, frame.rsi, frame.rdx as usize));
                if frame.rax == 10 && result == -11 {
                    #[cfg(feature = "boot-smoke")]
                    {
                        process.ipc_waits += 1;
                    }
                    // Only an open, empty channel the handle may receive
                    // from returns -11.
                    let channel = channel_for(process, frame.rdi, capability::RECEIVE)
                        .expect("blocked receive has a channel");
                    suspension = Some(State::Receiving(PendingReceive {
                        handle: frame.rdi,
                        address: frame.rsi,
                        capacity: frame.rdx as usize,
                        deadline: timer::ticks().wrapping_add(RECEIVE_TIMEOUT),
                        channel,
                    }));
                }
            }
            11 => {
                result = match process.handles.close(frame.rdi) {
                    Ok(capability) => {
                        with_ipc(|ipc| ipc.release(capability));
                        endpoint_released(capability);
                        signal_handle_waits(process, Some(frame.rdi));
                        0
                    }
                    Err(error) => cap_error(error),
                };
            }
            12 => {
                result = match process.handles.duplicate(frame.rdi, frame.rsi) {
                    Ok((handle, capability)) => {
                        with_ipc(|ipc| ipc.grant(capability));
                        handle as i64
                    }
                    Err(error) => cap_error(error),
                };
            }
            13 => result = surface_info(process, frame.rdi, frame.rsi, frame.rdx),
            14 => result = surface_present(process, frame.rdi, frame.rsi, frame.rdx),
            15 | 16 => {
                // Reject ungranted/wrong objects before entering the optional
                // session: finite init has no ACTIVE_INPUT pointer or grant.
                let routed = match process.handles.require(frame.rdi, capability::RECEIVE) {
                    Ok(capability) if capability.object == Object::Input => false,
                    Ok(Capability {
                        object: Object::AppInput(pid),
                        ..
                    }) if pid == process.pid => true,
                    Ok(_) => {
                        result = -13;
                        return;
                    }
                    Err(error) => {
                        result = cap_error(error);
                        return;
                    }
                };
                result = if routed {
                    with_windows(|session| {
                        receive_app_input(process, session, frame.rdi, frame.rsi, frame.rdx)
                    })
                } else {
                    with_input(|session| {
                        receive_input(process, session, frame.rdi, frame.rsi, frame.rdx)
                    })
                };
                if frame.rax == 15 && result == -11 {
                    process.input_waits += 1;
                    // An empty routed queue implies the app's window exists.
                    let queue = if routed {
                        sched::Queue::App(
                            with_windows(|session| session.index(process.pid))
                                .expect("blocked app input has a window"),
                        )
                    } else {
                        sched::Queue::Input
                    };
                    suspension = Some(State::InputWaiting(PendingInput {
                        handle: frame.rdi,
                        address: frame.rsi,
                        deadline: timer::ticks().wrapping_add(RECEIVE_TIMEOUT),
                        queue,
                    }));
                }
            }
            17 => {
                match process.handles.require(frame.rdi, capability::WRITE) {
                    Ok(capability) if capability.object == Object::Supervisor => {}
                    Ok(_) => {
                        result = -13;
                        return;
                    }
                    Err(error) => {
                        result = cap_error(error);
                        return;
                    }
                }
                if frame.rsi > 1 || frame.rdx != 0 {
                    result = -22;
                } else {
                    result = 0;
                    suspension = Some(State::Launching(frame.rsi));
                }
            }
            18 => result = application_info(process, frame.rdi, frame.rsi, frame.rdx),
            19 => result = application_event(process, frame.rdi, frame.rsi, frame.rdx),
            20 => result = application_read(process, frame.rdi, frame.rsi, frame.rdx),
            21 => {
                result = match application_pid(process, frame.rdi) {
                    Ok(pid) if frame.rsi == 0 && frame.rdx == 0 => {
                        suspension = Some(State::Closing(pid));
                        0
                    }
                    Ok(_) => -22,
                    Err(error) => error,
                };
            }
            22 => {
                result = match process.handles.require(frame.rdi, capability::WRITE) {
                    Ok(Capability {
                        object: Object::AppSurface(pid),
                        ..
                    }) if pid == process.pid => {
                        if frame.rsi != 0 || frame.rdx != 0 {
                            -22
                        } else {
                            with_windows(|session| session.get_mut(pid)?.commit())
                                .map_or_else(|error| error, |_| 0)
                        }
                    }
                    Ok(_) => -13,
                    Err(error) => cap_error(error),
                };
            }
            vm_abi::SYS_VM_STATS => {
                if frame.rsi != vm_abi::STATS_SIZE as u64 || frame.rdx != 0 {
                    result = -22;
                } else {
                    let stats = process.vm.stats();
                    let bytes = vm_abi::VmStats {
                        reserved_bytes: stats.reserved_bytes,
                        committed_bytes: stats.committed_bytes,
                        regions: stats.regions,
                        reserved_limit: vm_abi::RESERVED_LIMIT,
                        committed_limit: vm_abi::COMMITTED_LIMIT,
                    }
                    .encode();
                    // SAFETY: The suspended process owns the complete checked
                    // destination; no other user or device can change mappings.
                    result = match unsafe { process.space.copy_to_user(frame.rdi, &bytes) } {
                        Ok(()) => bytes.len() as i64,
                        Err(_) => -14,
                    };
                }
            }
            thread_abi::SYS_TLS_SET => {
                let bases = thread_abi::TlsBases {
                    fs: frame.rdi,
                    gs: frame.rsi,
                };
                if frame.rdx != 0 || !bases.is_valid() {
                    result = -22;
                } else {
                    // Validate both before changing either. Assembly restores
                    // these values only after Rust and return checks unwind.
                    frame.fs_base = bases.fs;
                    frame.gs_base = bases.gs;
                    frame.fs_selector = 0;
                    frame.gs_selector = 0;
                    result = 0;
                }
            }
            thread_abi::SYS_TLS_GET => {
                if frame.rsi != thread_abi::TLS_BASES_SIZE as u64 || frame.rdx != 0 {
                    result = -22;
                } else {
                    // The current trap captured actual hardware bases, which
                    // can change through user selector loads between syscalls.
                    let bytes = thread_abi::TlsBases {
                        fs: frame.fs_base,
                        gs: frame.gs_base,
                    }
                    .encode();
                    // SAFETY: The user is suspended; this complete checked copy
                    // cannot race mappings and leaves invalid output untouched.
                    result = match unsafe { process.space.copy_to_user(frame.rdi, &bytes) } {
                        Ok(()) => bytes.len() as i64,
                        Err(_) => -14,
                    };
                }
            }
            thread_abi::SYS_THREAD_CREATE => {
                result = create_thread(process, frame).map_or_else(|error| error, |tid| tid as i64);
            }
            thread_abi::SYS_THREAD_EXIT => {
                if frame.rsi != 0 || frame.rdx != 0 {
                    result = -22;
                } else {
                    #[cfg(feature = "boot-smoke")]
                    {
                        process.threads_exited += 1;
                    }
                    let last = process
                        .threads()
                        .filter(|thread| !matches!(thread.state, State::Exited(_)))
                        .count()
                        == 1;
                    if last {
                        process.thread_mut().state = State::Exited(frame.rdi);
                        process.finish(Outcome::Exit(frame.rdi));
                        finished = true;
                    } else {
                        result = 0;
                        suspension = Some(State::Exited(frame.rdi));
                    }
                }
            }
            thread_abi::SYS_THREAD_JOIN => match join_thread(process, frame) {
                Ok(state) => {
                    result = 0;
                    suspension = state;
                }
                Err(error) => result = error,
            },
            thread_abi::SYS_THREAD_ID => {
                result = if frame.rdi == 0 && frame.rsi == 0 && frame.rdx == 0 {
                    process.thread().id as i64
                } else {
                    -22
                };
            }
            thread_abi::SYS_WAIT_U32 => match wait_word(process, frame) {
                Ok(state) => {
                    result = 0;
                    suspension = Some(state);
                }
                Err(error) => result = error,
            },
            thread_abi::SYS_WAKE_U32 => {
                result = wake_word(process, frame).map_or_else(|error| error, |count| count as i64);
            }
            _ => {
                #[cfg(feature = "boot-smoke")]
                {
                    process.unknown_calls += 1;
                }
            }
        }
    })();
    frame.rax = result as u64;
    SyscallResult {
        finished,
        suspension,
    }
}

/// Runs after the selected timer's acknowledgement, only for interrupted CPL3.
/// A tick charges the interrupted user only; kernel and sleeping time are not
/// charged. This bounds delivered user IRQ ticks, not exact CPU or wall time.
pub(crate) fn timer_tick(frame: &mut TrapFrame) {
    let mut expired = false;
    let mut preempt = false;
    with_active(frame, |process| {
        process.charged_ticks += 1;
        process.thread_mut().charged_ticks += 1;
        process.thread_mut().quantum_left -= 1;
        let throttled = match &mut process.policy {
            CpuPolicy::Probe => false,
            CpuPolicy::Shell(policy) | CpuPolicy::Application(policy) => {
                policy.charge(timer::ticks())
            }
        };
        if matches!(process.policy, CpuPolicy::Probe) && process.charged_ticks >= USER_TICK_BUDGET {
            process.finish(Outcome::BudgetExhausted);
            expired = true;
        } else if throttled || process.thread().quantum_left == 0 {
            if throttled {
                process.throttles += 1;
            }
            process.preemptions += 1;
            #[cfg(feature = "boot-smoke")]
            {
                if process
                    .threads()
                    .filter(|thread| !matches!(thread.state, State::Exited(_)))
                    .count()
                    > 1
                {
                    process.thread_preemptions += 1;
                }
                if process.checks_passed {
                    process.post_checks_preemptions += 1;
                }
            }
            preempt = true;
        }
    });
    if expired {
        interrupts::return_from_user(frame);
    } else if preempt {
        suspend(frame, State::Ready);
    } else if sched::deadline_due(timer::ticks()) {
        // The deadline's own tick hands the CPU to the waits it ends, and the
        // charged thread keeps the rest of its quantum (0030-D3).
        displace(frame);
    }
}

/// A PS/2 IRQ interrupted CPL3, after its EOI. When it completed input that
/// a thread waits for, the interrupted thread returns to the scheduler with
/// no tick charged, its quantum unchanged and no preemption or throttle
/// counted: an IRQ fires per byte, not per tick (0030-D2). The scheduler
/// clears the arrival flag and wakes the waiters.
pub(crate) fn input_interrupt(frame: &mut TrapFrame) {
    if input::arrival_pending() && input::waiters() > 0 {
        displace(frame);
    }
}

fn suspend(frame: &mut TrapFrame, state: State) {
    park_current(frame, state, false);
}

/// Return the running thread to the scheduler still ready, so the threads
/// that an event woke dispatch first. It keeps the rest of its quantum and
/// then rejoins the run queue behind them (0030-D3).
fn displace(frame: &mut TrapFrame) {
    park_current(frame, State::Ready, true);
}

fn park_current(frame: &mut TrapFrame, state: State, displaced: bool) {
    // Validate before saving. Otherwise a malformed user stack could enter a
    // ready context and fail a kernel invariant on its next resume.
    prepare_return(frame);
    if frame.cs & 3 != 3 {
        return;
    }
    with_active(frame, |process| {
        assert!(matches!(
            state,
            State::Ready
                | State::Sleeping(_)
                | State::Receiving(_)
                | State::InputWaiting(_)
                | State::Launching(_)
                | State::Closing(_)
                | State::Memory(_)
                | State::Joining(_)
                | State::Waiting(_)
                | State::Exited(_)
        ));
        let thread = process.thread_mut();
        thread.context.frame = *frame;
        thread.displaced = displaced;
        park(process, state);
    });
    // The current handler must unwind before the scheduler touches other tasks.
    interrupts::return_from_user(frame);
}

pub(crate) fn fault(frame: &mut TrapFrame, cr2: u64) {
    let mut recoverable = false;
    with_active(frame, |process| {
        recoverable = frame.vector == 14 && process.vm.permits_fault(cr2, frame.error);
    });
    if recoverable {
        suspend(
            frame,
            State::Memory(MemoryRequest::Fault {
                address: cr2,
                error: frame.error,
            }),
        );
        return;
    }
    with_active(frame, |process| {
        record_fault(process, frame, cr2);
    });
    interrupts::return_from_user(frame);
}

fn record_fault(process: &mut Process, frame: &TrapFrame, cr2: u64) {
    process.finish(Outcome::Fault {
        vector: frame.vector,
        error: frame.error,
        address: cr2,
        rip: frame.rip,
    });
    serial::println!(
        "MIRROR_USER_FAULT vector={} error={:#x} rip={:#x} cr2={:#x}",
        frame.vector,
        frame.error,
        frame.rip,
        cr2
    );
}

/// Validate saved addresses and selectors against the native userspace contract
/// before IRETQ. Check every user return, including timer IRQs, and end invalid
/// contexts on the trusted continuation. Unmapped canonical user addresses may
/// return safely; subsequent user accesses produce ordinary contained faults.
pub(crate) fn prepare_return(frame: &mut TrapFrame) {
    let invalid = if !(PAGE_SIZE..(1_u64 << 47)).contains(&frame.rsp) {
        Some(Outcome::InvalidStack(frame.rsp))
    } else if !(PAGE_SIZE..(1_u64 << 47)).contains(&frame.rip)
        || frame.cs != 0x33
        || frame.ss != 0x2b
    {
        Some(Outcome::InvalidContext {
            rip: frame.rip,
            cs: frame.cs,
            ss: frame.ss,
        })
    } else if !interrupts::valid_user_tls(frame) {
        Some(Outcome::InvalidTls {
            fs: frame.fs_base,
            gs: frame.gs_base,
            fs_selector: frame.fs_selector,
            gs_selector: frame.gs_selector,
        })
    } else {
        None
    };
    if let Some(outcome) = invalid {
        with_active(frame, |process| {
            process.finish(outcome);
        });
        interrupts::return_from_user(frame);
    }
}
