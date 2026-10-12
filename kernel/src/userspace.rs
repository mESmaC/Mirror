//! Native BSP scheduling with finite probes and periodically limited services.
//! IRQ/syscall handlers return to the scheduler before another process resumes.
//! User buffers are checked and copied through the direct map before use.

use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::{
    arch,
    capability::{self, CapError, Capability, HandleTable, Object},
    context::Context,
    elf, framebuffer, input,
    interrupts::{self, TrapFrame},
    ipc::{Ipc, IpcError, MESSAGE_SIZE},
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PageTables, PagingError, UserAddressSpace, UserPermissions},
    ps2, serial, stacks, surface, timer, vm, windows,
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
// Finite init probes include register/preemption loops and 21 thread creations.
// Leave headroom for the complete debug workload, including IRQs delivered on
// syscall return. This finite lifetime allowance is shared by all its threads;
// it does not change the independent recurring budgets of ordinary services.
const USER_TICK_BUDGET: u64 = 128;
const MAX_SLEEP_TICKS: u64 = 1000;
const RECEIVE_TIMEOUT: u64 = 100;
const SERVICE_ALLOWANCE: u64 = 32;
const SERVICE_PERIOD: u64 = 100;
#[cfg(feature = "test-trace")]
const DESKTOP_READY: &[u8] = b"Mirror desktop: ready\n";
#[cfg(feature = "test-trace")]
const DESKTOP_FRAME: &[u8] = b"Mirror desktop: frame\n";
#[cfg(feature = "test-trace")]
const DESKTOP_CHECKS: &[u8] = b"Mirror desktop: checks passed\n";
#[cfg(feature = "test-trace")]
const AGENT_READY: &[u8] = b"Mirror agent: ready\n";
#[cfg(feature = "test-trace")]
const AGENT_CHECKS: &[u8] = b"Mirror agent: checks passed\n";

// Only the BSP accesses this slot, with IF=0. It is populated immediately before
// entry and cleared after the synchronous return; it never owns the context.
static ACTIVE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_IPC: AtomicPtr<Ipc> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_SURFACE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_INPUT: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_WINDOWS: AtomicPtr<windows::Session> = AtomicPtr::new(core::ptr::null_mut());
static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static NEXT_TID: AtomicU64 = AtomicU64::new(1);

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

struct Thread {
    id: u64,
    context: Context,
    state: State,
    joiner: Option<u64>,
    charged_ticks: u64,
    sleep_charge: u64,
    quantum_left: u64,
}

impl Thread {
    fn new(entry: u64, stack: u64, argument: u64) -> Self {
        let id = NEXT_TID.fetch_add(1, Ordering::Relaxed);
        assert!((1..=i64::MAX as u64).contains(&id), "thread IDs exhausted");
        Self {
            id,
            context: Context::new(entry, stack, argument),
            state: State::Ready,
            joiner: None,
            charged_ticks: 0,
            sleep_charge: 0,
            quantum_left: QUANTUM_TICKS,
        }
    }
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingInput {
    handle: u64,
    address: u64,
    deadline: u64,
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
    Desktop(ServicePolicy),
    Agent(ServicePolicy),
    Shell(ServicePolicy),
    Application(ServicePolicy),
}

// Fields gated on `boot-smoke` are bookkeeping that only init's self-test
// battery reads. Fields gated on `test-trace` feed only trace markers. Neither
// exists in a default kernel (spec 0010).
struct Process<'kernel> {
    pid: u64,
    space: UserAddressSpace<'kernel>,
    vm: vm::Vm,
    direct_map: u64,
    #[cfg(feature = "boot-smoke")]
    vm_checks: bool,
    #[cfg(feature = "boot-smoke")]
    demand_faults: u64,
    #[cfg(feature = "boot-smoke")]
    copy_faults: u64,
    threads: [Option<Thread>; thread_abi::MAX_THREADS],
    current_thread: usize,
    next_thread: usize,
    #[cfg(feature = "boot-smoke")]
    thread_checks: bool,
    #[cfg(feature = "boot-smoke")]
    threads_created: u64,
    #[cfg(feature = "boot-smoke")]
    threads_joined: u64,
    #[cfg(feature = "boot-smoke")]
    threads_exited: u64,
    #[cfg(feature = "boot-smoke")]
    threads_cancelled: u64,
    #[cfg(feature = "boot-smoke")]
    thread_preemptions: u64,
    next_waiter: usize,
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
    agent_ready: bool,
    agent_checks: bool,
    input_events: u64,
    input_lost: u64,
    input_waits: u64,
    input_timeouts: u64,
}

impl Process<'_> {
    fn thread(&self) -> &Thread {
        self.threads[self.current_thread]
            .as_ref()
            .expect("current thread")
    }

    fn thread_mut(&mut self) -> &mut Thread {
        self.threads[self.current_thread]
            .as_mut()
            .expect("current thread")
    }

    fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    fn finish(&mut self, outcome: Outcome) {
        assert!(self.outcome.is_none());
        self.outcome = Some(outcome);
        for thread in self.threads.iter_mut().flatten() {
            #[cfg(feature = "boot-smoke")]
            {
                if !matches!(thread.state, State::Exited(_)) {
                    self.threads_cancelled += 1;
                }
                if matches!(thread.state, State::Waiting(_)) {
                    self.sync_cancelled += 1;
                }
            }
            thread.state = State::Finished;
        }
    }

    fn ready(&self) -> bool {
        !self.finished()
            && self
                .threads
                .iter()
                .flatten()
                .any(|thread| thread.state == State::Ready)
    }

    fn select_thread(&mut self) {
        assert!(!self.finished());
        let index = (0..self.threads.len())
            .map(|offset| (self.next_thread + offset) % self.threads.len())
            .find(|&index| {
                self.threads[index]
                    .as_ref()
                    .is_some_and(|thread| thread.state == State::Ready)
            })
            .expect("process has no runnable thread");
        self.current_thread = index;
        self.next_thread = (index + 1) % self.threads.len();
    }
}

