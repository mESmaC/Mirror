//! Native BSP scheduling with finite probes and a periodically limited desktop.
//! IRQ/syscall handlers return to the scheduler before another process resumes.
//! User buffers are checked and copied through the direct map before use.

use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::{
    arch,
    capability::{self, CapError, Capability, HandleTable, Object},
    elf, framebuffer, input,
    interrupts::{self, TrapFrame},
    ipc::{Ipc, IpcError, MESSAGE_SIZE},
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PageTables, PagingError, UserAddressSpace, UserPermissions},
    ps2, serial, surface, timer,
};

const STACK_BASE: u64 = 0x7fff_0000;
const STACK_TOP: u64 = 0x8000_0000;
const KERNEL_STACK_SIZE: usize = 64 * 1024;
const MAX_WRITE: usize = 256;
const CHECKS_PASSED: &[u8] = b"Mirror init: userspace checks passed\n";
const IPC_CHECKS_PASSED: &[u8] = b"Mirror init: IPC checks passed\n";
const SURFACE_CHECKS_PASSED: &[u8] = b"Mirror init: surface checks passed\n";
const SURFACE_DENIED_CHECKS: &[u8] = b"Mirror init: surface denied\n";
const PROCESS_COUNT: usize = 2;
const QUANTUM_TICKS: u64 = 2;
const USER_TICK_BUDGET: u64 = 32;
const MAX_SLEEP_TICKS: u64 = 1000;
const RECEIVE_TIMEOUT: u64 = 100;
const SERVICE_ALLOWANCE: u64 = 32;
const SERVICE_PERIOD: u64 = 100;
const DESKTOP_READY: &[u8] = b"Mirror desktop: ready\n";
const DESKTOP_FRAME: &[u8] = b"Mirror desktop: frame\n";
const DESKTOP_CHECKS: &[u8] = b"Mirror desktop: checks passed\n";

#[repr(C, align(16))]
struct KernelStack([u8; KERNEL_STACK_SIZE]);
static mut KERNEL_STACK: KernelStack = KernelStack([0; KERNEL_STACK_SIZE]);

// Only the BSP accesses this slot, with IF=0. It is populated immediately before
// entry and cleared after the synchronous return; it never owns the context.
static ACTIVE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_IPC: AtomicPtr<Ipc> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_SURFACE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static ACTIVE_INPUT: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static NEXT_PID: AtomicU64 = AtomicU64::new(1);

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
    BudgetExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ready,
    Running,
    Sleeping(u64),
    Receiving(PendingReceive),
    InputWaiting(PendingInput),
    Finished,
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
}

struct Process<'kernel> {
    pid: u64,
    space: UserAddressSpace<'kernel>,
    frame: TrapFrame,
    state: State,
    outcome: Option<Outcome>,
    checks_passed: bool,
    bad_pointers: usize,
    unknown_calls: usize,
    dispatches: usize,
    yields: usize,
    sleeps: usize,
    sleep_charge: u64,
    charged_ticks: u64,
    quantum_left: u64,
    policy: CpuPolicy,
    throttles: u64,
    preemptions: usize,
    post_checks_preemptions: usize,
    handles: HandleTable,
    boot_handles: [u64; 5],
    handles_closed: bool,
    ipc_checks: bool,
    ipc_sent: usize,
    ipc_received: usize,
    ipc_waits: usize,
    ipc_closed_wakes: usize,
    ipc_timeouts: usize,
    surface_checks: bool,
    surface_denied_checks: bool,
    surface_info_reads: u64,
    surface_tiles: u64,
    surface_pixels: u64,
    foreign_surface: u64,
    foreign_surface_denials: u8,
    desktop_ready: bool,
    desktop_checks: bool,
    desktop_revision: u64,
    input_events: u64,
    input_lost: u64,
    input_waits: u64,
    input_timeouts: u64,
}