/// Run init's self-test battery. Only smoke kernels run init: default and
/// trace kernels start the shell, or the agent-demo desktop, as the first
/// userspace process instead (spec 0010-D3).
#[cfg(feature = "boot-smoke")]
pub fn run_init(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    direct_map: u64,
    scenario: u64,
    mut display: Option<&mut framebuffer::Driver<'_>>,
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

fn create_process<'kernel>(
    kernel: &'kernel PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
    argument: u64,
) -> Result<Process<'kernel>, PagingError> {
    // SAFETY: The BSP is alone with IF=0, kernel root active, and the shared
    // upper mappings and unique allocator remain live throughout this run.
    let space = unsafe { UserAddressSpace::new(kernel, frames) }?;
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    assert!(
        (1..=i64::MAX as u64).contains(&pid),
        "process IDs exhausted"
    );
    let mut process = Process {
        pid,
        space,
        vm: vm::Vm::new(),
        direct_map,
        #[cfg(feature = "boot-smoke")]
        vm_checks: false,
        #[cfg(feature = "boot-smoke")]
        demand_faults: 0,
        #[cfg(feature = "boot-smoke")]
        copy_faults: 0,
        threads: core::array::from_fn(|index| {
            (index == 0).then(|| Thread::new(image.entry, STACK_TOP - 8, argument))
        }),
        current_thread: 0,
        next_thread: 0,
        #[cfg(feature = "boot-smoke")]
        thread_checks: false,
        #[cfg(feature = "boot-smoke")]
        threads_created: 1,
        #[cfg(feature = "boot-smoke")]
        threads_joined: 0,
        #[cfg(feature = "boot-smoke")]
        threads_exited: 0,
        #[cfg(feature = "boot-smoke")]
        threads_cancelled: 0,
        #[cfg(feature = "boot-smoke")]
        thread_preemptions: 0,
        next_waiter: 0,
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
        agent_ready: false,
        agent_checks: false,
        input_events: 0,
        input_lost: 0,
        input_waits: 0,
        input_timeouts: 0,
    };
    if let Err(error) = load(&mut process.space, frames, bytes, image, direct_map) {
        // SAFETY: This root has never been active; mapped frames belong to it.
        unsafe { process.space.destroy(frames) }.expect("failed-load cleanup");
        return Err(error);
    }
    serial::println!(
        "Native process: pid={} entry={:#x} root={:#x} pages={} tables={}",
        pid,
        image.entry,
        process.space.root_physical_address(),
        process.space.owned_page_count(),
        process.space.owned_table_count()
    );
    Ok(process)
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
    let mut surface = display
        .map(surface::Session::new)
        .transpose()
        .expect("surface session");
    let before = frames.stats().expect("recycling required").allocated_frames;
    let mut first = create_process(kernel, frames, bytes, image, direct_map, scenario)
        .expect("load first native process");
    let second = match create_process(kernel, frames, bytes, image, direct_map, 1 << 8) {
        Ok(process) => process,
        Err(error) => {
            // SAFETY: Neither process has been activated; the failed creation
            // cleaned its own tree and this first process has no live users.
            unsafe { first.space.destroy(frames) }.expect("peer-load cleanup");
            assert_eq!(frames.stats().unwrap().allocated_frames, before);
            panic!("could not load peer: {error:?}");
        }
    };
    let mut processes = [first, second];
    let mut ipc = Ipc::new();
    for (index, process) in processes.iter_mut().enumerate() {
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
    }
    if surface.is_some() {
        let handle = processes[0]
            .handles
            .insert(Object::Surface, capability::WRITE)
            .expect("bootstrap surface capability");
        processes[0].boot_handles[3] = handle;
        // Trusted verification knows the owner's token but grants no entry to
        // the peer. Its real IPC fixture must receive and try this exact token.
        processes[1].foreign_surface = handle;
        assert_eq!(processes[0].handles.len(), 4);
    }
    assert_eq!(processes[1].handles.len(), 3);
    assert_eq!(processes[1].boot_handles[3], 0);
    assert!(processes.iter().all(|process| process.boot_handles[4] == 0));
    assert!(ACTIVE_IPC.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_SURFACE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_INPUT.load(Ordering::SeqCst).is_null());
    assert_ne!(processes[0].pid, processes[1].pid);
    assert_ne!(
        processes[0].space.root_physical_address(),
        processes[1].space.root_physical_address()
    );
    assert_ne!(
        processes[0]
            .space
            .translate(STACK_BASE)
            .unwrap()
            .unwrap()
            .physical,
        processes[1]
            .space
            .translate(STACK_BASE)
            .unwrap()
            .unwrap()
            .physical
    );
    let stack_top = stacks::bsp_rsp0_top();
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    // SAFETY: The BSP's guarded RSP0 stack is mapped in the shared upper half
    // of every root. IF=0 and no user process is active.
    unsafe { interrupts::set_kernel_stack(stack_top) };
    let mut cursor = 0;
    let mut last = None;
    let mut switches = 0;
    let mut idle_ticks = 0;
    while processes.iter().any(|process| !process.finished()) {
        assert!(!arch::interrupts_enabled());
        release_finished_handles(&mut processes, &mut ipc);
        let now = timer::ticks();
        for process in &mut processes {
            wake_threads(process, &mut ipc, None, None, now);
        }
        let selected = (0..PROCESS_COUNT)
            .map(|offset| (cursor + offset) % PROCESS_COUNT)
            .find(|&index| processes[index].ready());
        let Some(index) = selected else {
            assert!(processes.iter().any(|process| !process.finished()
                && process.threads.iter().flatten().any(|thread| matches!(
                    thread.state,
                    State::Sleeping(_) | State::Receiving(_) | State::Waiting(_)
                ))));
            let before_idle = timer::ticks();
            let before_charge: u64 = processes.iter().map(|process| process.charged_ticks).sum();
            // SAFETY: No active process or locks, kernel root restored, timer
            // ready. STI's interrupt shadow closes the check-to-HLT wakeup race.
            // The CPL0 IRQ path only acknowledges/increments; CLI restores IF=0.
            unsafe { core::arch::asm!("sti", "hlt", "cli", options(nostack)) };
            idle_ticks += timer::ticks().wrapping_sub(before_idle);
            assert_eq!(
                processes
                    .iter()
                    .map(|process| process.charged_ticks)
                    .sum::<u64>(),
                before_charge,
                "idle charged user CPU ticks"
            );
            continue;
        };
        cursor = (index + 1) % PROCESS_COUNT;
        if last != Some(index) {
            switches += 1;
            last = Some(index);
        }
        resume_process(
            kernel,
            frames,
            &mut processes[index],
            &mut ipc,
            surface.as_mut(),
            None,
            None,
        );
        if processes[index].outcome == Some(Outcome::BudgetExhausted) {
            assert_eq!(index, 0, "peer exhausted its budget");
            assert_eq!(scenario, 7, "unexpected budget exhaustion");
            assert_eq!(
                processes[1].outcome,
                Some(Outcome::Exit(0)),
                "CPU-bound task starved peer"
            );
            assert!(
                processes[0].post_checks_preemptions > 0,
                "CPU-only loop was never preempted"
            );
        }
    }
    release_finished_handles(&mut processes, &mut ipc);
    assert!(ACTIVE_SURFACE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_INPUT.load(Ordering::SeqCst).is_null());
    assert!(
        ipc.is_drained(),
        "IPC messages or endpoint references leaked"
    );
    for process in &mut processes {
        assert!(process.handles.is_empty());
        // SAFETY: All contexts are finished, ACTIVE is clear, the kernel root
        // is active, and no handler retains a reference into either user tree.
        unsafe { process.space.destroy(frames) }.expect("release user address space");
    }
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before,
        "user frame leak"
    );
    serial::println!("MIRROR_USER_CLEANUP_OK allocated={}", before);
    serial::println!("MIRROR_VM_CLEANUP_OK allocated={}", before);
    assert_eq!(
        processes[1].surface_tiles, 0,
        "peer painted without authority"
    );
    // Taking the session releases its display borrow when this branch ends.
    if let Some(session) = surface.take() {
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
    validate_process(&processes[0], scenario);
    validate_process(&processes[1], 0);
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
    let yields: usize = processes.iter().map(|process| process.yields).sum();
    let sleeps: usize = processes.iter().map(|process| process.sleeps).sum();
    let preemptions: usize = processes.iter().map(|process| process.preemptions).sum();
    assert!(switches >= 4, "processes did not alternate");
    serial::println!(
        "MIRROR_SCHEDULER_OK processes={} switches={} yields={} sleeps={} preemptions={} idle_ticks={}",
        PROCESS_COUNT,
        switches,
        yields,
        sleeps,
        preemptions,
        idle_ticks
    );
}