pub fn run_init(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    direct_map: u64,
    scenario: u64,
    display: Option<&mut framebuffer::Driver<'_>>,
) {
    assert!(!arch::interrupts_enabled());
    let image = elf::parse(bytes).expect("invalid native init ELF");
    #[cfg(feature = "boot-smoke")]
    {
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
    }
    #[cfg(feature = "boot-smoke")]
    let mut display = display;
    #[cfg(feature = "boot-smoke")]
    let first_display = display.as_deref_mut();
    #[cfg(not(feature = "boot-smoke"))]
    let first_display = display;
    run_pair(
        kernel,
        frames,
        bytes,
        &image,
        direct_map,
        scenario,
        first_display,
    );
    #[cfg(feature = "boot-smoke")]
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
        frame: interrupts::user_frame(image.entry, STACK_TOP - 8, argument),
        state: State::Ready,
        outcome: None,
        checks_passed: false,
        bad_pointers: 0,
        unknown_calls: 0,
        dispatches: 0,
        yields: 0,
        sleeps: 0,
        sleep_charge: 0,
        charged_ticks: 0,
        quantum_left: QUANTUM_TICKS,
        policy: CpuPolicy::Probe,
        throttles: 0,
        preemptions: 0,
        post_checks_preemptions: 0,
        handles: HandleTable::new(),
        boot_handles: [0; 5],
        handles_closed: false,
        ipc_checks: false,
        ipc_sent: 0,
        ipc_received: 0,
        ipc_waits: 0,
        ipc_closed_wakes: 0,
        ipc_timeouts: 0,
        surface_checks: false,
        surface_denied_checks: false,
        surface_info_reads: 0,
        surface_tiles: 0,
        surface_pixels: 0,
        foreign_surface: 0,
        foreign_surface_denials: 0,
        desktop_ready: false,
        desktop_checks: false,
        desktop_revision: 0,
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
    let stack_top = (&raw const KERNEL_STACK) as u64 + KERNEL_STACK_SIZE as u64;
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    // SAFETY: Dedicated static upper-half stack, IF=0, no active user process.
    unsafe { interrupts::set_kernel_stack(stack_top) };
    let mut cursor = 0;
    let mut last = None;
    let mut switches = 0;
    let mut idle_ticks = 0;
    while processes
        .iter()
        .any(|process| process.state != State::Finished)
    {
        assert!(!arch::interrupts_enabled());
        release_finished_handles(&mut processes, &mut ipc);
        let now = timer::ticks();
        for process in &mut processes {
            if let State::Sleeping(deadline) = process.state
                && deadline_reached(now, deadline)
            {
                assert_eq!(
                    process.charged_ticks, process.sleep_charge,
                    "sleep charged user CPU ticks"
                );
                process.state = State::Ready;
            }
            if let State::Receiving(pending) = process.state {
                assert_eq!(
                    process.charged_ticks, process.sleep_charge,
                    "IPC wait charged user CPU ticks"
                );
                let mut result = receive(
                    process,
                    &mut ipc,
                    pending.handle,
                    pending.address,
                    pending.capacity,
                );
                if result == -11 && deadline_reached(now, pending.deadline) {
                    result = -110;
                    process.ipc_timeouts += 1;
                }
                if result != -11 {
                    if result == -32 {
                        process.ipc_closed_wakes += 1;
                    }
                    process.frame.rax = result as u64;
                    process.state = State::Ready;
                }
            }
        }
        let selected = (0..PROCESS_COUNT)
            .map(|offset| (cursor + offset) % PROCESS_COUNT)
            .find(|&index| processes[index].state == State::Ready);
        let Some(index) = selected else {
            assert!(
                processes.iter().any(|process| matches!(
                    process.state,
                    State::Sleeping(_) | State::Receiving(_)
                ))
            );
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
            &mut processes[index],
            &mut ipc,
            surface.as_mut(),
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
    assert_eq!(
        processes[1].surface_tiles, 0,
        "peer painted without authority"
    );
    // Taking the session releases its display borrow when this branch ends.
    if let Some(session) = surface.take() {
        assert!(
            processes[0].surface_checks,
            "owner surface checks did not complete"
        );
        assert!(
            processes[1].surface_denied_checks,
            "peer surface checks did not complete"
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

/// Publish disjoint scheduler-owned objects only while the saved kernel caller
/// is suspended. Every handler unwinds before any of these borrows is used again.
fn resume_process(
    kernel: &PageTables,
    process: &mut Process<'_>,
    ipc: &mut Ipc,
    surface: Option<&mut surface::Session<'_, '_>>,
    input: Option<&mut input::Session<'_>>,
) {
    assert!(!arch::interrupts_enabled());
    assert!(ACTIVE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_IPC.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_SURFACE.load(Ordering::SeqCst).is_null());
    assert!(ACTIVE_INPUT.load(Ordering::SeqCst).is_null());
    assert_eq!(process.state, State::Ready);
    process.state = State::Running;
    process.quantum_left = QUANTUM_TICKS;
    process.dispatches += 1;
    // Save a copy, not a reference into a context that handlers can mutate.
    let context = process.frame;
    let process_pointer = core::ptr::from_mut(process);
    let surface_pointer = surface.map_or(core::ptr::null_mut(), |session| {
        core::ptr::from_mut(session).cast()
    });
    let input_pointer = input.map_or(core::ptr::null_mut(), |session| {
        core::ptr::from_mut(session).cast()
    });
    ACTIVE_SURFACE.store(surface_pointer, Ordering::SeqCst);
    ACTIVE_INPUT.store(input_pointer, Ordering::SeqCst);
    ACTIVE_IPC.store(core::ptr::from_mut(ipc), Ordering::SeqCst);
    ACTIVE.store(process_pointer.cast(), Ordering::SeqCst);
    // SAFETY: Single BSP and IF=0, disjoint exclusively owned session objects,
    // and shared upper mappings cover both kernel stacks. The raw pointers are
    // derived from fresh reborrows; no original reference is accessed until all
    // IRQ/syscall reborrows have ended and the published pointers are revoked.
    unsafe {
        (*process_pointer)
            .space
            .activate()
            .expect("activate user root");
        interrupts::resume_user(&context);
    }
    ACTIVE.store(core::ptr::null_mut(), Ordering::SeqCst);
    ACTIVE_IPC.store(core::ptr::null_mut(), Ordering::SeqCst);
    ACTIVE_SURFACE.store(core::ptr::null_mut(), Ordering::SeqCst);
    ACTIVE_INPUT.store(core::ptr::null_mut(), Ordering::SeqCst);
    // SAFETY: All handlers returned, the user is suspended, and IF remains clear.
    unsafe { kernel.activate() }.expect("restore kernel root");
    assert_ne!(
        process.state,
        State::Running,
        "process returned without transition"
    );
}

/// Run a separate native desktop until exit or a contained user fault. Its CPU
/// allowance renews periodically; finite init probes retain their original cap.
pub fn run_desktop(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    direct_map: u64,
    display: &mut framebuffer::Driver<'_>,
    consumer: &mut ps2::Consumer,
    smoke: bool,
) {
    assert!(!arch::interrupts_enabled());
    let before = frames.stats().expect("recycling required").allocated_frames;
    let outcome = {
        let image = elf::parse(bytes).expect("invalid native desktop ELF");
        let mut process =
            create_process(kernel, frames, bytes, &image, direct_map, u64::from(smoke))
                .expect("load native desktop");
        process.policy = CpuPolicy::Desktop(ServicePolicy::new(timer::ticks()));
        let mut ipc = Ipc::new();
        let mut surface = surface::Session::desktop(display).expect("desktop surface");
        let mut input = input::Session::new(consumer);
        for (slot, object, rights) in [
            (0, Object::Console, capability::WRITE),
            (3, Object::Surface, capability::WRITE),
            (4, Object::Input, capability::RECEIVE),
        ] {
            process.boot_handles[slot] = process
                .handles
                .insert(object, rights)
                .expect("desktop bootstrap capability");
        }
        assert_eq!(process.handles.len(), 3);
        let stack_top = (&raw const KERNEL_STACK) as u64 + KERNEL_STACK_SIZE as u64;
        // SAFETY: IF=0, no active user, and the dedicated static kernel stack
        // remains mapped in the shared upper half of every process root.
        unsafe { interrupts::set_kernel_stack(stack_top) };

        while process.state != State::Finished {
            assert!(!arch::interrupts_enabled());
            let now = timer::ticks();
            match process.state {
                State::Sleeping(deadline) if deadline_reached(now, deadline) => {
                    assert_eq!(process.charged_ticks, process.sleep_charge);
                    process.state = State::Ready;
                }
                State::InputWaiting(pending) => {
                    assert_eq!(
                        process.charged_ticks, process.sleep_charge,
                        "blocked input charged user CPU ticks"
                    );
                    let mut result = receive_input(
                        &mut process,
                        &mut input,
                        pending.handle,
                        pending.address,
                        input::EVENT_SIZE as u64,
                    );
                    if result == -11 && deadline_reached(now, pending.deadline) {
                        result = -110;
                        process.input_timeouts += 1;
                        if process.input_timeouts == 1 {
                            serial::println!("MIRROR_DESKTOP_INPUT_TIMEOUT");
                        }
                    }
                    if result != -11 {
                        process.frame.rax = result as u64;
                        process.state = State::Ready;
                    }
                }
                State::Ready | State::Sleeping(_) => {}
                // This process owns no IPC channel; a receiving state would
                // violate capability checks before any legitimate IPC wait.
                _ => panic!("invalid desktop scheduler state"),
            }
            let CpuPolicy::Desktop(policy) = &mut process.policy else {
                panic!("desktop lost its service policy");
            };
            // Eligibility is independent of Ready/Sleeping/InputWaiting. A
            // yield or a newly completed input wait cannot reset spent credit.
            let eligible = policy.eligible(now);
            if process.state == State::Ready && eligible {
                resume_process(
                    kernel,
                    &mut process,
                    &mut ipc,
                    Some(&mut surface),
                    Some(&mut input),
                );
            } else {
                let before_charge = process.charged_ticks;
                // SAFETY: No published user/session pointers or outstanding
                // borrows. IF stays clear from the queue/deadline check through
                // STI-HLT, so a pending input IRQ cannot create a lost wakeup.
                unsafe { core::arch::asm!("sti", "hlt", "cli", options(nostack)) };
                assert_eq!(
                    process.charged_ticks, before_charge,
                    "desktop idle/throttling charged user CPU ticks"
                );
            }
        }
        assert!(ACTIVE.load(Ordering::SeqCst).is_null());
        assert!(ACTIVE_IPC.load(Ordering::SeqCst).is_null());
        assert!(ACTIVE_INPUT.load(Ordering::SeqCst).is_null());
        assert!(ACTIVE_SURFACE.load(Ordering::SeqCst).is_null());
        assert_eq!(
            surface.totals(),
            (process.surface_tiles, process.surface_pixels)
        );
        release_finished_handles(core::slice::from_mut(&mut process), &mut ipc);
        assert!(process.handles.is_empty() && ipc.is_drained());
        // SAFETY: The process is finished, all handlers unwound, every published
        // pointer is clear, and the kernel root is active with IF=0.
        unsafe { process.space.destroy(frames) }.expect("desktop address-space cleanup");
        assert_eq!(
            frames.stats().unwrap().allocated_frames,
            before,
            "desktop frame leak"
        );
        let outcome = process.outcome.expect("desktop finished without outcome");
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
        }
        // Surface and input sessions leave scope before the caller can start
        // the kernel console. No desktop handle or frame survives this return.
        outcome
    };
    match outcome {
        Outcome::Exit(status) => serial::println!(
            "MIRROR_DESKTOP_CLEANUP_OK reason=exit handles_reclaimed=1 status={}",
            status
        ),
        _ => serial::println!("MIRROR_DESKTOP_CLEANUP_OK reason=fault handles_reclaimed=1"),
    }
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
        if process.state == State::Finished && !process.handles_closed {
            while let Some(capability) = process.handles.take_one() {
                ipc.release(capability);
            }
            process.handles_closed = true;
        }
    }
}

fn validate_process(process: &Process<'_>, scenario: u64) {
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
        #[cfg(feature = "boot-smoke")]
        (6, Outcome::InvalidStack(0x0000_8000_0000_0000)) => {
            serial::println!("MIRROR_USER_RETURN_REJECTED rsp=0x800000000000");
        }
        #[cfg(feature = "boot-smoke")]
        (7, Outcome::BudgetExhausted) => {
            assert_eq!(process.charged_ticks, USER_TICK_BUDGET);
            serial::println!(
                "MIRROR_USER_BUDGET_CONTAINED pid={} ticks={}",
                process.pid,
                process.charged_ticks
            );
        }
        #[cfg(feature = "boot-smoke")]
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
        assert_eq!(process.state, State::Running);
        operation(process);
    }
}

fn with_ipc<T>(operation: impl FnOnce(&mut Ipc) -> T) -> T {
    assert!(!arch::interrupts_enabled());
    let pointer = ACTIVE_IPC.load(Ordering::SeqCst);
    assert!(!pointer.is_null(), "no IPC session");
    // SAFETY: resume_process publishes this disjoint stack-owned object only while
    // tasks can run. Scheduler borrows end before entry; handlers run with IF=0
    // on the sole CPU and do not reenter this helper or retain references.
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
    // ending scheduler borrows, publishes only while resume_user runs, and clears
    // it before accessing the session again. Neither IRQ nor another CPU touches
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
    // reborrow only during user entry and revokes it before scheduler access.
    // Device IRQs mutate queues, never this staged session; IF=0 excludes races.
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
        Object::Console | Object::Surface | Object::Input => Err(-13),
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

fn desktop_message(process: &mut Process<'_>, message: &[u8]) {
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

fn require_surface(process: &mut Process<'_>, handle: u64, operation: u8) -> Result<(), i64> {
    match process.handles.require(handle, capability::WRITE) {
        Ok(capability) if capability.object == Object::Surface => Ok(()),
        Ok(_) => Err(-13),
        Err(error) => {
            if error == CapError::BadHandle && handle != 0 && handle == process.foreign_surface {
                process.foreign_surface_denials |= operation;
            }
            Err(cap_error(error))
        }
    }
}

fn surface_info(process: &mut Process<'_>, handle: u64, address: u64, length: u64) -> i64 {
    if let Err(error) = require_surface(process, handle, 1) {
        return error;
    }
    if length != surface::INFO_SIZE as u64 {
        return -22;
    }
    let bytes = with_surface(|session| session.info());
    // SAFETY: This user is suspended, mappings are immutable on the sole BSP,
    // and the complete fixed-size destination is checked before any write.
    // The owned kernel info bytes cannot alias an owned user physical page.
    if unsafe { process.space.copy_to_user(address, &bytes) }.is_err() {
        return -14;
    }
    process.surface_info_reads += 1;
    surface::INFO_SIZE as i64
}

fn surface_present(process: &mut Process<'_>, handle: u64, address: u64, length: u64) -> i64 {
    if let Err(error) = require_surface(process, handle, 2) {
        return error;
    }
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
    match with_surface(|session| session.present(&packet[..length])) {
        Ok(pixels) => {
            process.surface_tiles += 1;
            process.surface_pixels += pixels as u64;
            length as i64
        }
        Err(_) => -22,
    }
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

pub(crate) fn syscall(frame: &mut TrapFrame) {
    let mut result = -38_i64;
    let mut finished = false;
    let mut suspension = None;
    with_active(frame, |process| {
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
                    process.bad_pointers += 1;
                    result = -14;
                    return;
                }
                // Diagnostic console ABI accepts UTF-8 and escapes controls so
                // an application cannot manufacture kernel result log lines.
                if let Ok(message) = core::str::from_utf8(&buffer[..length]) {
                    serial::println!("user: {}", message.escape_debug());
                    process.checks_passed |= &buffer[..length] == CHECKS_PASSED;
                    process.ipc_checks |= &buffer[..length] == IPC_CHECKS_PASSED;
                    process.surface_checks |= &buffer[..length] == SURFACE_CHECKS_PASSED;
                    process.surface_denied_checks |= &buffer[..length] == SURFACE_DENIED_CHECKS;
                    desktop_message(process, &buffer[..length]);
                    result = length as i64;
                } else {
                    result = -22;
                }
            }
            2 => result = timer::ticks() as i64,
            3 => {
                assert!(process.outcome.is_none());
                process.outcome = Some(Outcome::Exit(frame.rdi));
                process.state = State::Finished;
                finished = true;
            }
            4 => {
                result = 0;
                process.yields += 1;
                suspension = Some(State::Ready);
            }
            5 => {
                if frame.rdi > MAX_SLEEP_TICKS {
                    result = -22;
                } else {
                    result = 0;
                    if frame.rdi == 0 {
                        process.yields += 1;
                        suspension = Some(State::Ready);
                    } else {
                        process.sleeps += 1;
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
                    process.ipc_waits += 1;
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
                match process.handles.require(frame.rdi, capability::RECEIVE) {
                    Ok(capability) if capability.object == Object::Input => {}
                    Ok(_) => {
                        result = -13;
                        return;
                    }
                    Err(error) => {
                        result = cap_error(error);
                        return;
                    }
                }
                result = with_input(|session| {
                    receive_input(process, session, frame.rdi, frame.rsi, frame.rdx)
                });
                if frame.rax == 15 && result == -11 {
                    process.input_waits += 1;
                    suspension = Some(State::InputWaiting(PendingInput {
                        handle: frame.rdi,
                        address: frame.rsi,
                        deadline: timer::ticks().wrapping_add(RECEIVE_TIMEOUT),
                    }));
                }
            }
            _ => process.unknown_calls += 1,
        }
    });
    frame.rax = result as u64;
    if finished {
        interrupts::return_from_user(frame);
    } else if let Some(state) = suspension {
        suspend(frame, state);
    }
}

/// Runs after IRQ0 acknowledgement, only for an interrupted CPL3 context.
/// A tick charges the interrupted user only; kernel and sleeping time are not
/// charged. This bounds delivered user IRQ ticks, not exact CPU or wall time.
pub(crate) fn timer_tick(frame: &mut TrapFrame) {
    let mut expired = false;
    let mut preempt = false;
    with_active(frame, |process| {
        process.charged_ticks += 1;
        process.quantum_left -= 1;
        let throttled = match &mut process.policy {
            CpuPolicy::Probe => false,
            CpuPolicy::Desktop(policy) => policy.charge(timer::ticks()),
        };
        if matches!(process.policy, CpuPolicy::Probe) && process.charged_ticks >= USER_TICK_BUDGET {
            process.outcome = Some(Outcome::BudgetExhausted);
            process.state = State::Finished;
            expired = true;
        } else if throttled || process.quantum_left == 0 {
            if throttled {
                process.throttles += 1;
            }
            process.preemptions += 1;
            if process.checks_passed {
                process.post_checks_preemptions += 1;
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
            State::Ready | State::Sleeping(_) | State::Receiving(_) | State::InputWaiting(_)
        ));
        process.frame = *frame;
        if matches!(
            state,
            State::Sleeping(_) | State::Receiving(_) | State::InputWaiting(_)
        ) {
            process.sleep_charge = process.charged_ticks;
        }
        process.state = state;
    });
    // The current handler must unwind before the scheduler touches other tasks.
    interrupts::return_from_user(frame);
}

pub(crate) fn fault(frame: &mut TrapFrame, cr2: u64) {
    with_active(frame, |process| {
        assert!(process.outcome.is_none());
        process.outcome = Some(Outcome::Fault {
            vector: frame.vector,
            error: frame.error,
            address: cr2,
            rip: frame.rip,
        });
        process.state = State::Finished;
    });
    serial::println!(
        "MIRROR_USER_FAULT vector={} error={:#x} rip={:#x} cr2={:#x}",
        frame.vector,
        frame.error,
        frame.rip,
        cr2
    );
    interrupts::return_from_user(frame);
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
    } else {
        None
    };
    if let Some(outcome) = invalid {
        with_active(frame, |process| {
            assert!(process.outcome.is_none());
            process.outcome = Some(outcome);
            process.state = State::Finished;
        });
        interrupts::return_from_user(frame);
    }
}