/// Publish disjoint I/O sessions through user execution and deferred syscall
/// completion. Revoke the process pointer before VM work; every handler has
/// unwound before scheduler-owned memory is accessed again.
fn resume_process(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    process: &mut Process<'_>,
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
    process.select_thread();
    assert_eq!(process.thread().state, State::Ready);
    process.thread_mut().state = State::Running;
    process.thread_mut().quantum_left = QUANTUM_TICKS;
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

/// Complete bounded VM work with the kernel root active and no interrupt frames
/// or published process borrow. Disjoint I/O sessions remain borrowed by resume.
fn complete_memory(
    process: &mut Process<'_>,
    frames: &mut FrameAllocator<'_>,
    request: MemoryRequest,
) {
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
            if result.is_ok()
                && (frame.rax == vm_abi::SYS_VM_RELEASE
                    || (frame.rax == vm_abi::SYS_VM_PROTECT && frame.rdx == vm_abi::PROT_NONE))
            {
                // Invalidate the old wait key before any sibling can run or
                // re-reserve this address. Failed transactional changes leave
                // waiters untouched. No backing page or raw pointer is pinned.
                invalidate_waits(process, frame.rdi, frame.rsi);
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
                process.thread_mut().state = outcome.suspension.unwrap_or(State::Ready);
                if matches!(
                    process.thread().state,
                    State::Sleeping(_)
                        | State::Receiving(_)
                        | State::InputWaiting(_)
                        | State::Waiting(_)
                ) {
                    process.thread_mut().sleep_charge = process.thread().charged_ticks;
                }
            }
        }
    }
}

/// Describe only authorized, bounded copy operations. Bad handles, wrong object
/// types and malformed lengths keep their existing error precedence and cannot
/// cause page allocation. Actual dispatch repeats all validation after page-in.
fn syscall_buffer(process: &Process<'_>, frame: &TrapFrame) -> Option<(u64, usize, bool)> {
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

/// Run a native desktop and its capability-connected automation client. Each
/// service has its own renewable CPU allowance. The desktop owns the session;
/// its termination also cancels a client that has not already exited or faulted.
pub fn run_desktop(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    programs: [&[u8]; 2],
    direct_map: u64,
    display: &mut framebuffer::Driver<'_>,
    consumer: &mut ps2::Consumer,
    smoke: bool,
) {
    assert!(!arch::interrupts_enabled());
    let before = frames.stats().expect("recycling required").allocated_frames;
    let (desktop_outcome, agent_outcome) = {
        // Validate both immutable modules before acquiring either address space.
        let desktop_image = elf::parse(programs[0]).expect("invalid native desktop ELF");
        let agent_image = elf::parse(programs[1]).expect("invalid native agent ELF");
        let mut desktop = create_process(
            kernel,
            frames,
            programs[0],
            &desktop_image,
            direct_map,
            u64::from(smoke),
        )
        .expect("load native desktop");
        let mut agent = match create_process(
            kernel,
            frames,
            programs[1],
            &agent_image,
            direct_map,
            u64::from(smoke),
        ) {
            Ok(process) => process,
            Err(error) => {
                // SAFETY: Neither root has been active. Failed creation released
                // its own mappings; the desktop owns all remaining user pages.
                unsafe { desktop.space.destroy(frames) }.expect("agent-load cleanup");
                assert_eq!(frames.stats().unwrap().allocated_frames, before);
                panic!("could not load native agent: {error:?}");
            }
        };
        let now = timer::ticks();
        desktop.policy = CpuPolicy::Desktop(ServicePolicy::new(now));
        agent.policy = CpuPolicy::Agent(ServicePolicy::new(now));
        let mut processes = [desktop, agent];
        let mut ipc = Ipc::new();
        let mut surface = surface::Session::desktop(display).expect("desktop surface");
        let (framebuffer, region) = surface.placement();
        let mut input = input::Session::new(consumer, framebuffer, region);
        for (index, process) in processes.iter_mut().enumerate() {
            // Channel 0 carries requests to the desktop; channel 1 carries
            // replies/jobs to the agent. Handles never cross process tables.
            for (slot, object, rights) in [
                (0, Object::Console, capability::WRITE),
                (1, Object::Channel(1 - index), capability::SEND),
                (2, Object::Channel(index), capability::RECEIVE),
            ] {
                process.boot_handles[slot] = process
                    .handles
                    .insert(object, rights)
                    .expect("service bootstrap capability");
                ipc.grant(Capability { object, rights });
            }
        }
        for (slot, object, rights) in [
            (3, Object::Surface, capability::WRITE),
            (4, Object::Input, capability::RECEIVE),
        ] {
            processes[0].boot_handles[slot] = processes[0]
                .handles
                .insert(object, rights)
                .expect("desktop bootstrap capability");
        }
        assert_eq!(processes[0].handles.len(), 5);
        assert_eq!(processes[1].handles.len(), 3);
        assert_eq!(processes[1].boot_handles[3..], [0, 0, 0]);
        let stack_top = stacks::bsp_rsp0_top();
        // SAFETY: IF=0, no active user, and the BSP's guarded RSP0 stack
        // remains mapped in the shared upper half of every process root.
        unsafe { interrupts::set_kernel_stack(stack_top) };

        let mut cursor = 0;
        while !processes[0].finished() {
            assert!(!arch::interrupts_enabled());
            // Endpoint closure is visible before checking a peer's pending
            // receive. A client may exit while the desktop remains interactive.
            release_finished_handles(&mut processes, &mut ipc);
            let now = timer::ticks();
            for (index, process) in processes.iter_mut().enumerate() {
                wake_threads(
                    process,
                    &mut ipc,
                    if index == 0 { Some(&mut input) } else { None },
                    None,
                    now,
                );
            }
            let mut selected = None;
            for offset in 0..processes.len() {
                let index = (cursor + offset) % processes.len();
                let process = &mut processes[index];
                let policy = match &mut process.policy {
                    CpuPolicy::Desktop(policy) | CpuPolicy::Agent(policy) => policy,
                    _ => panic!("service lost its recurring allowance"),
                };
                // Wakes and yields cannot reset either service's spent credit.
                if policy.eligible(now) && process.ready() {
                    selected = Some(index);
                    break;
                }
            }
            if let Some(index) = selected {
                cursor = (index + 1) % processes.len();
                resume_process(
                    kernel,
                    frames,
                    &mut processes[index],
                    &mut ipc,
                    if index == 0 { Some(&mut surface) } else { None },
                    if index == 0 { Some(&mut input) } else { None },
                    None,
                );
            } else {
                let before_charge = processes.each_ref().map(|process| process.charged_ticks);
                // SAFETY: No published user/session pointers or outstanding
                // borrows. IF stays clear from the queue/deadline check through
                // STI-HLT, so a pending input IRQ cannot create a lost wakeup.
                unsafe { core::arch::asm!("sti", "hlt", "cli", options(nostack)) };
                assert_eq!(
                    processes.each_ref().map(|process| process.charged_ticks),
                    before_charge,
                    "service idle/throttling charged user CPU ticks"
                );
            }
        }
        if !processes[1].finished() {
            assert!(processes[1].outcome.is_none());
            processes[1].finish(Outcome::SessionEnded);
        }
        assert!(ACTIVE.load(Ordering::SeqCst).is_null());
        assert!(ACTIVE_IPC.load(Ordering::SeqCst).is_null());
        assert!(ACTIVE_INPUT.load(Ordering::SeqCst).is_null());
        assert!(ACTIVE_SURFACE.load(Ordering::SeqCst).is_null());
        assert_eq!(
            surface.totals(),
            (processes[0].surface_tiles, processes[0].surface_pixels)
        );
        assert_eq!(processes[1].surface_tiles, 0, "agent used display directly");
        assert_eq!(processes[1].input_events, 0, "agent read hardware input");
        release_finished_handles(&mut processes, &mut ipc);
        assert!(ipc.is_drained());
        for process in &mut processes {
            assert!(process.handles.is_empty());
            // SAFETY: Both processes are finished, all handlers unwound, every
            // published pointer is clear, and the kernel root is active with IF=0.
            unsafe { process.space.destroy(frames) }.expect("service address-space cleanup");
        }
        assert_eq!(
            frames.stats().unwrap().allocated_frames,
            before,
            "desktop/agent frame leak"
        );
        let process = &processes[0];
        let outcome = process.outcome.expect("desktop finished without outcome");
        let agent = &processes[1];
        let agent_outcome = agent.outcome.expect("agent finished without outcome");
        if smoke {
            assert!(
                matches!(outcome, Outcome::Fault { vector: 6, .. }),
                "desktop smoke did not end in contained UD2: {outcome:?}"
            );
            assert!(process.desktop_checks, "desktop smoke checks missing");
            assert!(process.throttles > 0, "desktop allowance never throttled");
            assert!(process.input_timeouts > 0, "desktop input never timed out");
            assert!(
                process.input_lost > 0,
                "desktop did not read initial loss barrier"
            );
            assert!(process.input_waits > 0);
            serial::println!(
                "MIRROR_DESKTOP_TESTS_OK throttles={} timeouts={} input_events={}",
                process.throttles,
                process.input_timeouts,
                process.input_events
            );
            assert!(agent.agent_ready, "agent ready marker missing");
            assert!(agent.agent_checks, "agent checks did not complete");
            assert_eq!(
                agent_outcome,
                Outcome::Exit(0),
                "agent did not exit successfully"
            );
            assert!(agent.dispatches > 1, "agent never resumed");
            assert!(
                agent.ipc_sent > 0 && agent.ipc_received > 0,
                "agent did not exchange IPC"
            );
            serial::println!(
                "MIRROR_AGENT_TESTS_OK sent={} received={} dispatches={}",
                agent.ipc_sent,
                agent.ipc_received,
                agent.dispatches
            );
        }
        // Surface and input sessions leave scope before the caller can start
        // the kernel console. No service handle or frame survives this return.
        (outcome, agent_outcome)
    };
    match desktop_outcome {
        Outcome::Exit(status) => serial::println!(
            "MIRROR_DESKTOP_CLEANUP_OK reason=exit handles_reclaimed=1 status={}",
            status
        ),
        _ => serial::println!("MIRROR_DESKTOP_CLEANUP_OK reason=fault handles_reclaimed=1"),
    }
    match agent_outcome {
        Outcome::Exit(status) => serial::println!(
            "MIRROR_AGENT_CLEANUP_OK reason=exit handles_reclaimed=1 status={}",
            status
        ),
        Outcome::SessionEnded => {
            serial::println!("MIRROR_AGENT_CLEANUP_OK reason=session_ended handles_reclaimed=1")
        }
        outcome => serial::println!(
            "MIRROR_AGENT_CLEANUP_OK reason=fault handles_reclaimed=1 outcome={:?}",
            outcome
        ),
    }
}

struct ShellRuntime<'kernel, 'image> {
    kernel: &'kernel PageTables,
    bytes: &'image [u8],
    image: elf::Image,
    direct_map: u64,
    children: [Option<Process<'kernel>>; windows::APP_COUNT],
    windows: windows::Session,
}

impl<'kernel> ShellRuntime<'kernel, '_> {
    fn launch(
        &mut self,
        frames: &mut FrameAllocator<'_>,
        desktop: &mut Process<'kernel>,
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
        let mut child = create_process(
            self.kernel,
            frames,
            self.bytes,
            &self.image,
            self.direct_map,
            kind,
        )
        .map_err(|_| -12)?;
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
            match desktop
                .handles
                .insert(Object::Application(pid), capability::WRITE)
            {
                Ok(handle) => Ok((application, handle)),
                Err(error) => {
                    // SAFETY: No application resource or handle was published.
                    unsafe { application.destroy(frames) };
                    Err(cap_error(error))
                }
            }
        })();
        let (application, handle) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
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
        self.children[index] = Some(child);
        self.windows.applications[index] = Some(application);
        self.windows.launches += 1;
        serial::println!("MIRROR_APP_STARTED pid={} kind={}", pid, kind);
        Ok(handle)
    }

    fn reclaim(&mut self, index: usize, frames: &mut FrameAllocator<'_>, ipc: &mut Ipc) {
        let mut child = self.children[index]
            .take()
            .expect("reclaim live child slot");
        assert!(child.finished());
        let outcome = child.outcome.expect("child ended without outcome");
        release_finished_handles(core::slice::from_mut(&mut child), ipc);
        assert!(child.handles.is_empty());
        let before = frames.stats().unwrap().allocated_frames;
        let owned = child.space.owned_page_count() + child.space.owned_table_count();
        // SAFETY: The scheduler restored its kernel root and cleared every
        // active pointer before considering a finished process for reclamation.
        unsafe { child.space.destroy(frames) }.expect("application address-space cleanup");
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
            if self.children[index]
                .as_ref()
                .is_some_and(|child| child.finished())
            {
                self.reclaim(index, frames, ipc);
            }
        }
    }

    fn close(
        &mut self,
        frames: &mut FrameAllocator<'_>,
        desktop: &mut Process<'_>,
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
        if let Some(child) = &mut self.children[index] {
            assert_ne!(child.thread().state, State::Running);
            if !child.finished() {
                child.finish(reason);
            }
            self.reclaim(index, frames, ipc);
        }
        // Revoke all duplicates before a slot can be reused for a fresh PID.
        desktop.handles.revoke_application(pid);
        let mut application = self.windows.applications[index].take().unwrap();
        let before = frames.stats().unwrap().allocated_frames;
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

fn service_eligible(process: &mut Process<'_>, now: u64) -> bool {
    let policy = match &mut process.policy {
        CpuPolicy::Shell(policy) | CpuPolicy::Application(policy) => policy,
        _ => panic!("invalid shell scheduling policy"),
    };
    policy.eligible(now) && process.ready()
}

/// Every wake runs with the kernel root active. Siblings may have changed a
/// shared mapping or closed a shared handle while this thread was blocked, so
/// the original checked receive helpers repeat permission/range validation.
fn wake_threads(
    process: &mut Process<'_>,
    ipc: &mut Ipc,
    mut input: Option<&mut input::Session<'_>>,
    mut windows: Option<&mut windows::Session>,
    now: u64,
) {
    if process.finished() {
        return;
    }
    complete_joins(process);
    for index in 0..process.threads.len() {
        if process.threads[index].is_none() {
            continue;
        }
        process.current_thread = index;
        let state = process.thread().state;
        if matches!(
            state,
            State::Sleeping(_)
                | State::Receiving(_)
                | State::InputWaiting(_)
                | State::Joining(_)
                | State::Waiting(_)
        ) {
            assert_eq!(
                process.thread().charged_ticks,
                process.thread().sleep_charge,
                "blocked thread charged user CPU ticks"
            );
        }
        match state {
            State::Sleeping(deadline) if deadline_reached(now, deadline) => {
                process.thread_mut().state = State::Ready;
            }
            State::Waiting(pending) if pending.expired(now) => {
                complete_wait(process.thread_mut(), -110);
                #[cfg(feature = "boot-smoke")]
                {
                    process.sync_timeouts += 1;
                }
            }
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
                if result != -11 {
                    #[cfg(feature = "boot-smoke")]
                    if result == -32 {
                        process.ipc_closed_wakes += 1;
                    }
                    process.thread_mut().context.frame.rax = result as u64;
                    process.thread_mut().state = State::Ready;
                }
            }
            State::InputWaiting(pending) => {
                // Resolve the capability afresh: it may have been closed by a
                // sibling since the wait began. Session availability is tied
                // to process authority, never to the thread that issued it.
                let mut result = match process.handles.require(pending.handle, capability::RECEIVE)
                {
                    Ok(cap) => match cap.object {
                        Object::Input => receive_input(
                            process,
                            input.as_deref_mut().expect("input owner session"),
                            pending.handle,
                            pending.address,
                            input::EVENT_SIZE as u64,
                        ),
                        Object::AppInput(pid) if pid == process.pid => receive_app_input(
                            process,
                            windows.as_deref_mut().expect("application input session"),
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
                    // Whether this prints depends on input timing, so it is a
                    // per-input marker and only trace kernels print it (0010).
                    #[cfg(feature = "test-trace")]
                    if matches!(process.policy, CpuPolicy::Desktop(_))
                        && process.input_timeouts == 1
                    {
                        serial::println!("MIRROR_DESKTOP_INPUT_TIMEOUT");
                    }
                }
                if result != -11 {
                    process.thread_mut().context.frame.rax = result as u64;
                    process.thread_mut().state = State::Ready;
                }
            }
            State::Ready
            | State::Sleeping(_)
            | State::Joining(_)
            | State::Waiting(_)
            | State::Exited(_)
            | State::Launching(_)
            | State::Closing(_) => {}
            State::Running | State::Memory(_) | State::Finished => {
                panic!("invalid thread suspension")
            }
        }
    }
}

fn complete_wait(thread: &mut Thread, result: i64) {
    assert!(matches!(thread.state, State::Waiting(_)));
    assert_eq!(
        thread.charged_ticks, thread.sleep_charge,
        "wait charged user CPU ticks"
    );
    thread.context.frame.rax = result as u64;
    thread.state = State::Ready;
}

/// Compare and enqueue are one non-preemptible operation on the sole CPU.
/// Deferred page-in also completes with IF=0 before another user can run. An
/// SMP implementation must instead serialize the value check and wait queue
/// with wake/invalidation under the same lock; disabling local IRQs won't do.
fn wait_word(process: &mut Process<'_>, frame: &TrapFrame) -> Result<State, i64> {
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
fn wake_word(process: &mut Process<'_>, frame: &TrapFrame) -> Result<u64, i64> {
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
    let now = timer::ticks();
    let first = process.next_waiter;
    let mut woken = 0;
    for offset in 0..process.threads.len() {
        let index = (first + offset) % process.threads.len();
        let Some(thread) = &mut process.threads[index] else {
            continue;
        };
        let State::Waiting(pending) = thread.state else {
            continue;
        };
        if pending.address != frame.rdi {
            continue;
        }
        if pending.expired(now) {
            complete_wait(thread, -110);
            #[cfg(feature = "boot-smoke")]
            {
                process.sync_timeouts += 1;
            }
        } else if woken < frame.rsi {
            complete_wait(thread, 0);
            woken += 1;
            process.next_waiter = (index + 1) % process.threads.len();
        }
    }
    #[cfg(feature = "boot-smoke")]
    {
        process.sync_wakes += woken;
    }
    Ok(woken)
}

fn invalidate_waits(process: &mut Process<'_>, address: u64, length: u64) {
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    assert!(!arch::interrupts_enabled());
    let end = address.checked_add(length).expect("successful VM interval");
    for thread in process.threads.iter_mut().flatten() {
        if let State::Waiting(pending) = thread.state
            && (address..end).contains(&pending.address)
        {
            complete_wait(thread, -14);
            #[cfg(feature = "boot-smoke")]
            {
                process.sync_invalidations += 1;
            }
        }
    }
}

/// Reap only after resume_user has saved/scrubbed the exiting context's FPU
/// state and released its raw pointer. A join result needs no user buffer and
/// is delivered atomically in the waiting thread's saved RAX/RDX registers.
fn complete_joins(process: &mut Process<'_>) {
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    for index in 0..process.threads.len() {
        let Some(target) = &process.threads[index] else {
            continue;
        };
        let (State::Exited(status), Some(waiter)) = (target.state, target.joiner) else {
            continue;
        };
        let tid = target.id;
        let joiner = process
            .threads
            .iter_mut()
            .flatten()
            .find(|thread| thread.id == waiter)
            .expect("join waiter exists");
        assert_eq!(joiner.state, State::Joining(tid));
        joiner.context.frame.rax = 0;
        joiner.context.frame.rdx = status;
        joiner.state = State::Ready;
        process.threads[index] = None;
        #[cfg(feature = "boot-smoke")]
        {
            process.threads_joined += 1;
        }
    }
}

fn create_thread(process: &mut Process<'_>, frame: &TrapFrame) -> Result<u64, i64> {
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
    let index = process
        .threads
        .iter()
        .position(Option::is_none)
        .ok_or(-11)?;
    let mut thread = Thread::new(start.entry, start.stack, start.argument);
    thread.context.frame.fs_base = start.fs;
    thread.context.frame.gs_base = start.gs;
    let tid = thread.id;
    process.threads[index] = Some(thread);
    #[cfg(feature = "boot-smoke")]
    {
        process.threads_created += 1;
    }
    Ok(tid)
}

fn join_thread(process: &mut Process<'_>, frame: &mut TrapFrame) -> Result<Option<State>, i64> {
    if frame.rsi != 0 || frame.rdx != 0 {
        return Err(-22);
    }
    let current = process.thread().id;
    if frame.rdi == current {
        return Err(-35);
    }
    let index = process
        .threads
        .iter()
        .position(|slot| slot.as_ref().is_some_and(|thread| thread.id == frame.rdi))
        .ok_or(-3)?;
    let target = process.threads[index].as_ref().unwrap();
    if target.joiner.is_some() {
        return Err(-16);
    }
    if let State::Exited(status) = target.state {
        frame.rdx = status;
        process.threads[index] = None;
        #[cfg(feature = "boot-smoke")]
        {
            process.threads_joined += 1;
        }
        return Ok(None);
    }
    // A bounded wait-for walk rejects cycles before claiming the target. All
    // joins are within one process; TIDs are never interpreted as slot indexes.
    let mut state = target.state;
    for _ in 0..process.threads.len() {
        let State::Joining(tid) = state else {
            break;
        };
        if tid == current {
            return Err(-35);
        }
        state = process
            .threads
            .iter()
            .flatten()
            .find(|thread| thread.id == tid)
            .expect("join target exists")
            .state;
    }
    process.threads[index].as_mut().unwrap().joiner = Some(current);
    Ok(Some(State::Joining(frame.rdi)))
}

/// Supervise two native applications without granting them scanout or physical
/// input. Lifecycle operations execute here after all syscall frames unwind.
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
    let desktop_image = elf::parse(programs[0]).expect("invalid shell ELF");
    let image = elf::parse(programs[1]).expect("invalid application ELF");
    let mut desktop = create_process(
        kernel,
        frames,
        programs[0],
        &desktop_image,
        direct_map,
        2 + u64::from(smoke),
    )
    .expect("load native shell");
    desktop.policy = CpuPolicy::Shell(ServicePolicy::new(timer::ticks()));
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
    let mut runtime = ShellRuntime {
        kernel,
        bytes: programs[1],
        image,
        direct_map,
        children: [None, None],
        windows: windows::Session::new(smoke),
    };
    #[cfg(feature = "boot-smoke")]
    {
        // Exhaust the supervisor's remaining handle slots after loading the
        // shell, then force a launch to roll back its root and both buffers.
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
        assert_eq!(runtime.launch(frames, &mut desktop, 0), Err(-24));
        assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
        assert!(runtime.children.iter().all(Option::is_none));
        assert!(runtime.windows.applications.iter().all(Option::is_none));
        for handle in &temporary[..count] {
            desktop.handles.close(*handle).unwrap();
        }
        serial::println!("MIRROR_APP_LOAD_ROLLBACK_OK");
    }
    let mut ipc = Ipc::new();
    let mut surface = surface::Session::desktop(display).expect("shell scanout");
    let (framebuffer, region) = surface.placement();
    let mut input = input::Session::new(consumer, framebuffer, region);
    let stack_top = stacks::bsp_rsp0_top();
    // SAFETY: IF=0, no active users; the guarded RSP0 stack stays mapped in
    // every root's shared upper half.
    unsafe { interrupts::set_kernel_stack(stack_top) };
    let mut cursor = 0;
    while !desktop.finished() {
        assert!(ACTIVE_WINDOWS.load(Ordering::SeqCst).is_null());
        runtime.reclaim_finished(frames, &mut ipc);
        let now = timer::ticks();
        wake_threads(&mut desktop, &mut ipc, Some(&mut input), None, now);
        for index in 0..desktop.threads.len() {
            if desktop.threads[index].is_none() {
                continue;
            }
            desktop.current_thread = index;
            match desktop.thread().state {
                State::Launching(kind) => {
                    let result = runtime
                        .launch(frames, &mut desktop, kind)
                        .unwrap_or_else(|error| error as u64);
                    desktop.thread_mut().context.frame.rax = result;
                    desktop.thread_mut().state = State::Ready;
                }
                State::Closing(pid) => {
                    let result = runtime
                        .close(frames, &mut desktop, &mut ipc, pid, Outcome::Terminated)
                        .map_or_else(|error| error as u64, |()| 0);
                    desktop.thread_mut().context.frame.rax = result;
                    desktop.thread_mut().state = State::Ready;
                }
                _ => {}
            }
        }
        for child in runtime.children.iter_mut().flatten() {
            wake_threads(child, &mut ipc, None, Some(&mut runtime.windows), now);
        }
        let mut selected = None;
        for offset in 0..=windows::APP_COUNT {
            let index = (cursor + offset) % (windows::APP_COUNT + 1);
            let ready = if index == 0 {
                service_eligible(&mut desktop, now)
            } else {
                runtime.children[index - 1]
                    .as_mut()
                    .is_some_and(|child| service_eligible(child, now))
            };
            if ready {
                selected = Some(index);
                break;
            }
        }
        if let Some(index) = selected {
            cursor = (index + 1) % (windows::APP_COUNT + 1);
            if index == 0 {
                resume_process(
                    kernel,
                    frames,
                    &mut desktop,
                    &mut ipc,
                    Some(&mut surface),
                    Some(&mut input),
                    Some(&mut runtime.windows),
                );
            } else {
                resume_process(
                    kernel,
                    frames,
                    runtime.children[index - 1].as_mut().unwrap(),
                    &mut ipc,
                    None,
                    None,
                    Some(&mut runtime.windows),
                );
            }
        } else {
            // SAFETY: No published context/resource pointers or locks; IF stayed
            // clear from the queue checks through this atomic enable-and-sleep.
            unsafe { core::arch::asm!("sti", "hlt", "cli", options(nostack)) };
        }
    }
    runtime.reclaim_finished(frames, &mut ipc);
    let pids = runtime
        .windows
        .applications
        .each_ref()
        .map(|application| application.as_ref().map(|application| application.pid));
    for pid in pids.into_iter().flatten() {
        runtime
            .close(frames, &mut desktop, &mut ipc, pid, Outcome::SessionEnded)
            .expect("shell child cancellation");
    }
    release_finished_handles(core::slice::from_mut(&mut desktop), &mut ipc);
    assert!(desktop.handles.is_empty() && ipc.is_drained());
    assert!(
        ACTIVE.load(Ordering::SeqCst).is_null() && ACTIVE_WINDOWS.load(Ordering::SeqCst).is_null()
    );
    assert_eq!(
        surface.totals(),
        (desktop.surface_tiles, desktop.surface_pixels)
    );
    // SAFETY: The shell and children are stopped; all active pointers are clear
    // and no resource retains a reference to the shell's private address space.
    unsafe { desktop.space.destroy(frames) }.expect("shell address-space cleanup");
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        before,
        "shell frame leak"
    );
    let outcome = desktop.outcome.expect("shell ended without outcome");
    if smoke {
        assert_eq!(outcome, Outcome::Exit(0), "shell smoke failed");
        assert!(desktop.desktop_checks, "shell checks marker missing");
        assert!(
            runtime.windows.launches >= 3 && runtime.windows.closes >= 2,
            "application lifecycle checks missing"
        );
        assert!(
            runtime.windows.faults >= 1,
            "application fault containment missing"
        );
        assert!(
            runtime.windows.preemptions > 0,
            "applications were never preempted"
        );
        serial::println!(
            "MIRROR_SHELL_TESTS_OK launches={} closes={} faults={} preemptions={}",
            runtime.windows.launches,
            runtime.windows.closes,
            runtime.windows.faults,
            runtime.windows.preemptions
        );
    }
    serial::println!(
        "MIRROR_SHELL_CLEANUP_OK apps_reclaimed=1 outcome={:?}",
        outcome
    );
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

fn release_finished_handles(processes: &mut [Process<'_>], ipc: &mut Ipc) {
    for process in processes {
        if process.finished() && !process.handles_closed {
            while let Some(capability) = process.handles.take_one() {
                ipc.release(capability);
            }
            process.handles_closed = true;
        }
    }
}

#[cfg(feature = "boot-smoke")]
fn validate_process(process: &Process<'_>, scenario: u64) {
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
    assert!(
        process
            .threads
            .iter()
            .flatten()
            .all(|thread| thread.state == State::Finished)
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
    space: &mut UserAddressSpace<'_>,
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
    space: &mut UserAddressSpace<'_>,
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

fn with_active(frame: &TrapFrame, operation: impl FnOnce(&mut Process<'_>)) {
    assert_eq!(frame.cs & 3, 3, "userspace handler requires CPL3");
    assert!(!arch::interrupts_enabled());
    let pointer = ACTIVE.load(Ordering::SeqCst).cast::<Process<'_>>();
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

fn channel_for(process: &Process<'_>, handle: u64, rights: u64) -> Result<usize, i64> {
    let capability = process.handles.require(handle, rights).map_err(cap_error)?;
    match capability.object {
        Object::Channel(index) => Ok(index),
        _ => Err(-13),
    }
}

fn receive_input(
    process: &mut Process<'_>,
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
fn match_init_oracles(process: &mut Process<'_>, message: &[u8]) {
    process.checks_passed |= message == CHECKS_PASSED;
    process.vm_checks |= message == b"Mirror init: VM checks passed\n";
    process.tls_checks |= message == b"Mirror init: TLS checks passed\n";
    process.clock_checks |= message == b"Mirror init: clock checks passed\n";
    process.clock_unavailable |= message == b"Mirror init: clock unavailable\n";
    process.thread_checks |= message == b"Mirror init: thread checks passed\n";
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
}

/// Shell, desktop and agent console-text oracles and their markers. Only trace
/// kernels match console text (spec 0010-D4).
#[cfg(feature = "test-trace")]
fn service_message(process: &mut Process<'_>, message: &[u8]) {
    if matches!(process.policy, CpuPolicy::Shell(_)) {
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
        return;
    }
    if matches!(process.policy, CpuPolicy::Agent(_)) {
        if message == AGENT_READY && !process.agent_ready {
            process.agent_ready = true;
            serial::println!("MIRROR_AGENT_READY pid={}", process.pid);
        } else if message == AGENT_CHECKS {
            process.agent_checks = true;
        }
        return;
    }
    if !matches!(process.policy, CpuPolicy::Desktop(_)) {
        return;
    }
    if message == DESKTOP_READY && !process.desktop_ready {
        process.desktop_ready = true;
        let info = with_surface(|session| session.info());
        let width = u32::from_le_bytes(info[4..8].try_into().unwrap());
        let height = u32::from_le_bytes(info[8..12].try_into().unwrap());
        serial::println!(
            "MIRROR_DESKTOP_READY pid={} width={} height={}",
            process.pid,
            width,
            height
        );
    } else if message == DESKTOP_FRAME {
        process.desktop_revision += 1;
        serial::println!(
            "MIRROR_DESKTOP_FRAME revision={} tiles={} input_events={} keyboard_irq={} mouse_irq={}",
            process.desktop_revision,
            process.surface_tiles,
            process.input_events,
            crate::keyboard::irq_count(),
            crate::mouse::irq_count()
        );
    } else if message == DESKTOP_CHECKS {
        process.desktop_checks = true;
    }
}

fn require_surface(process: &Process<'_>, handle: u64) -> Result<Option<u64>, i64> {
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
fn note_foreign_surface(process: &mut Process<'_>, handle: u64, error: i64, operation: u8) {
    if error == cap_error(CapError::BadHandle) && handle != 0 && handle == process.foreign_surface {
        process.foreign_surface_denials |= operation;
    }
}

fn surface_info(process: &mut Process<'_>, handle: u64, address: u64, length: u64) -> i64 {
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

fn surface_present(process: &mut Process<'_>, handle: u64, address: u64, length: u64) -> i64 {
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

fn application_pid(process: &Process<'_>, handle: u64) -> Result<u64, i64> {
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

fn application_info(process: &mut Process<'_>, handle: u64, address: u64, length: u64) -> i64 {
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

fn application_event(process: &mut Process<'_>, handle: u64, address: u64, length: u64) -> i64 {
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
        session.get_mut(pid)?.enqueue(event)
    }) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn application_read(process: &mut Process<'_>, handle: u64, address: u64, length: u64) -> i64 {
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
    process: &mut Process<'_>,
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

fn send(process: &mut Process<'_>, ipc: &mut Ipc, handle: u64, address: u64, length: u64) -> i64 {
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
            length as i64
        }
        Err(error) => ipc_error(error),
    }
}

fn receive(
    process: &mut Process<'_>,
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
    }
}

fn dispatch_syscall(process: &mut Process<'_>, frame: &mut TrapFrame) -> SyscallResult {
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
                    suspension = Some(State::Receiving(PendingReceive {
                        handle: frame.rdi,
                        address: frame.rsi,
                        capacity: frame.rdx as usize,
                        deadline: timer::ticks().wrapping_add(RECEIVE_TIMEOUT),
                    }));
                }
            }
            11 => {
                result = match process.handles.close(frame.rdi) {
                    Ok(capability) => {
                        with_ipc(|ipc| ipc.release(capability));
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
                    suspension = Some(State::InputWaiting(PendingInput {
                        handle: frame.rdi,
                        address: frame.rsi,
                        deadline: timer::ticks().wrapping_add(RECEIVE_TIMEOUT),
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
                        .threads
                        .iter()
                        .flatten()
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
            CpuPolicy::Desktop(policy)
            | CpuPolicy::Agent(policy)
            | CpuPolicy::Shell(policy)
            | CpuPolicy::Application(policy) => policy.charge(timer::ticks()),
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
                    .threads
                    .iter()
                    .flatten()
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
    }
}

fn suspend(frame: &mut TrapFrame, state: State) {
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
        process.thread_mut().context.frame = *frame;
        if matches!(
            state,
            State::Sleeping(_)
                | State::Receiving(_)
                | State::InputWaiting(_)
                | State::Joining(_)
                | State::Waiting(_)
        ) {
            process.thread_mut().sleep_charge = process.thread().charged_ticks;
        }
        process.thread_mut().state = state;
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

fn record_fault(process: &mut Process<'_>, frame: &TrapFrame, cr2: u64) {
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
