//! Limine-assisted secondary CPU bring-up and bounded IPI rendezvous.
//!
//! APs own guarded kernel stacks, GDT/TSS/IST, local APIC and CPU controls.
//! They never allocate and own no device queue. With the LAPIC tick source
//! each AP runs its own periodic timer, and any CPU may send another the
//! reschedule IPI, vector 242 (spec 0051, point 3); every online CPU then
//! runs user threads (`userspace_mask`), and an AP whose idle loop wakes
//! while a session is open joins it (point 5). Once a slot is ONLINE, its command, timer
//! and reschedule interrupts and its CPL3 syscalls and exceptions take the
//! normal trap dispatch, and any other trap or a panic takes the crash path
//! (`crash::enter`, point 10). A trap or panic before ONLINE, or a smoke
//! probe's armed fault, records the slot FAILED (point 4). Serial output from
//! any CPU goes through its owner-aware lock (spec 0051). Shared mapping
//! changes use bounded synchronous shootdowns; retired frames remain owned
//! until every target ACKs. A user root's changes flush the CPUs in its
//! space's mask with OP_FLUSH_USER (spec 0051, point 8).
//! https://github.com/Limine-Bootloader/limine-protocol/blob/trunk/PROTOCOL.md#mp-multiprocessor-feature

use crate::{
    apic, arch,
    boot::BootInfo,
    cpudata, fpu, hardening, interrupts,
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{self, PageTables},
    serial, stacks, timer,
};
use core::{
    arch::{asm, x86_64::__cpuid},
    cell::UnsafeCell,
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};
use limine::mp::{Cpu, ResponseFlags};
use mirror_acpi::Topology;

const MAX_CPUS: usize = 64;
const STACK_PAGES: usize = 16;
// AP slots follow the BSP region of the stack arena (stacks.rs). Each holds the
// kernel stack, then IST1 (#DF), IST2 (NMI) and IST3 (#MC), every one with its
// own guard page below and above.
const SLOT_STACKS: usize = 1 + stacks::IST_COUNT;
const STACK_STRIDE: u64 =
    stacks::span(STACK_PAGES) + stacks::IST_COUNT as u64 * stacks::span(stacks::IST_PAGES);
const IPI_VECTOR: u8 = 240;
/// The reschedule IPI, which any CPU may send (spec 0051, point 3).
const RESCHEDULE_VECTOR: u8 = 242;
const WAIT_TICKS: u64 = 500;
const ABSENT: u32 = 0;
const STARTING: u32 = 1;
const ONLINE: u32 = 2;
const FAILED: u32 = 3;
const CANCELLED: u32 = 4;
const OP_PROOF: u32 = 1;
const OP_INVALIDATE_RANGE: u32 = 2;
/// The most pages one shootdown command covers; kernel_va batches trims by it.
pub(crate) const MAX_INVALIDATE_PAGES: u64 = 256;
const OP_READ_SHARED: u32 = 3;
#[cfg(feature = "boot-smoke")]
const OP_GUARD_FAULT: u32 = 4;
#[cfg(feature = "boot-smoke")]
const OP_UNMAPPED_FAULT: u32 = 5;
#[cfg(feature = "boot-smoke")]
const OP_READ_CPU_DATA: u32 = 6;
/// Print `PRINT_LINES` check lines at the same time as the BSP and every other
/// AP (spec 0051, step 1). The argument is the mask of printing slots.
#[cfg(feature = "boot-smoke")]
const OP_PRINT: u32 = 7;
#[cfg(feature = "boot-smoke")]
const PRINT_LINES: u64 = 8;
/// Drop every translation through one user root (spec 0051, point 8). The
/// argument is the root: a target whose CR3 holds it reloads CR3, and any
/// other target only acknowledges. The result proves which it did.
const OP_FLUSH_USER: u32 = 8;
/// Load a scratch user root and read one word through it; the argument is
/// the root, the payload the word's address (boot-smoke user flush test).
#[cfg(feature = "boot-smoke")]
const OP_USER_ENTER: u32 = 9;
/// Return to the kernel root (boot-smoke user flush test).
#[cfg(feature = "boot-smoke")]
const OP_USER_LEAVE: u32 = 10;
/// Fixed text that makes each check line long enough for a broken one to show.
#[cfg(feature = "boot-smoke")]
const PRINT_TEXT: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
/// How long a printing CPU waits for the others' first lines: the IPI waits'
/// own bound (WAIT_TICKS).
#[cfg(feature = "boot-smoke")]
const PRINT_BARRIER_NS: u64 = WAIT_TICKS * 10_000_000;
/// The LAPIC ticks each AP must take in the tick check (spec 0051, step 3).
#[cfg(feature = "boot-smoke")]
const TICK_PROOF: u64 = 3;
/// `fail` reason: the AP's periodic LAPIC timer did not start.
const FAIL_TIMER: u64 = 15;
/// Run one crash case's AP half (spec 0051, step 4); the argument is the
/// `CrashTest`. The AP never acknowledges it: it panics, or keeps the
/// scheduler lock until the crash stops it.
#[cfg(feature = "boot-smoke")]
const OP_CRASH_TEST: u32 = 11;
/// `fail` reason: a crash case's AP half outlived its bound.
#[cfg(feature = "boot-smoke")]
const FAIL_CRASH_TEST: u64 = 16;
/// How long a crash case's AP half waits for the BSP's half, and how long
/// SchedStall's holder keeps the scheduler lock before it gives up: far past
/// the lock's 1 s bound and the crash stop's.
#[cfg(feature = "boot-smoke")]
const CRASH_TEST_WAIT_NS: u64 = 10_000_000_000;

#[repr(align(64))]
struct Slot {
    apic_id: AtomicU32,
    state: AtomicU32,
    entered: AtomicBool,
    stack_top: AtomicU64,
    actual_stack: AtomicU64,
    actual_root: AtomicU64,
    // CR4 and EFER as the AP saw them once its descriptors, FPU controls, CPU
    // protections and root were installed, and its own CPUID MCE bit and
    // protection set (hardening::Controls), for the BSP's per-CPU reports.
    actual_cr4: AtomicU64,
    actual_efer: AtomicU64,
    cpuid_mce: AtomicBool,
    controls: AtomicU8,
    // IA32_GS_BASE as the AP read it back after installing its CpuData
    // (0050-D1 (e)), for the BSP's check before MIRROR_CPU_DATA_READY.
    cpu_data: AtomicU64,
    command: AtomicU64,
    operation: AtomicU32,
    argument: AtomicU64,
    payload: AtomicU64,
    acknowledgement: AtomicU64,
    result: AtomicU64,
    completed: AtomicU64,
    fault_vector: AtomicU64,
    fault_error: AtomicU64,
    fault_rip: AtomicU64,
    fault_address: AtomicU64,
    /// Set by this AP just before a smoke probe's deliberate fault, so that
    /// fault keeps the `secondary_fault` record instead of the normal trap
    /// dispatch or, from step 4, the crash path (spec 0051, point 4).
    #[cfg(feature = "boot-smoke")]
    expect_fault: AtomicBool,
}

impl Slot {
    const fn new() -> Self {
        Self {
            apic_id: AtomicU32::new(u32::MAX),
            state: AtomicU32::new(ABSENT),
            entered: AtomicBool::new(false),
            stack_top: AtomicU64::new(0),
            actual_stack: AtomicU64::new(0),
            actual_root: AtomicU64::new(0),
            actual_cr4: AtomicU64::new(0),
            actual_efer: AtomicU64::new(0),
            cpuid_mce: AtomicBool::new(false),
            controls: AtomicU8::new(0),
            cpu_data: AtomicU64::new(0),
            command: AtomicU64::new(0),
            operation: AtomicU32::new(0),
            argument: AtomicU64::new(0),
            payload: AtomicU64::new(0),
            acknowledgement: AtomicU64::new(0),
            result: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            fault_vector: AtomicU64::new(0),
            fault_error: AtomicU64::new(0),
            fault_rip: AtomicU64::new(0),
            fault_address: AtomicU64::new(0),
            #[cfg(feature = "boot-smoke")]
            expect_fault: AtomicBool::new(false),
        }
    }
}

static SLOTS: [Slot; MAX_CPUS] = [const { Slot::new() }; MAX_CPUS];
static ATTEMPTED: AtomicBool = AtomicBool::new(false);
static PUBLISHED: AtomicBool = AtomicBool::new(false);
static BSP_ID: AtomicU32 = AtomicU32::new(u32::MAX);
static COUNT: AtomicUsize = AtomicUsize::new(0);
static ROOT: AtomicU64 = AtomicU64::new(0);
static PAT: AtomicU64 = AtomicU64::new(0);
static COMMAND_BUSY: AtomicBool = AtomicBool::new(false);
/// The CPUs that run user threads, as a mask of slots, fixed at SMP startup
/// (spec 0051, point 3): every online CPU with the LAPIC tick source, else
/// the BSP alone.
static USERSPACE: AtomicU64 = AtomicU64::new(1);
/// The kernel tables' physical-address mask, which bounds a user root.
static PHYSICAL_MASK: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "boot-smoke")]
static TIMEOUT_ARMED: AtomicBool = AtomicBool::new(false);
/// The command whose first acknowledgement the timeout target withholds.
#[cfg(feature = "boot-smoke")]
static TIMEOUT_OPERATION: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "boot-smoke")]
static TIMEOUT_TARGET: AtomicUsize = AtomicUsize::new(MAX_CPUS);
#[cfg(feature = "boot-smoke")]
static TIMEOUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// The slots that have printed their first check line.
#[cfg(feature = "boot-smoke")]
static PRINT_STARTED: AtomicU64 = AtomicU64::new(0);
/// The APs whose next reschedule IPI the round-trip check armed: each takes
/// its request and, if it was set, kicks the BSP back (spec 0051, step 3).
#[cfg(feature = "boot-smoke")]
static RESCHEDULE_ECHO: AtomicU64 = AtomicU64::new(0);
/// Set once slot 1 holds the scheduler lock in ApPanic or SchedStall.
#[cfg(feature = "boot-smoke")]
static CRASH_TEST_HELD: AtomicBool = AtomicBool::new(false);
/// DoublePanic's barrier: the CPUs that wait to panic, the BSP included.
/// Each panics once every one has arrived, so none has a head start.
#[cfg(feature = "boot-smoke")]
static CRASH_TEST_ARRIVED: AtomicUsize = AtomicUsize::new(0);
/// How many CPUs DoublePanic's barrier waits for; the BSP sets it before it
/// sends any AP its half.
#[cfg(feature = "boot-smoke")]
static CRASH_TEST_CPUS: AtomicUsize = AtomicUsize::new(0);

/// A crash case that needs an AP (spec 0051, step 4), as OP_CRASH_TEST's
/// argument.
#[cfg(feature = "boot-smoke")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashTest {
    /// Slot 1 takes the scheduler lock, waits until the BSP spins on it with
    /// IF clear, and panics; its crash must stop the BSP by NMI.
    ApPanic = 1,
    /// Slot 1 takes the scheduler lock and keeps it; the BSP's bounded spin
    /// panics, and its crash must stop the holder by NMI.
    SchedStall = 2,
    /// Every online AP panics at the same moment as the BSP: one crash wins
    /// and every other CPU stops.
    DoublePanic = 3,
}

#[cfg(feature = "boot-smoke")]
impl CrashTest {
    const ALL: [Self; 3] = [Self::ApPanic, Self::SchedStall, Self::DoublePanic];
}

/// The BSP is the sole command producer. Reject reentry instead of spinning:
/// interrupts are briefly enabled during waits and receivers never take this
/// guard. No IRQ-needed lock or allocator guard may span a command operation.
struct CommandGuard(PhantomData<*mut ()>);

impl CommandGuard {
    fn acquire() -> Result<Self, &'static str> {
        if arch::interrupts_enabled() || is_secondary() {
            return Err("commands require BSP with IF=0");
        }
        COMMAND_BUSY
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| "kernel command transport busy")?;
        Ok(Self(PhantomData))
    }
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        COMMAND_BUSY.store(false, Ordering::Release);
    }
}

/// Permission to publish a bounded kernel leaf update, with its exact CPU targets
/// already validated. Dropping an unused ticket is safe before PTE publication;
/// after publication the caller must complete invalidation before reclaiming
/// a frame or exposing the updated mapping to another kernel operation.
pub(crate) struct KernelUpdate {
    _guard: CommandGuard,
    count: usize,
    targets: [bool; MAX_CPUS],
    sequences: [u64; MAX_CPUS],
}
struct Platform(UnsafeCell<Option<Topology>>);
// SAFETY: The BSP writes once before PUBLISHED/goto release, then every CPU
// only reads. The cloned topology is retained for all late or cancelled APs.
unsafe impl Sync for Platform {}
static PLATFORM: Platform = Platform(UnsafeCell::new(None));

#[derive(Clone, Copy)]
struct Identity {
    apic: u32,
    uid: u32,
}

fn validate_identities(
    records: &[Identity],
    cpus: &[mirror_acpi::Cpu],
    bsp: u32,
) -> Result<(), &'static str> {
    if records.is_empty() || records.len() > MAX_CPUS {
        return Err("cpu_count");
    }
    if cpus.iter().filter(|cpu| cpu.enabled).count() != records.len() {
        return Err("cpu_count_mismatch");
    }
    if records.iter().filter(|cpu| cpu.apic == bsp).count() != 1 {
        return Err("missing_bsp");
    }
    for (index, record) in records.iter().enumerate() {
        if record.apic >= 255 {
            return Err("unsupported_apic_id");
        }
        if records[..index]
            .iter()
            .any(|prior| prior.apic == record.apic || prior.uid == record.uid)
        {
            return Err("duplicate_cpu");
        }
        if !cpus
            .iter()
            .any(|cpu| cpu.enabled && cpu.apic_id == record.apic && cpu.uid == record.uid)
        {
            return Err("cpu_identity_mismatch");
        }
    }
    Ok(())
}

fn slot_base(slot: usize) -> u64 {
    stacks::AP_BASE + (slot - 1) as u64 * STACK_STRIDE
}
fn stack_bottom(slot: usize) -> u64 {
    slot_base(slot) + PAGE_SIZE
}
fn stack_top(slot: usize) -> u64 {
    stack_bottom(slot) + STACK_PAGES as u64 * PAGE_SIZE
}
fn ist_bottom(slot: usize, ist: usize) -> u64 {
    slot_base(slot)
        + stacks::span(STACK_PAGES)
        + ist as u64 * stacks::span(stacks::IST_PAGES)
        + PAGE_SIZE
}
/// The tops of a slot's IST1-3 stacks, in TSS order.
fn ist_tops(slot: usize) -> [u64; stacks::IST_COUNT] {
    core::array::from_fn(|ist| ist_bottom(slot, ist) + stacks::IST_PAGES as u64 * PAGE_SIZE)
}
/// A slot's stacks in map order: the kernel stack, then IST1-3.
fn slot_stacks(slot: usize) -> [(u64, usize); SLOT_STACKS] {
    core::array::from_fn(|index| match index {
        0 => (stack_bottom(slot), STACK_PAGES),
        ist => (ist_bottom(slot, ist - 1), stacks::IST_PAGES),
    })
}

/// Only BSP boot calls this, after all mutating kernel paging self-tests and
/// before userspace. All AP resources remain reserved for the rest of boot.
pub fn init(
    boot: &BootInfo,
    topology: Option<&Topology>,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
) {
    assert!(!arch::interrupts_enabled());
    assert!(!ATTEMPTED.swap(true, Ordering::SeqCst));
    ROOT.store(tables.root_physical_address(), Ordering::Relaxed);
    PHYSICAL_MASK.store(tables.physical_mask(), Ordering::Relaxed);
    #[cfg(feature = "boot-smoke")]
    self_test();
    if !apic::is_enabled() {
        serial::println!("MIRROR_SMP_SKIPPED reason=legacy_pic online=1");
        #[cfg(feature = "boot-smoke")]
        startup_self_test();
        return;
    }
    let result = prepare(boot, topology, tables, frames);
    let (cpus, count) = match result {
        Ok(value) => value,
        Err(reason) => {
            serial::println!("MIRROR_SMP_SKIPPED reason={} online=1", reason);
            #[cfg(feature = "boot-smoke")]
            startup_self_test();
            return;
        }
    };
    for (index, cpu) in cpus[..count].iter().enumerate().skip(1) {
        let cpu = cpu.expect("validated secondary processor");
        SLOTS[index].state.store(STARTING, Ordering::Release);
        cpu.extra.store(index as u64, Ordering::Relaxed);
        cpu.goto_address.write(secondary_entry);
    }
    // One deadline covers the entire batch. A timeout never frees descriptors,
    // stacks, mailboxes, topology or the root that a late AP may still observe.
    wait_until(|| (1..count).all(|index| SLOTS[index].state.load(Ordering::Acquire) != STARTING));
    let mut online = 1;
    let mut online_mask = 1;
    for (index, slot) in SLOTS[..count].iter().enumerate().skip(1) {
        let _ =
            slot.state
                .compare_exchange(STARTING, CANCELLED, Ordering::AcqRel, Ordering::Acquire);
        if slot.state.load(Ordering::Acquire) == ONLINE {
            online += 1;
            online_mask |= 1 << index;
            assert_eq!(
                slot.actual_root.load(Ordering::Relaxed),
                ROOT.load(Ordering::Relaxed)
            );
            let stack = slot.actual_stack.load(Ordering::Relaxed);
            assert!((stack_bottom(index)..stack_top(index)).contains(&stack));
            serial::println!(
                "MIRROR_SMP_CPU slot={} apic={} stack={:#x} cr3={:#x}",
                index,
                slot.apic_id.load(Ordering::Relaxed),
                slot.stack_top.load(Ordering::Relaxed),
                slot.actual_root.load(Ordering::Relaxed)
            );
        } else {
            report_failure(index);
        }
    }
    // Every online CPU runs user threads only when each one ticks; with the
    // PIT the APs have no timer, so userspace stays on the BSP (point 3).
    if timer::local_timer() {
        USERSPACE.store(online_mask, Ordering::Release);
    }
    serial::println!(
        "MIRROR_SMP_READY discovered={} online={} bsp={} aps={} userspace_cpus={}",
        count,
        online,
        BSP_ID.load(Ordering::Relaxed),
        online - 1,
        userspace_cpus()
    );
    #[cfg(feature = "boot-smoke")]
    assert_eq!(online, count, "not every secondary reached kernel idle");
    rendezvous("boot", 8);
    #[cfg(feature = "boot-smoke")]
    startup_self_test();
}

/// The CPUs that run user threads, as a mask of slots (spec 0051, point 3):
/// the CPUs online at startup when the LAPIC is the tick source, else the
/// BSP alone, also when SMP startup was skipped. An AP that has failed since
/// drops out, so no session kicks or waits for it.
pub(crate) fn userspace_mask() -> u64 {
    let online = SLOTS[..configured_count()]
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, slot)| slot.state.load(Ordering::Acquire) == ONLINE)
        .fold(1, |mask, (index, _)| mask | 1 << index);
    USERSPACE.load(Ordering::Acquire) & online
}

/// How many CPUs run user threads (`userspace_mask`).
pub(crate) fn userspace_cpus() -> usize {
    userspace_mask().count_ones() as usize
}

/// The kernel stack this CPU's TSS.RSP0 names while no user thread runs on
/// it (0050-D2): the BSP's guarded RSP0 stack, or an AP's own kernel stack,
/// as its descriptors were installed. No CPL3 code runs while it is
/// installed, so hardware never pushes onto it.
pub(crate) fn rsp0_placeholder() -> u64 {
    match cpudata::current_cpu() {
        0 => stacks::bsp_rsp0_top(),
        slot => stack_top(slot),
    }
}

/// After startup, report every online CPU's stacks, the BSP first, as
/// `MIRROR_CPU_STACKS`: how many of its TSS IST1-3 fields point at the tops of
/// its own guarded IST stacks, and its CR4.MCE and CPUID MCE bits. The BSP
/// reads its own state; an AP's comes from its slot and its TSS.
pub fn report_stacks() {
    assert!(!arch::interrupts_enabled() && !is_secondary());
    let report = |cpu: usize, expected: [u64; stacks::IST_COUNT], cr4: u64, cpuid_mce: bool| {
        // SAFETY: The BSP reads its own TSS. An AP's slot was observed online
        // with Acquire ordering below, after its init_secondary succeeded,
        // and an AP never writes its TSS again.
        let installed = unsafe { interrupts::installed_ists(cpu) };
        let ist = installed
            .iter()
            .zip(expected)
            .filter(|&(&actual, top)| actual == top)
            .count();
        serial::println!(
            "MIRROR_CPU_STACKS cpu={} ist={} mce={} cpuid_mce={}",
            cpu,
            ist,
            u8::from(cr4 & interrupts::CR4_MCE != 0),
            u8::from(cpuid_mce)
        );
    };
    let cr4: u64;
    // SAFETY: CPU-local register inspection at CPL0, no mutation.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    let bsp_tops = stacks::bsp_ist_stacks().map(|stack| stack.end);
    report(0, bsp_tops, cr4, interrupts::cpuid_mce());
    // COUNT stays 0 when SMP was skipped, so only the BSP reports.
    let count = COUNT.load(Ordering::Relaxed);
    for (index, slot) in SLOTS.iter().enumerate().take(count).skip(1) {
        if slot.state.load(Ordering::Acquire) == ONLINE {
            report(
                index,
                ist_tops(index),
                slot.actual_cr4.load(Ordering::Relaxed),
                slot.cpuid_mce.load(Ordering::Relaxed),
            );
        }
    }
}

/// After startup, report every online CPU's protections, the BSP first, as
/// `MIRROR_CPU_HARDENING`. The BSP reads its own CR4 and EFER; an AP's come
/// from its slot, with the CPUID set it recorded.
pub fn report_hardening() {
    assert!(!arch::interrupts_enabled() && !is_secondary());
    let bsp = hardening::bsp_controls().expect("BSP CPU controls");
    hardening::report(0, hardening::read_cr4(), hardening::read_efer(), bsp);
    // COUNT stays 0 when SMP was skipped, so only the BSP reports.
    let count = COUNT.load(Ordering::Relaxed);
    for (index, slot) in SLOTS.iter().enumerate().take(count).skip(1) {
        if slot.state.load(Ordering::Acquire) == ONLINE {
            hardening::report(
                index,
                slot.actual_cr4.load(Ordering::Relaxed),
                slot.actual_efer.load(Ordering::Relaxed),
                hardening::Controls::from_bits(slot.controls.load(Ordering::Relaxed)),
            );
        }
    }
}

fn prepare(
    boot: &BootInfo,
    topology: Option<&Topology>,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
) -> Result<([Option<&'static Cpu>; MAX_CPUS], usize), &'static str> {
    let topology = topology.ok_or("missing_topology")?;
    let response = boot.processors.ok_or("missing_mp_response")?;
    if response.flags().contains(ResponseFlags::X2APIC) {
        return Err("x2apic_response");
    }
    let provided = response.cpus();
    if provided.is_empty() || provided.len() > MAX_CPUS {
        return Err("cpu_count");
    }
    let bsp = response.bsp_lapic_id();
    if initial_apic_id() != bsp || apic::local_apic_id()? != bsp {
        return Err("bsp_identity_mismatch");
    }
    let mut identities = [Identity { apic: 0, uid: 0 }; MAX_CPUS];
    for (index, cpu) in provided.iter().enumerate() {
        identities[index] = Identity {
            apic: cpu.lapic_id,
            uid: cpu.id,
        };
    }
    validate_identities(&identities[..provided.len()], topology.cpus(), bsp)?;
    let mut ordered = [None; MAX_CPUS];
    let mut next = 1;
    for &cpu in provided {
        if cpu.lapic_id == bsp {
            ordered[0] = Some(cpu);
        } else {
            ordered[next] = Some(cpu);
            next += 1;
        }
    }
    allocate_stacks(next, tables, frames)?;
    let root = if next > 1 {
        tables
            .share_with_secondaries()
            .map_err(|_| "root_publication")?
    } else {
        tables.root_physical_address()
    };
    let low: u32;
    let high: u32;
    // SAFETY: APIC MMIO setup already verified PAT support on the BSP.
    unsafe {
        asm!("rdmsr", in("ecx") 0x277_u32, out("eax") low, out("edx") high,
        options(nomem, nostack, preserves_flags));
    }
    PAT.store((u64::from(high) << 32) | u64::from(low), Ordering::Relaxed);
    ROOT.store(root, Ordering::Relaxed);
    BSP_ID.store(bsp, Ordering::Relaxed);
    COUNT.store(next, Ordering::Relaxed);
    for (index, cpu) in ordered[..next].iter().enumerate() {
        SLOTS[index]
            .apic_id
            .store(cpu.expect("validated CPU list").lapic_id, Ordering::Relaxed);
        if index != 0 {
            SLOTS[index]
                .stack_top
                .store(stack_top(index), Ordering::Relaxed);
        }
    }
    SLOTS[0].state.store(ONLINE, Ordering::Relaxed);
    // SAFETY: No secondary has been released; this is the only writer and no
    // reference to the retained topology has yet been created.
    unsafe {
        *PLATFORM.0.get() = Some(topology.clone());
    }
    PUBLISHED.store(true, Ordering::Release);
    Ok((ordered, next))
}

fn allocate_stacks(
    count: usize,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
) -> Result<(), &'static str> {
    // SAFETY: The BSP runs with IF=0 on the active root and owns the
    // allocator. No AP was released and the root is not yet published, so
    // kernel mappings are unsealed. Only its slot uses each slot's arena
    // range, and slot_stacks is a pure function of the slot. Each stack owns a
    // separate span of its slot, guards included, so no two stacks of any
    // slots share a page; the self-test checks the layout. On failure map_sets
    // unmaps every slot it mapped before any AP could run on it.
    unsafe { stacks::map_sets(tables, frames, 1..count, slot_stacks) }
}

unsafe extern "C" fn secondary_entry(cpu: &Cpu) -> ! {
    arch::disable_interrupts();
    if !PUBLISHED.load(Ordering::Acquire) {
        arch::halt();
    }
    let index = cpu.extra.load(Ordering::Acquire) as usize;
    if index == 0 || index >= COUNT.load(Ordering::Relaxed) {
        arch::halt();
    }
    let slot = &SLOTS[index];
    if slot.entered.swap(true, Ordering::AcqRel)
        || slot.apic_id.load(Ordering::Relaxed) != cpu.lapic_id
        || initial_apic_id() != cpu.lapic_id
    {
        fail(index, 1);
    }
    if slot.state.load(Ordering::Acquire) != STARTING {
        arch::halt();
    }
    // Every CPU must report the BSP's protections (0016-D1). Check CPUID before
    // the IDT load: once the BSP has SMAP, every entry stub executes CLAC,
    // which raises #UD on a CPU whose CPUID lacks SMAP, so an exception on such
    // an AP would recurse through the #UD gate on Limine's stack.
    if !hardening::matches_bsp() {
        fail(index, 12);
    }
    let top = slot.stack_top.load(Ordering::Relaxed);
    // SAFETY: Unique validated AP slot whose CPUID matches the BSP's, so the
    // entry stubs' CLAC is defined here. Limine's reserved stack and the kernel
    // image, with this CPU's descriptors and the shared IDT, are mapped now and
    // in the sealed root. The RSP0 and IST1-3 tops sit above this slot's
    // guarded stacks, which the BSP mapped before publication, but only in the
    // sealed root. So from init_secondary's IDT load until activate_secondary
    // below, pushing a frame onto an IST stack faults. Only a #DF then
    // triple-faults this CPU: its IST1 push raises #PF, and #DF plus #PF shuts
    // the CPU down. NMI and #MC are benign, so the #PF from an IST2 or IST3
    // push is delivered serially through the #PF gate, which has no IST, on
    // Limine's stack, and secondary_fault records this slot FAILED with vector
    // 14 and cr2 inside that IST stack. LINT1 stays masked until
    // apic::init_secondary. No CPL3 entry is allowed.
    if !unsafe { interrupts::init_secondary(index, top, ist_tops(index)) } {
        fail(index, 2);
    }
    if fpu::init_secondary().is_err() {
        fail(index, 3);
    }
    // Still on Limine's stack and root, whose kernel mappings are
    // supervisor-only, so SMEP, SMAP and UMIP change nothing here, and neither
    // does clearing AC. Recheck that the set configure_current enabled matches
    // the BSP's (0016-D1).
    let Ok(controls) = hardening::configure_current() else {
        fail(index, 11);
    };
    if hardening::bsp_controls() != Some(controls) {
        fail(index, 12);
    }
    slot.controls.store(controls.bits(), Ordering::Relaxed);
    // SAFETY: The BSP published the retained root and matching PAT. Descriptors
    // are installed, IF=0, and Limine's stack survives the copied bootstrap map.
    if unsafe {
        paging::activate_secondary(ROOT.load(Ordering::Relaxed), PAT.load(Ordering::Relaxed))
    }
    .is_err()
    {
        fail(index, 4);
    }
    // SAFETY: The BSP mapped this private 64 KiB stack with adjacent guards.
    // The owned root is active, no references into the boot stack escape, and
    // the noreturn call follows SysV alignment without returning to that stack.
    unsafe {
        asm!("mov rsp, rsi", "xor ebp, ebp", "call {entry}", "ud2",
            entry = sym secondary_main, in("rdi") index, in("rsi") top, options(noreturn));
    }
}

extern "C" fn secondary_main(index: usize) -> ! {
    let slot = &SLOTS[index];
    let stack: u64;
    let root: u64;
    let cr4: u64;
    // SAFETY: CPU-local register inspection at CPL0, no mutation.
    unsafe {
        asm!("mov {}, rsp", out(reg) stack, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr3", out(reg) root, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
    }
    let efer = hardening::read_efer();
    if !(stack_bottom(index)..stack_top(index)).contains(&stack)
        || root != ROOT.load(Ordering::Relaxed)
    {
        fail(index, 5);
    }
    // SAFETY: PUBLISHED Acquire in secondary_entry observes the immutable BSP
    // topology publication, whose storage remains live even after cancellation.
    let topology = unsafe { (*PLATFORM.0.get()).as_ref() };
    let Some(topology) = topology else {
        fail(index, 6);
    };
    if apic::init_secondary(slot.apic_id.load(Ordering::Relaxed), topology).is_err() {
        fail(index, 7);
    }
    // With the LAPIC tick source this CPU ticks too, with the BSP's reload
    // (spec 0051, point 3). IF stays clear until the idle loop below, so its
    // first tick arrives once the slot is ONLINE, when traps dispatch.
    if timer::start_secondary().is_err() {
        fail(index, FAIL_TIMER);
    }
    slot.actual_stack.store(stack, Ordering::Relaxed);
    slot.actual_root.store(root, Ordering::Relaxed);
    slot.actual_cr4.store(cr4, Ordering::Relaxed);
    slot.actual_efer.store(efer, Ordering::Relaxed);
    // init_secondary installed this CPU's CpuData and checked it; the BSP
    // checks this readback too before it reports per-CPU data.
    slot.cpu_data.store(cpudata::gs_base(), Ordering::Relaxed);
    slot.cpuid_mce
        .store(interrupts::cpuid_mce(), Ordering::Relaxed);
    // Cancellation is terminal: a late AP may never resurrect an offline slot.
    if slot
        .state
        .compare_exchange(STARTING, ONLINE, Ordering::Release, Ordering::Acquire)
        .is_err()
    {
        // IF stays clear, so a running timer could not interrupt the halt;
        // stop it anyway, so a cancelled CPU keeps nothing running.
        let _ = timer::stop_secondary();
        arch::halt();
    }
    loop {
        if slot.state.load(Ordering::Acquire) != ONLINE {
            arch::halt();
        }
        // SAFETY: Private descriptors, validated local APIC and shared mappings
        // are ready. Only command, timer, reschedule and spurious interrupts
        // return on an idle AP. CLI closes each idle iteration so no Rust
        // handler/state update nests.
        unsafe {
            asm!("sti", "hlt", "cli", options(nostack));
        }
        // A wake while a session is open joins it, until its barrier sends
        // this CPU back here (spec 0051, point 5). Outside one this returns
        // at once.
        crate::userspace::join_session();
    }
}

fn fail(index: usize, reason: u64) -> ! {
    SLOTS[index].fault_vector.store(u64::MAX, Ordering::Relaxed);
    SLOTS[index].fault_error.store(reason, Ordering::Relaxed);
    SLOTS[index].state.store(FAILED, Ordering::Release);
    arch::halt()
}

/// This CPU's initial APIC ID from CPUID, which needs neither GS nor the
/// local APIC mapping: the crash owner's key (spec 0051, point 10).
pub(crate) fn initial_apic_id() -> u32 {
    // xAPIC discovery rejects IDs >=255, where this 8-bit field would be ambiguous.
    __cpuid(1).ebx >> 24
}

/// Whether this CPU is an AP. Once this CPU installed its CpuData the
/// answer is a read of IA32_GS_BASE (0050-D1); before that, the CPUID check.
pub fn is_secondary() -> bool {
    match cpudata::try_current_slot() {
        Some(slot) => slot != 0,
        None => {
            PUBLISHED.load(Ordering::Acquire) && initial_apic_id() != BSP_ID.load(Ordering::Relaxed)
        }
    }
}

/// This CPU's slot: its installed CpuData's, or before install the CPUID
/// scan, which finds a slot only once SMP startup has published them.
fn current_slot() -> Option<usize> {
    cpudata::try_current_slot().or_else(cpuid_slot)
}

/// This CPU's slot by its CPUID APIC ID, without GS. None until SMP startup
/// has published the slots; until then only the BSP runs.
pub(crate) fn cpuid_slot() -> Option<usize> {
    if !PUBLISHED.load(Ordering::Acquire) {
        return None;
    }
    let id = initial_apic_id();
    SLOTS[..COUNT.load(Ordering::Relaxed)]
        .iter()
        .position(|slot| slot.apic_id.load(Ordering::Relaxed) == id)
}

/// Whether this AP's traps may take the normal dispatch (spec 0051, point 4):
/// only once its slot is ONLINE, and not while a smoke probe has armed its
/// deliberate fault. Otherwise `secondary_fault` records the trap.
pub(crate) fn ap_traps_dispatch() -> bool {
    !ap_records_faults()
}

/// Whether a fatal trap or a panic on this AP keeps the `secondary_fault`
/// record instead of the crash path (spec 0051, points 4 and 10): before its
/// slot is ONLINE, or while a smoke probe has armed its deliberate fault.
/// It reads only atomics, so the panic handler may ask it.
pub(crate) fn ap_records_faults() -> bool {
    let Some(index) = current_slot() else {
        return true;
    };
    let slot = &SLOTS[index];
    #[cfg(feature = "boot-smoke")]
    if slot.expect_fault.load(Ordering::Relaxed) {
        return true;
    }
    index == 0 || slot.state.load(Ordering::Acquire) != ONLINE
}

/// This CPU's slot for the crash path, without asserting: its CpuData's,
/// else its CPUID APIC ID's, else the BSP's before SMP startup published the
/// slots, when only the BSP runs. `None` only for a CPU outside the slots.
pub(crate) fn crash_slot() -> Option<usize> {
    current_slot().or_else(|| (!PUBLISHED.load(Ordering::Acquire)).then_some(0))
}

/// The ONLINE slots, the BSP's included, with their APIC IDs: the targets of
/// the crash stop (spec 0051, point 10). Empty before SMP startup published
/// the slots or when it was skipped. It reads only atomics.
pub(crate) fn online_cpus() -> impl Iterator<Item = (usize, u32)> {
    SLOTS[..configured_count()]
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.state.load(Ordering::Acquire) == ONLINE)
        .map(|(index, slot)| (index, slot.apic_id.load(Ordering::Relaxed)))
}

/// AP slot `slot`'s stacks as `bottom..top` ranges, guards excluded, for the
/// crash backtrace (spec 0051, point 11); `None` for the BSP or a slot out
/// of range. Only an online AP, whose stacks are mapped, walks its own.
pub(crate) fn slot_stack_ranges(slot: usize) -> Option<[core::ops::Range<u64>; SLOT_STACKS]> {
    (1..MAX_CPUS)
        .contains(&slot)
        .then(|| slot_stacks(slot).map(|(bottom, pages)| bottom..bottom + pages as u64 * PAGE_SIZE))
}

/// Record a trap or panic on an AP that is not ONLINE, or a smoke probe's
/// armed fault, in its slot and halt (spec 0051, point 4). This path never
/// touches serial, scheduler or crash state.
pub fn secondary_fault(vector: u64, error: u64, rip: u64, address: u64) -> ! {
    arch::disable_interrupts();
    if let Some(index) = current_slot() {
        let slot = &SLOTS[index];
        slot.fault_vector.store(vector, Ordering::Relaxed);
        slot.fault_error.store(error, Ordering::Relaxed);
        slot.fault_rip.store(rip, Ordering::Relaxed);
        slot.fault_address.store(address, Ordering::Relaxed);
        slot.state.store(FAILED, Ordering::Release);
    }
    arch::halt()
}

fn proof(payload: u64, id: u32, sequence: u64) -> u64 {
    let mut value = payload ^ (u64::from(id) << 32) ^ sequence;
    for round in 0..64u64 {
        value = value.rotate_left(7).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ round;
    }
    value
}

#[cfg(feature = "boot-smoke")]
fn shared_page_valid(address: u64) -> bool {
    address.is_multiple_of(PAGE_SIZE)
        && (paging::SHARED_BASE..paging::SHARED_BASE + paging::SHARED_PAGES as u64 * PAGE_SIZE)
            .contains(&address)
}

fn invalidation_range_valid(address: u64, pages: u64) -> bool {
    if !address.is_multiple_of(PAGE_SIZE) || !(1..=MAX_INVALIDATE_PAGES).contains(&pages) {
        return false;
    }
    let Some(end) = pages
        .checked_mul(PAGE_SIZE)
        .and_then(|bytes| address.checked_add(bytes))
    else {
        return false;
    };
    // A batch must fit wholly inside one reserved window, never across a guard
    // or through the gap between windows. Their addresses are canonical.
    [
        (paging::SHARED_BASE, paging::SHARED_PAGES as u64 * PAGE_SIZE),
        (crate::heap::HEAP_START, crate::heap::HEAP_MAX_SIZE as u64),
        (crate::kernel_va::BASE, crate::kernel_va::SIZE as u64),
    ]
    .into_iter()
    .any(|(start, bytes)| {
        start
            .checked_add(bytes)
            .is_some_and(|limit| address >= start && end <= limit)
    })
}

/// # Safety
/// The entire range must pass invalidation_range_valid. PTE publication and
/// ownership follow KernelUpdate's contract; no page is accessed by this loop.
unsafe fn invalidate_local_range(address: u64, pages: u64) {
    for page in 0..pages {
        // SAFETY: The validated range is canonical and its checked end proves
        // that every page calculation fits. INVLPG also removes global entries.
        unsafe { arch::invalidate_tlb_page(address + page * PAGE_SIZE) };
    }
}

/// A user root a flush may name: nonzero, page-aligned, within the physical
/// mask, and not the kernel root.
fn user_root_valid(root: u64) -> bool {
    root != 0
        && root.is_multiple_of(PAGE_SIZE)
        && root & !PHYSICAL_MASK.load(Ordering::Relaxed) == 0
        && root != ROOT.load(Ordering::Relaxed)
}

/// A lower-half word the user flush test may read: aligned, past the null
/// page and below the end of user space.
#[cfg(feature = "boot-smoke")]
fn user_word_valid(address: u64) -> bool {
    address.is_multiple_of(8) && (PAGE_SIZE..(1 << 47) - 8).contains(&address)
}

fn operation_valid(operation: u32, argument: u64, payload: u64, index: usize) -> bool {
    match operation {
        OP_PROOF => argument == 0,
        OP_INVALIDATE_RANGE => invalidation_range_valid(argument, payload),
        OP_READ_SHARED => invalidation_range_valid(argument, 1),
        OP_FLUSH_USER => index != 0 && user_root_valid(argument) && payload == 0,
        #[cfg(feature = "boot-smoke")]
        OP_USER_ENTER => index != 0 && user_root_valid(argument) && user_word_valid(payload),
        #[cfg(feature = "boot-smoke")]
        OP_USER_LEAVE => index != 0 && argument == 0 && payload == 0,
        #[cfg(feature = "boot-smoke")]
        OP_GUARD_FAULT => index != 0 && argument == 0,
        #[cfg(feature = "boot-smoke")]
        OP_UNMAPPED_FAULT => index != 0 && shared_page_valid(argument),
        #[cfg(feature = "boot-smoke")]
        OP_READ_CPU_DATA => argument == 0 && payload == 0,
        #[cfg(feature = "boot-smoke")]
        OP_PRINT => {
            index != 0
                && index < MAX_CPUS
                && argument & 1 != 0
                && argument & (1 << index) != 0
                && payload == 0
        }
        #[cfg(feature = "boot-smoke")]
        OP_CRASH_TEST => {
            index != 0 && CrashTest::ALL.iter().any(|&test| argument == test as u64) && payload == 0
        }
        _ => {
            let _ = index;
            false
        }
    }
}

fn next_sequence(command: u64, acknowledgement: u64, completed: u64) -> Result<u64, &'static str> {
    if command != acknowledgement {
        return Err("kernel command still outstanding");
    }
    if completed != acknowledgement {
        return Err("kernel command completion mismatch");
    }
    command
        .checked_add(1)
        .ok_or("kernel command sequence exhausted")
}

fn ready_sequence(slot: &Slot) -> Result<u64, &'static str> {
    if slot.state.load(Ordering::Acquire) != ONLINE {
        return Err("kernel command target not online");
    }
    let acknowledgement = slot.acknowledgement.load(Ordering::Acquire);
    next_sequence(
        slot.command.load(Ordering::Relaxed),
        acknowledgement,
        slot.completed.load(Ordering::Relaxed),
    )
}

fn publish_command(
    index: usize,
    sequence: u64,
    operation: u32,
    argument: u64,
    payload: u64,
) -> Result<(), &'static str> {
    if !operation_valid(operation, argument, payload, index) {
        return Err("invalid kernel command");
    }
    let slot = &SLOTS[index];
    if ready_sequence(slot)? != sequence {
        return Err("kernel command sequence changed");
    }
    slot.operation.store(operation, Ordering::Relaxed);
    slot.argument.store(argument, Ordering::Relaxed);
    slot.payload.store(payload, Ordering::Relaxed);
    // This release also publishes preceding PTE writes. send_ipi adds the
    // normal-memory/UC-MMIO fence before notifying this specific processor.
    slot.command.store(sequence, Ordering::Release);
    apic::send_ipi(slot.apic_id.load(Ordering::Relaxed), IPI_VECTOR)
}

fn target_complete(state: u32, acknowledgement: u64, expected: u64) -> bool {
    state == ONLINE && acknowledgement == expected
}

/// Unlike diagnostic rendezvous, offline transitions never satisfy a target.
/// A failed target ends the wait only to produce a fail-stop assertion below.
fn wait_exact(
    count: usize,
    targets: &[bool; MAX_CPUS],
    sequences: &[u64; MAX_CPUS],
    timeout_message: &'static str,
) {
    let complete = wait_until(|| {
        SLOTS[..count]
            .iter()
            .enumerate()
            .any(|(index, slot)| targets[index] && slot.state.load(Ordering::Acquire) != ONLINE)
            || SLOTS[..count].iter().enumerate().all(|(index, slot)| {
                !targets[index]
                    || target_complete(
                        slot.state.load(Ordering::Acquire),
                        slot.acknowledgement.load(Ordering::Acquire),
                        sequences[index],
                    )
            })
    });
    assert!(complete, "{}", timeout_message);
    for (index, slot) in SLOTS[..count].iter().enumerate() {
        if targets[index] {
            assert_eq!(
                slot.state.load(Ordering::Acquire),
                ONLINE,
                "kernel command target failed before acknowledgement"
            );
            assert_eq!(
                slot.acknowledgement.load(Ordering::Acquire),
                sequences[index],
                "kernel command acknowledgement mismatch"
            );
            assert_eq!(
                slot.completed.load(Ordering::Relaxed),
                sequences[index],
                "kernel command completion mismatch"
            );
        }
    }
}

/// Preflight before shared kernel PTEs change. The local-only path is also valid
/// before MP startup and under legacy PIC policy: no secondary has received a
/// root. Before ROOT publication the caller's paging handle validates its root.
/// Every configured AP must be online; cancelled, late or failed CPUs keep the
/// hierarchy sealed rather than being silently omitted from reclamation proof.
pub(crate) fn prepare_kernel_update() -> Result<KernelUpdate, &'static str> {
    // No holder of the scheduler lock waits for another CPU (spec 0051, open
    // point 3): that CPU may be spinning on the lock with IF clear.
    assert!(
        !cpudata::sched_held(),
        "kernel update under the scheduler lock"
    );
    let guard = CommandGuard::acquire()?;
    let cr3: u64;
    // SAFETY: This is a CPL0-only kernel API; reading CR3 changes no CPU state.
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
    let expected_root = ROOT.load(Ordering::Relaxed);
    if expected_root != 0 && cr3 != expected_root {
        return Err("kernel update requires kernel root");
    }
    let published = PUBLISHED.load(Ordering::Acquire);
    let count = if published {
        COUNT.load(Ordering::Relaxed)
    } else {
        0
    };
    let mut targets = [false; MAX_CPUS];
    let mut sequences = [0; MAX_CPUS];
    for (index, slot) in SLOTS[..count].iter().enumerate() {
        sequences[index] = ready_sequence(slot)?;
        targets[index] = index != 0;
    }
    Ok(KernelUpdate {
        _guard: guard,
        count,
        targets,
        sequences,
    })
}

impl KernelUpdate {
    /// Complete one already-published shared-window, heap or kernel VA PTE
    /// change.
    ///
    /// # Safety
    /// The caller must satisfy invalidate_range's contract for this one page.
    pub(crate) unsafe fn invalidate_page(self, address: u64) {
        // SAFETY: The caller supplies the same ownership and lifetime promises
        // as the range API, restricted to one page.
        unsafe { self.invalidate_range(address, 1) };
    }

    /// Complete 1..=256 already-published contiguous PTE changes with one IPI
    /// per target. Any failure stops the BSP before a retired frame is returned.
    ///
    /// # Safety
    /// The range lies wholly inside the shared window, the reserved heap or
    /// the kernel VA window. The caller owns every changed page in it and
    /// excludes live access to them throughout the update; an unchanged page
    /// keeps its mapping, and invalidation only drops cached copies of it. No
    /// IRQ-needed lock or allocator guard may be held. Removed frames and page
    /// tables remain allocated until this call returns. Call on the same
    /// BSP/root with IF=0 that prepared this ticket.
    pub(crate) unsafe fn invalidate_range(self, address: u64, pages: usize) {
        let pages = pages as u64;
        assert!(
            invalidation_range_valid(address, pages),
            "invalid kernel shootdown range"
        );
        assert!(!arch::interrupts_enabled());
        assert!(!is_secondary());
        // SAFETY: The checked window contains the entire canonical range; the
        // caller owns the update and keeps live kernel mappings valid.
        unsafe { invalidate_local_range(address, pages) };
        for index in 1..self.count {
            if self.targets[index] {
                publish_command(
                    index,
                    self.sequences[index],
                    OP_INVALIDATE_RANGE,
                    address,
                    pages,
                )
                .expect("kernel TLB shootdown delivery failed");
            }
        }
        wait_exact(
            self.count,
            &self.targets,
            &self.sequences,
            "kernel TLB shootdown acknowledgement timeout",
        );
    }
}

/// Read one caller-owned shared-window, heap or kernel VA page on every CPU
/// and verify the value and per-CPU sequence proof. This deliberately warms
/// each CPU's TLB.
///
/// # Safety
/// Run on BSP with IF=0, holding no IRQ-needed lock. The aligned page must be
/// initialized and mapped readable in the shared window, the reserved heap or
/// the kernel VA window. It remains allocated, mapped and immutable until all
/// commands complete. No allocator guard may be held, and no Rust reference
/// spans an IPI read.
pub(crate) unsafe fn probe_shared_page(address: u64, expected: u64) -> usize {
    assert!(
        invalidation_range_valid(address, 1),
        "invalid shared probe page"
    );
    let update = prepare_kernel_update().expect("shared probe preflight");
    if update.count == 0 {
        let value: u64;
        // SAFETY: Caller supplies a stable mapped page; assembly performs the
        // read without manufacturing a Rust reference to mutable shared RAM.
        unsafe {
            asm!("mov {}, qword ptr [{}]", out(reg) value, in(reg) address, options(nostack, readonly, preserves_flags))
        };
        assert_eq!(value, expected, "local shared probe value");
        return 1;
    }
    let mut targets = update.targets;
    targets[0] = true;
    for index in 0..update.count {
        publish_command(index, update.sequences[index], OP_READ_SHARED, address, 0)
            .expect("shared probe delivery failed");
    }
    wait_exact(
        update.count,
        &targets,
        &update.sequences,
        "shared probe acknowledgement timeout",
    );
    for (index, slot) in SLOTS[..update.count].iter().enumerate() {
        assert_eq!(
            slot.result.load(Ordering::Relaxed),
            proof(
                expected,
                slot.apic_id.load(Ordering::Relaxed),
                update.sequences[index]
            ),
            "shared probe observed stale translation"
        );
    }
    update.count
}

/// Called on either CPU by the DPL0 vector240 gate, always with IF=0.
pub fn handle_ipi() {
    let Some(index) = current_slot() else {
        arch::halt();
    };
    let slot = &SLOTS[index];
    apic::eoi();
    let sequence = slot.command.load(Ordering::Acquire);
    if sequence == 0 || sequence == slot.acknowledgement.load(Ordering::Relaxed) {
        return;
    }
    if slot.state.load(Ordering::Acquire) != ONLINE {
        return;
    }
    let operation = slot.operation.load(Ordering::Relaxed);
    let argument = slot.argument.load(Ordering::Relaxed);
    // Completed duplicates returned before reading any command fields. The
    // acquire of command publishes this payload together with the PTE writes.
    let payload = slot.payload.load(Ordering::Relaxed);
    if slot.acknowledgement.load(Ordering::Relaxed).checked_add(1) != Some(sequence)
        || !operation_valid(operation, argument, payload, index)
    {
        fail(index, 9);
    }
    #[cfg(feature = "boot-smoke")]
    if operation == OP_GUARD_FAULT {
        let guard = stack_bottom(index) - PAGE_SIZE;
        // The fault below keeps the secondary_fault record (spec 0051,
        // point 4). The asm block may access memory, so this store precedes it.
        slot.expect_fault.store(true, Ordering::Relaxed);
        // SAFETY: This deliberate supervisor write probes an unmapped private
        // stack guard. Assembly avoids constructing an invalid Rust reference;
        // the AP's exception path records the exact #PF and halts this CPU.
        // A mistakenly mapped guard must fail the probe instead of returning a
        // successful acknowledgement or touching the BSP's exception counters.
        unsafe {
            asm!("mov qword ptr [{}], 0", in(reg) guard, options(nostack));
        }
        fail(index, 8);
    }
    #[cfg(feature = "boot-smoke")]
    if operation == OP_UNMAPPED_FAULT {
        // As for OP_GUARD_FAULT; the read-only asm block may read this store.
        slot.expect_fault.store(true, Ordering::Relaxed);
        // SAFETY: Smoke-only command deliberately reads an absent shared alias.
        // Its ownership and completed unmap are verified by the BSP. A real
        // page fault records this AP's failure without invoking BSP test state.
        unsafe {
            asm!("mov {}, qword ptr [{}]", out(reg) _, in(reg) argument,
                options(nostack, readonly, preserves_flags));
        }
        fail(index, 10);
    }
    #[cfg(feature = "boot-smoke")]
    if operation == OP_READ_CPU_DATA {
        let value = cpu_data_proof(index, sequence);
        slot.result.store(value, Ordering::Relaxed);
        slot.completed.fetch_add(1, Ordering::Relaxed);
        slot.acknowledgement.store(sequence, Ordering::Release);
        return;
    }
    #[cfg(feature = "boot-smoke")]
    if operation == OP_USER_ENTER {
        // SAFETY: CPL0 with IF=0. The BSP validated the root, keeps it live
        // with this CPU's bit set in its space until OP_USER_LEAVE is
        // acknowledged, and it borrows every kernel mapping.
        unsafe { paging::load_root_for_test(argument) };
        let value: u64;
        // SAFETY: The BSP mapped this aligned word as a supervisor-only leaf
        // of the scratch root and keeps it until the acknowledgement.
        unsafe {
            asm!("mov {}, qword ptr [{}]", out(reg) value, in(reg) payload,
                options(nostack, readonly, preserves_flags));
        }
        slot.result
            .store(proof(value, initial_apic_id(), sequence), Ordering::Relaxed);
        slot.completed.fetch_add(1, Ordering::Relaxed);
        slot.acknowledgement.store(sequence, Ordering::Release);
        return;
    }
    #[cfg(feature = "boot-smoke")]
    if operation == OP_USER_LEAVE {
        // SAFETY: CPL0 with IF=0; the sealed kernel root maps everything live.
        unsafe { paging::load_root_for_test(ROOT.load(Ordering::Relaxed)) };
        slot.result
            .store(proof(0, initial_apic_id(), sequence), Ordering::Relaxed);
        slot.completed.fetch_add(1, Ordering::Relaxed);
        slot.acknowledgement.store(sequence, Ordering::Release);
        return;
    }
    #[cfg(feature = "boot-smoke")]
    if operation == OP_CRASH_TEST {
        crash_test_on_ap(index, argument);
    }
    #[cfg(feature = "boot-smoke")]
    if operation == OP_PRINT {
        if !print_lines(index, argument) {
            fail(index, 14);
        }
        slot.result.store(
            proof(PRINT_LINES, initial_apic_id(), sequence),
            Ordering::Relaxed,
        );
        slot.completed.fetch_add(1, Ordering::Relaxed);
        slot.acknowledgement.store(sequence, Ordering::Release);
        return;
    }
    let value = if operation == OP_INVALIDATE_RANGE {
        // SAFETY: BSP published the PTE before the command release; operation
        // validation checked the entire canonical range and bounded page count.
        // Every page is invalidated before ACK without accessing its mapping.
        unsafe { invalidate_local_range(argument, payload) };
        #[cfg(feature = "boot-smoke")]
        if withhold_acknowledgement(index, operation, sequence) {
            return;
        }
        0
    } else if operation == OP_FLUSH_USER {
        // SAFETY: CPL0 with IF=0. The current root, reloaded unchanged, maps
        // every live kernel code, stack and data page.
        let reloaded = unsafe { paging::flush_user_root(argument) };
        #[cfg(feature = "boot-smoke")]
        if withhold_acknowledgement(index, operation, sequence) {
            return;
        }
        proof(u64::from(reloaded), initial_apic_id(), sequence)
    } else if operation == OP_READ_SHARED {
        let value: u64;
        // SAFETY: The BSP owns and retains this mapped, immutable kernel page
        // until ACK. The operation's argument was checked before any access.
        unsafe {
            asm!("mov {}, qword ptr [{}]", out(reg) value, in(reg) argument,
                options(nostack, readonly, preserves_flags));
        }
        proof(value, initial_apic_id(), sequence)
    } else {
        proof(payload, initial_apic_id(), sequence)
    };
    slot.result.store(value, Ordering::Relaxed);
    slot.completed.fetch_add(1, Ordering::Relaxed);
    slot.acknowledgement.store(sequence, Ordering::Release);
}

/// Ask CPU `slot` to reschedule (spec 0051, point 3): set its request, then
/// send it vector 242. Any CPU may kick any ONLINE CPU, itself included, with
/// IF=0, holding the scheduler lock or not: the send waits only for this
/// CPU's own ICR, never for the target. A delivery failure is fatal.
pub(crate) fn kick(slot: usize) {
    assert!(!arch::interrupts_enabled(), "a kick requires IF=0");
    assert!(
        slot < configured_count(),
        "kick target outside the configured CPUs"
    );
    let target = &SLOTS[slot];
    assert_eq!(
        target.state.load(Ordering::Acquire),
        ONLINE,
        "kick target not online"
    );
    // send_ipi's fence orders this store before the IPI.
    cpudata::request_resched(slot);
    apic::send_ipi(target.apic_id.load(Ordering::Relaxed), RESCHEDULE_VECTOR)
        .expect("reschedule IPI delivery failed");
}

/// Called on any CPU by the DPL0 vector 242 gate, with IF=0: acknowledge and
/// count the reschedule IPI. The trap dispatch then acts on this CPU's
/// request (spec 0051, point 3).
pub(crate) fn handle_reschedule() {
    apic::eoi();
    cpudata::count_reschedule();
    #[cfg(feature = "boot-smoke")]
    echo_reschedule();
}

/// The round-trip check's AP half (spec 0051, step 3): an AP whose echo the
/// BSP armed takes the request the BSP set and, only if it was set, kicks the
/// BSP back. A missing request leaves the BSP to time out.
#[cfg(feature = "boot-smoke")]
fn echo_reschedule() {
    let index = cpudata::current_cpu();
    let bit = 1_u64 << index;
    if index == 0 || RESCHEDULE_ECHO.fetch_and(!bit, Ordering::AcqRel) & bit == 0 {
        return;
    }
    if cpudata::take_need_resched() {
        kick(0);
    }
}

/// Whether this CPU, as the armed timeout target, withholds this command's
/// acknowledgement after doing its work (boot-smoke SmpTlbTimeout and
/// UserFlushTimeout). It keeps withholding duplicates of the same command,
/// otherwise an unrelated IPI could turn the timeout probe into success.
#[cfg(feature = "boot-smoke")]
fn withhold_acknowledgement(index: usize, operation: u32, sequence: u64) -> bool {
    if TIMEOUT_TARGET.load(Ordering::Acquire) != index
        || TIMEOUT_OPERATION.load(Ordering::Acquire) != operation
    {
        return false;
    }
    let _ = TIMEOUT_SEQUENCE.compare_exchange(0, sequence, Ordering::AcqRel, Ordering::Acquire);
    TIMEOUT_SEQUENCE.load(Ordering::Acquire) == sequence
}

/// OP_READ_CPU_DATA's answer (0050, acceptance test 2): this CPU's CpuData
/// address and slot, both read through GS, bound to its CPUID APIC ID. The
/// BSP expects them to name the slot whose mailbox it wrote. A GS read that
/// disagrees with IA32_GS_BASE fails this CPU.
#[cfg(feature = "boot-smoke")]
fn cpu_data_proof(index: usize, sequence: u64) -> u64 {
    let (address, slot) = cpudata::read_through_gs();
    if address != cpudata::gs_base() {
        if index == 0 {
            panic!("BSP per-CPU data disagrees with IA32_GS_BASE");
        }
        fail(index, 13);
    }
    proof(address ^ slot, initial_apic_id(), sequence)
}

/// After SMP startup, check that every online CPU runs with its own CpuData
/// in GS and report `MIRROR_CPU_DATA_READY cpus=<online>` (0050-D4). The BSP
/// reads its own MSR; each AP's readback comes from its slot. Smoke kernels
/// first send OP_READ_CPU_DATA to the BSP and every online AP.
pub fn report_cpu_data() {
    assert!(!arch::interrupts_enabled() && !is_secondary());
    assert_eq!(cpudata::current_cpu(), 0, "BSP per-CPU slot");
    assert_eq!(cpudata::gs_base(), cpudata::address(0), "BSP per-CPU data");
    // COUNT stays 0 when SMP was skipped, so only the BSP counts.
    let count = COUNT.load(Ordering::Relaxed);
    let mut cpus = 1;
    for (index, slot) in SLOTS.iter().enumerate().take(count).skip(1) {
        if slot.state.load(Ordering::Acquire) == ONLINE {
            assert_eq!(
                slot.cpu_data.load(Ordering::Relaxed),
                cpudata::address(index),
                "AP per-CPU data readback"
            );
            cpus += 1;
        }
    }
    #[cfg(feature = "boot-smoke")]
    {
        let probed = read_cpu_data_self_test();
        assert_eq!(probed, cpus, "per-CPU data probe count");
        serial::println!("MIRROR_CPU_DATA_TESTS_OK cpus={}", probed);
    }
    serial::println!("MIRROR_CPU_DATA_READY cpus={}", cpus);
}

/// Send OP_READ_CPU_DATA to the BSP and every online AP and check each
/// answer against its slot's CpuData and APIC ID. Without published SMP the
/// BSP checks itself directly. Returns the number of CPUs proved.
#[cfg(feature = "boot-smoke")]
fn read_cpu_data_self_test() -> usize {
    let update = prepare_kernel_update().expect("per-CPU data probe preflight");
    if update.count == 0 {
        assert_eq!(
            cpudata::read_through_gs(),
            (cpudata::address(0), 0),
            "BSP per-CPU data through GS"
        );
        return 1;
    }
    let mut targets = update.targets;
    targets[0] = true;
    for index in 0..update.count {
        publish_command(index, update.sequences[index], OP_READ_CPU_DATA, 0, 0)
            .expect("per-CPU data probe delivery failed");
    }
    wait_exact(
        update.count,
        &targets,
        &update.sequences,
        "per-CPU data probe acknowledgement timeout",
    );
    for (index, slot) in SLOTS[..update.count].iter().enumerate() {
        assert_eq!(
            slot.result.load(Ordering::Relaxed),
            proof(
                cpudata::address(index) ^ index as u64,
                slot.apic_id.load(Ordering::Relaxed),
                update.sequences[index]
            ),
            "a CPU's GS did not resolve to its own CpuData"
        );
    }
    update.count
}

/// The SMP checks after startup (spec 0051, steps 1 and 3): the print
/// check, then the tick check, then the reschedule round trip, after which
/// `MIRROR_SMP_TESTS_OK` reports the printing CPUs, the APs that ticked and
/// the APs that answered.
#[cfg(feature = "boot-smoke")]
fn startup_self_test() {
    assert!(!arch::interrupts_enabled() && !is_secondary());
    let cpus = print_self_test();
    let ticking = tick_self_test();
    let rescheduled = reschedule_self_test();
    serial::println!(
        "MIRROR_SMP_TESTS_OK print_cpus={} print_lines={} ticking_aps={} resched_aps={}",
        cpus,
        PRINT_LINES,
        ticking,
        rescheduled
    );
}

/// The concurrent print check (spec 0051, step 1): the BSP and every online
/// AP print `PRINT_LINES` lines each at the same time, through the serial
/// lock. Each CPU prints its first line, waits until every CPU has printed
/// its first, then prints the rest, so every CPU's last line follows every
/// CPU's first. The run script checks that every line is whole and that the
/// CPUs interleave that way. Without published SMP only the BSP prints.
/// Returns the printing CPUs.
#[cfg(feature = "boot-smoke")]
fn print_self_test() -> usize {
    let update = prepare_kernel_update().expect("print check preflight");
    let cpus = update.count.max(1);
    let participants = u64::MAX >> (64 - cpus);
    PRINT_STARTED.store(0, Ordering::Release);
    for index in 1..update.count {
        publish_command(index, update.sequences[index], OP_PRINT, participants, 0)
            .expect("print check delivery failed");
    }
    assert!(
        print_lines(0, participants),
        "SMP print check barrier timeout"
    );
    wait_exact(
        update.count,
        &update.targets,
        &update.sequences,
        "SMP print check acknowledgement timeout",
    );
    for (index, slot) in SLOTS[..update.count].iter().enumerate().skip(1) {
        assert_eq!(
            slot.result.load(Ordering::Relaxed),
            proof(
                PRINT_LINES,
                slot.apic_id.load(Ordering::Relaxed),
                update.sequences[index]
            ),
            "an AP did not finish its print check"
        );
    }
    cpus
}

/// The tick check (spec 0051, step 3): with the LAPIC tick source, every
/// online AP takes `TICK_PROOF` ticks of its own timer, a missing EOI
/// included, since one stops the next, while `TICKS` advances by exactly the
/// BSP's own ticks. With the PIT source no AP timer runs. Returns the APs
/// that ticked.
#[cfg(feature = "boot-smoke")]
fn tick_self_test() -> usize {
    let aps = online_aps();
    let each = || (1..configured_count()).filter(move |&index| aps & 1 << index != 0);
    if !timer::local_timer() {
        for index in each() {
            assert_eq!(
                cpudata::local_ticks(index),
                0,
                "an AP ticked without the LAPIC tick source"
            );
        }
        return 0;
    }
    let mut before = [0; MAX_CPUS];
    for index in each() {
        before[index] = cpudata::local_ticks(index);
    }
    let clock = timer::ticks();
    let bsp = cpudata::local_ticks(0);
    assert!(
        wait_until(|| each().all(|index| cpudata::local_ticks(index) >= before[index] + TICK_PROOF)),
        "an AP's local timer did not tick"
    );
    assert_eq!(
        timer::ticks() - clock,
        cpudata::local_ticks(0) - bsp,
        "an AP's tick advanced the BSP's clock"
    );
    aps.count_ones() as usize
}

/// The reschedule round trip (spec 0051, step 3): the BSP kicks each online
/// AP in turn, the AP takes the request and kicks the BSP back, and the BSP
/// takes its own. Each side takes exactly one vector 242. Returns the APs
/// that answered.
#[cfg(feature = "boot-smoke")]
fn reschedule_self_test() -> usize {
    let aps = online_aps();
    assert!(
        !cpudata::take_need_resched(),
        "the BSP had a reschedule request before the round trip"
    );
    for index in (1..configured_count()).filter(|&index| aps & 1 << index != 0) {
        let (target, bsp) = (cpudata::reschedules(index), cpudata::reschedules(0));
        assert_eq!(
            RESCHEDULE_ECHO.fetch_or(1 << index, Ordering::AcqRel),
            0,
            "a reschedule echo was already armed"
        );
        kick(index);
        assert!(
            wait_until(|| cpudata::reschedules(index) > target && cpudata::reschedules(0) > bsp),
            "reschedule IPI round trip timeout"
        );
        assert_eq!(
            RESCHEDULE_ECHO.load(Ordering::Acquire),
            0,
            "an AP did not take its reschedule echo"
        );
        assert!(
            !cpudata::resched_requested(index),
            "an AP kept the BSP's reschedule request"
        );
        assert!(
            cpudata::take_need_resched(),
            "an AP's reschedule echo did not set the BSP's request"
        );
        assert_eq!(
            (cpudata::reschedules(index), cpudata::reschedules(0)),
            (target + 1, bsp + 1),
            "a reschedule IPI arrived more than once"
        );
    }
    aps.count_ones() as usize
}

/// Print this CPU's check lines as slot `index`: the first, a bounded wait
/// for every slot in `participants` to print its first, then the rest.
/// Returns false, after the first line only, if the wait timed out.
#[cfg(feature = "boot-smoke")]
fn print_lines(index: usize, participants: u64) -> bool {
    let print = |line: u64| {
        serial::println!(
            "MIRROR_SMP_PRINT cpu={} line={} text={}",
            index,
            line,
            PRINT_TEXT
        );
    };
    print(0);
    PRINT_STARTED.fetch_or(1 << index, Ordering::AcqRel);
    let mut bound = crate::clock::SpinBound::new(PRINT_BARRIER_NS);
    while PRINT_STARTED.load(Ordering::Acquire) & participants != participants {
        if bound.expired() {
            return false;
        }
        core::hint::spin_loop();
    }
    (1..PRINT_LINES).for_each(print);
    true
}

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    // As in prepare_kernel_update: never wait for another CPU while holding
    // the scheduler lock (spec 0051, open point 3).
    assert!(!cpudata::sched_held(), "IPI wait under the scheduler lock");
    let start = timer::ticks();
    let elapsed_start = crate::clock::snapshot().map(|clock| clock.monotonic_ns);
    while !done() {
        // Prefer hardware elapsed time so time spent with IF clear still counts
        // toward a failed CPU's deadline. Platforms without HPET retain the
        // existing delivered-tick fallback; timer IRQs supply the HLT wakeups.
        let expired = match elapsed_start {
            Some(start_ns) => {
                crate::clock::snapshot()
                    .expect("published clock disappeared")
                    .monotonic_ns
                    .saturating_sub(start_ns)
                    >= WAIT_TICKS * 10_000_000
            }
            None => timer::ticks().wrapping_sub(start) >= WAIT_TICKS,
        };
        if expired {
            return false;
        }
        // SAFETY: BSP IDT, selected timer and initialized input routes are active.
        // Even without a target ACK, timer IRQs wake HLT to observe the deadline.
        unsafe {
            asm!("sti", "hlt", "cli", options(nostack));
        }
    }
    true
}

/// The BSP verifies repeated targeted delivery and normal-memory publication
/// before and after userspace changes its own CR3 and allocates/frees memory.
pub fn rendezvous(phase: &str, rounds: u64) {
    if !PUBLISHED.load(Ordering::Acquire) {
        return;
    }
    assert!(!arch::interrupts_enabled());
    assert!(!is_secondary());
    let _guard = CommandGuard::acquire().expect("rendezvous command transport");
    let count = COUNT.load(Ordering::Relaxed);
    let mut tested = 0;
    for (index, slot) in SLOTS[..count].iter().enumerate() {
        if slot.state.load(Ordering::Acquire) == ONLINE {
            tested += 1;
        } else {
            report_failure(index);
        }
    }
    for _ in 0..rounds {
        tested = ipi_round(count);
    }
    #[cfg(feature = "boot-smoke")]
    assert_eq!(
        SLOTS[..count]
            .iter()
            .filter(|slot| slot.state.load(Ordering::Acquire) == ONLINE)
            .count(),
        count
    );
    serial::println!(
        "MIRROR_SMP_IPI_OK phase={} cpus={} rounds={}",
        phase,
        tested,
        rounds
    );
}

/// Submit one new command to each CPU that is online at the start of the round.
/// Already-offline CPUs are excluded; a participant that fails while handling
/// the command is reported separately and cannot count as a verified survivor.
fn ipi_round(count: usize) -> usize {
    let mut participating = [false; MAX_CPUS];
    for (index, slot) in SLOTS[..count].iter().enumerate() {
        if slot.state.load(Ordering::Acquire) != ONLINE {
            continue;
        }
        participating[index] = true;
        let sequence = ready_sequence(slot).expect("proof command preflight");
        publish_command(
            index,
            sequence,
            OP_PROOF,
            0,
            sequence.wrapping_mul(0xa076_1d64_78bd_642f),
        )
        .expect("IPI delivery");
    }
    let complete = wait_until(|| {
        SLOTS[..count].iter().enumerate().all(|(index, slot)| {
            !participating[index]
                || slot.state.load(Ordering::Acquire) != ONLINE
                || slot.acknowledgement.load(Ordering::Acquire)
                    == slot.command.load(Ordering::Relaxed)
        })
    });
    assert!(complete, "secondary IPI acknowledgement timeout");
    let mut verified = 0;
    for (index, slot) in SLOTS[..count].iter().enumerate() {
        if !participating[index] {
            continue;
        }
        if slot.state.load(Ordering::Acquire) != ONLINE {
            report_failure(index);
            continue;
        }
        let sequence = slot.acknowledgement.load(Ordering::Acquire);
        assert_eq!(
            slot.result.load(Ordering::Relaxed),
            proof(
                slot.payload.load(Ordering::Relaxed),
                slot.apic_id.load(Ordering::Relaxed),
                sequence
            )
        );
        assert_eq!(
            slot.completed.load(Ordering::Relaxed),
            sequence,
            "duplicate IPI execution"
        );
        verified += 1;
    }
    verified
}

/// Fault one secondary through its guard mapping, then prove that the BSP and
/// all other APs still service new targeted commands. Run after all ordinary
/// rendezvous phases; the failed AP and its resources remain permanently owned.
#[cfg(feature = "boot-smoke")]
pub fn guard_fault_self_test() {
    if !PUBLISHED.load(Ordering::Acquire) {
        return;
    }
    let count = COUNT.load(Ordering::Relaxed);
    if count <= 1 {
        return;
    }
    assert!(!arch::interrupts_enabled());
    assert!(!is_secondary());
    let _guard = CommandGuard::acquire().expect("guard probe command transport");
    let index = count - 1;
    let slot = &SLOTS[index];
    assert_eq!(slot.state.load(Ordering::Acquire), ONLINE);
    let previous_acknowledgement = slot.acknowledgement.load(Ordering::Acquire);
    let previous_completed = slot.completed.load(Ordering::Relaxed);
    let sequence = ready_sequence(slot).expect("guard probe command preflight");
    publish_command(index, sequence, OP_GUARD_FAULT, 0, 0).expect("AP guard probe IPI delivery");
    assert!(
        wait_until(|| slot.state.load(Ordering::Acquire) != ONLINE),
        "AP guard probe exception timeout"
    );
    assert_eq!(slot.state.load(Ordering::Acquire), FAILED);
    assert_eq!(slot.fault_vector.load(Ordering::Relaxed), 14);
    assert_eq!(slot.fault_error.load(Ordering::Relaxed), 2);
    assert_eq!(
        slot.fault_address.load(Ordering::Relaxed),
        stack_bottom(index) - PAGE_SIZE
    );
    assert_ne!(slot.fault_rip.load(Ordering::Relaxed), 0);
    assert_eq!(
        slot.acknowledgement.load(Ordering::Relaxed),
        previous_acknowledgement,
        "faulting AP acknowledged an incomplete command"
    );
    assert_eq!(slot.completed.load(Ordering::Relaxed), previous_completed);
    assert_eq!(
        ipi_round(count),
        count - 1,
        "surviving CPU rendezvous failed"
    );
    assert_eq!(slot.state.load(Ordering::Acquire), FAILED);
    serial::println!(
        "MIRROR_SMP_GUARD_OK slot={} vector=14 remaining={}",
        index,
        count - 1
    );
}

/// Read a shared alias only after its synchronous unmap completed, and prove
/// that the selected AP faults while every surviving CPU remains responsive.
///
/// # Safety
/// The caller owns the aligned shared-window address and completed its unmap
/// on every CPU. It remains unmapped until this test ends. Run on BSP with IF=0
/// after all ordinary rendezvous phases, in a boot with at least two CPUs.
#[cfg(feature = "boot-smoke")]
pub(crate) unsafe fn unmapped_shared_fault_self_test(address: u64) {
    assert!(shared_page_valid(address), "invalid unmapped probe page");
    let update = prepare_kernel_update().expect("unmapped probe preflight");
    assert!(update.count > 1, "unmapped probe requires an online AP");
    let index = update.count - 1;
    let slot = &SLOTS[index];
    let previous_acknowledgement = slot.acknowledgement.load(Ordering::Acquire);
    let previous_completed = slot.completed.load(Ordering::Relaxed);
    publish_command(
        index,
        update.sequences[index],
        OP_UNMAPPED_FAULT,
        address,
        0,
    )
    .expect("unmapped probe delivery failed");
    assert!(
        wait_until(|| slot.state.load(Ordering::Acquire) != ONLINE),
        "unmapped AP probe exception timeout"
    );
    assert_eq!(slot.state.load(Ordering::Acquire), FAILED);
    assert_eq!(slot.fault_vector.load(Ordering::Relaxed), 14);
    assert_eq!(slot.fault_error.load(Ordering::Relaxed), 0);
    assert_eq!(slot.fault_address.load(Ordering::Relaxed), address);
    assert_ne!(slot.fault_rip.load(Ordering::Relaxed), 0);
    assert_eq!(
        slot.acknowledgement.load(Ordering::Relaxed),
        previous_acknowledgement,
        "faulting AP acknowledged an incomplete read"
    );
    assert_eq!(slot.completed.load(Ordering::Relaxed), previous_completed);
    assert_eq!(
        ipi_round(update.count),
        update.count - 1,
        "surviving CPU rendezvous failed"
    );
    assert_eq!(slot.state.load(Ordering::Acquire), FAILED);
    serial::println!(
        "MIRROR_SMP_UNMAP_OK slot={} vector=14 remaining={}",
        index,
        update.count - 1
    );
}

/// Suppress the final AP's next invalidation ACK after executing INVLPG. The
/// subsequent kernel update must stop before returning any reclaimable frame.
#[cfg(feature = "boot-smoke")]
pub(crate) fn arm_shootdown_timeout_for_test() {
    let update = prepare_kernel_update().expect("shootdown timeout preflight");
    assert!(update.count > 1, "shootdown timeout probe requires an AP");
    assert!(!TIMEOUT_ARMED.swap(true, Ordering::SeqCst));
    let target = update.count - 1;
    TIMEOUT_OPERATION.store(OP_INVALIDATE_RANGE, Ordering::Release);
    TIMEOUT_TARGET.store(target, Ordering::Release);
    serial::println!("MIRROR_SMP_TLB_TIMEOUT_ARMED target={}", target);
}

/// Withhold the final AP's next OP_FLUSH_USER acknowledgement after its
/// reload (UserFlushTimeout). The flush that waits for it must stop before
/// the withdrawn frames return to the allocator.
#[cfg(feature = "boot-smoke")]
pub(crate) fn arm_user_flush_timeout_for_test() {
    let count = configured_count();
    assert!(count > 1, "user flush timeout probe requires an AP");
    assert!(!TIMEOUT_ARMED.swap(true, Ordering::SeqCst));
    let target = count - 1;
    TIMEOUT_OPERATION.store(OP_FLUSH_USER, Ordering::Release);
    TIMEOUT_TARGET.store(target, Ordering::Release);
    serial::println!("MIRROR_USER_FLUSH_TIMEOUT_ARMED target={}", target);
}

/// Run a crash case that needs an AP (spec 0051, step 4), at the end of
/// boot, on the BSP with IF clear: announce the panicking CPU, send each AP
/// its half, then play the BSP's. It never returns: the case ends in a
/// crash, whose stop line and exit the run script checks.
#[cfg(feature = "boot-smoke")]
pub(crate) fn crash_test(test: CrashTest) -> ! {
    assert!(!arch::interrupts_enabled() && !is_secondary());
    let aps = online_aps();
    assert!(aps & 0b10 != 0, "a crash case requires slot 1 online");
    let (cpu, targets) = match test {
        CrashTest::ApPanic => ("1", 0b10),
        CrashTest::SchedStall => ("0", 0b10),
        CrashTest::DoublePanic => ("any", aps),
    };
    serial::println!("MIRROR_TEST_EXPECT_PANIC cpu={}", cpu);
    CRASH_TEST_CPUS.store(1 + aps.count_ones() as usize, Ordering::Release);
    let _guard = CommandGuard::acquire().expect("crash case command transport");
    for index in (1..configured_count()).filter(|&index| targets & 1 << index != 0) {
        let sequence = ready_sequence(&SLOTS[index]).expect("crash case target not online");
        publish_command(index, sequence, OP_CRASH_TEST, test as u64, 0)
            .expect("crash case delivery failed");
    }
    match test {
        CrashTest::ApPanic | CrashTest::SchedStall => {
            assert!(
                wait_until(|| CRASH_TEST_HELD.load(Ordering::Acquire)),
                "crash case: slot 1 did not take the scheduler lock"
            );
            // ApPanic: slot 1 panics once this spin has taken its ticket,
            // and its crash stops this CPU here by NMI. SchedStall: slot 1
            // never releases, so this spin panics with "scheduler lock wait
            // exceeded" and this CPU's crash stops slot 1 by NMI.
            crate::userspace::wait_for_scheduler_lock_for_test();
            panic!("crash case: the scheduler lock was released");
        }
        CrashTest::DoublePanic => {
            assert!(crash_test_barrier(), "crash case: an AP did not arrive");
            panic!("double crash test panic");
        }
    }
}

/// DoublePanic's barrier: arrive, then wait, bounded, until every CPU in
/// the case has. Returns whether they all did.
#[cfg(feature = "boot-smoke")]
fn crash_test_barrier() -> bool {
    let cpus = CRASH_TEST_CPUS.load(Ordering::Acquire);
    CRASH_TEST_ARRIVED.fetch_add(1, Ordering::AcqRel);
    let mut bound = crate::clock::SpinBound::new(CRASH_TEST_WAIT_NS);
    while CRASH_TEST_ARRIVED.load(Ordering::Acquire) < cpus {
        if bound.expired() {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// A crash case's AP half, in this AP's command handler with IF clear. Each
/// bounded wait that runs out records the slot FAILED, which the run
/// script's stop check then reports as a CPU that never stopped.
#[cfg(feature = "boot-smoke")]
fn crash_test_on_ap(index: usize, argument: u64) -> ! {
    if argument == CrashTest::DoublePanic as u64 {
        if !crash_test_barrier() {
            fail(index, FAIL_CRASH_TEST);
        }
        panic!("double crash test panic");
    }
    // ApPanic and SchedStall: take the scheduler lock and never release it.
    let mut bound = crate::clock::SpinBound::new(CRASH_TEST_WAIT_NS);
    crate::userspace::hold_scheduler_lock_for_test();
    CRASH_TEST_HELD.store(true, Ordering::Release);
    let panics = argument == CrashTest::ApPanic as u64;
    loop {
        if panics && crate::userspace::scheduler_lock_contended_for_test() {
            panic!("AP crash test panic");
        }
        if bound.expired() {
            fail(index, FAIL_CRASH_TEST);
        }
        core::hint::spin_loop();
    }
}

/// The configured CPU count once SMP startup has published its slots, else 0.
fn configured_count() -> usize {
    if PUBLISHED.load(Ordering::Acquire) {
        COUNT.load(Ordering::Relaxed)
    } else {
        0
    }
}

/// The commands one call sent: the configured CPU count, the selected slots
/// and their sequences.
struct Sent {
    count: usize,
    selected: [bool; MAX_CPUS],
    sequences: [u64; MAX_CPUS],
}

/// Send one command to each AP in `targets`, a mask of slots, and wait for
/// every acknowledgement; a missing one panics with `timeout`. The caller
/// holds the command guard, on the BSP with IF=0 and no scheduler lock.
fn command_each(
    targets: u64,
    operation: u32,
    argument: u64,
    payload: u64,
    timeout: &'static str,
) -> Sent {
    let count = configured_count();
    assert!(
        targets & 1 == 0 && (count == MAX_CPUS || targets >> count == 0),
        "command targets outside the configured APs"
    );
    let mut sent = Sent {
        count,
        selected: [false; MAX_CPUS],
        sequences: [0; MAX_CPUS],
    };
    for (index, slot) in SLOTS.iter().enumerate().take(count).skip(1) {
        if targets & (1 << index) != 0 {
            sent.sequences[index] = ready_sequence(slot).expect("command target not online");
            sent.selected[index] = true;
        }
    }
    for index in 1..count {
        if sent.selected[index] {
            publish_command(index, sent.sequences[index], operation, argument, payload)
                .expect("command delivery failed");
        }
    }
    wait_exact(count, &sent.selected, &sent.sequences, timeout);
    sent
}

/// Make every CPU in `targets`, a mask of AP slots, drop its translations
/// through user root `root` (spec 0051, point 8), and return how many had it
/// loaded and reloaded CR3. A removal calls this after it withdrew entries
/// and read its space's mask, with the scheduler lock released; its frames
/// stay allocated until this returns. A missing acknowledgement panics.
pub(crate) fn flush_user(root: u64, targets: u64) -> usize {
    // No holder of the scheduler lock waits for another CPU (spec 0051, open
    // point 3).
    assert!(
        !cpudata::sched_held(),
        "user shootdown under the scheduler lock"
    );
    assert!(!arch::interrupts_enabled() && !is_secondary());
    assert!(user_root_valid(root), "invalid user shootdown root");
    if targets == 0 {
        return 0;
    }
    let _guard = CommandGuard::acquire().expect("user shootdown command transport");
    let sent = command_each(
        targets,
        OP_FLUSH_USER,
        root,
        0,
        "user TLB shootdown acknowledgement timeout",
    );
    let mut reloaded = 0;
    for (index, slot) in SLOTS[..sent.count].iter().enumerate() {
        if !sent.selected[index] {
            continue;
        }
        let answer = |reload: u64| {
            proof(
                reload,
                slot.apic_id.load(Ordering::Relaxed),
                sent.sequences[index],
            )
        };
        let result = slot.result.load(Ordering::Relaxed);
        if result == answer(1) {
            reloaded += 1;
        } else {
            assert_eq!(result, answer(0), "a CPU did not complete its user flush");
        }
    }
    reloaded
}

/// The online APs, as a mask of slots (boot-smoke user flush test).
#[cfg(feature = "boot-smoke")]
pub(crate) fn online_aps() -> u64 {
    SLOTS[..configured_count()]
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, slot)| slot.state.load(Ordering::Acquire) == ONLINE)
        .fold(0, |mask, (index, _)| mask | 1 << index)
}

/// Make every AP in `targets` load user root `root` and read the supervisor
/// word at `address` through it, which must hold `expected` (boot-smoke user
/// flush test). The caller set their bits in the space's mask first and
/// keeps the root and the word live until they leave.
#[cfg(feature = "boot-smoke")]
pub(crate) fn enter_user_root_for_test(targets: u64, root: u64, address: u64, expected: u64) {
    assert!(!arch::interrupts_enabled() && !is_secondary());
    if targets == 0 {
        return;
    }
    let _guard = CommandGuard::acquire().expect("user root test command transport");
    let sent = command_each(
        targets,
        OP_USER_ENTER,
        root,
        address,
        "user root test acknowledgement timeout",
    );
    for (index, slot) in SLOTS[..sent.count].iter().enumerate() {
        if sent.selected[index] {
            assert_eq!(
                slot.result.load(Ordering::Relaxed),
                proof(
                    expected,
                    slot.apic_id.load(Ordering::Relaxed),
                    sent.sequences[index]
                ),
                "an AP read a stale user translation"
            );
        }
    }
}

/// Make every AP in `targets` return to the kernel root (boot-smoke user
/// flush test). The caller then clears their bits in the space's mask.
#[cfg(feature = "boot-smoke")]
pub(crate) fn leave_user_root_for_test(targets: u64) {
    assert!(!arch::interrupts_enabled() && !is_secondary());
    if targets == 0 {
        return;
    }
    let _guard = CommandGuard::acquire().expect("user root test command transport");
    command_each(
        targets,
        OP_USER_LEAVE,
        0,
        0,
        "user root test acknowledgement timeout",
    );
}

fn report_failure(index: usize) {
    let slot = &SLOTS[index];
    serial::println!(
        "MIRROR_SMP_CPU_FAILED slot={} apic={} state={} vector={} error={} rip={:#x} address={:#x}",
        index,
        slot.apic_id.load(Ordering::Relaxed),
        slot.state.load(Ordering::Acquire),
        slot.fault_vector.load(Ordering::Relaxed),
        slot.fault_error.load(Ordering::Relaxed),
        slot.fault_rip.load(Ordering::Relaxed),
        slot.fault_address.load(Ordering::Relaxed)
    );
}

#[cfg(feature = "boot-smoke")]
fn self_test() {
    let identities = [Identity { apic: 7, uid: 10 }, Identity { apic: 2, uid: 42 }];
    let cpus = [
        mirror_acpi::Cpu {
            apic_id: 7,
            uid: 10,
            enabled: true,
            ..Default::default()
        },
        mirror_acpi::Cpu {
            apic_id: 2,
            uid: 42,
            enabled: true,
            ..Default::default()
        },
    ];
    assert_eq!(validate_identities(&identities, &cpus, 7), Ok(()));
    assert_eq!(validate_identities(&[], &cpus, 7), Err("cpu_count"));
    assert_eq!(
        validate_identities(&identities, &cpus, 0),
        Err("missing_bsp")
    );
    assert_eq!(
        validate_identities(&identities[..1], &cpus, 7),
        Err("cpu_count_mismatch")
    );
    assert_eq!(
        validate_identities(&[identities[0], identities[0]], &cpus, 2),
        Err("missing_bsp")
    );
    assert_eq!(
        validate_identities(
            &[
                identities[0],
                Identity {
                    uid: identities[0].uid,
                    ..identities[1]
                }
            ],
            &cpus,
            7
        ),
        Err("duplicate_cpu")
    );
    let non_bsp_duplicate = [identities[0], identities[1], identities[1]];
    let duplicate_topology = [cpus[0], cpus[1], cpus[1]];
    assert_eq!(
        validate_identities(&non_bsp_duplicate, &duplicate_topology, 7),
        Err("duplicate_cpu")
    );
    assert_eq!(
        validate_identities(
            &[
                identities[0],
                Identity {
                    uid: 99,
                    ..identities[1]
                }
            ],
            &cpus,
            7
        ),
        Err("cpu_identity_mismatch")
    );
    // Slot 1's lower guard is the first page after the BSP region, whose IST3
    // top lies one guard page below AP_BASE.
    assert_eq!(stack_bottom(1) - PAGE_SIZE, stacks::AP_BASE);
    let mut previous_top = stacks::AP_BASE - PAGE_SIZE;
    for index in 1..MAX_CPUS {
        let set = slot_stacks(index);
        assert_eq!(set[0], (stack_bottom(index), STACK_PAGES));
        assert_eq!(stack_top(index) - stack_bottom(index), 64 * 1024);
        for (ist, top) in ist_tops(index).into_iter().enumerate() {
            let (bottom, pages) = set[ist + 1];
            assert_eq!(bottom + pages as u64 * PAGE_SIZE, top);
            assert_eq!(top - bottom, 16 * 1024);
        }
        // Every stack keeps its own guard below and above, so each neighbour,
        // across slot boundaries too, sits exactly two unmapped pages away.
        for (bottom, pages) in set {
            assert!(bottom.is_multiple_of(PAGE_SIZE));
            assert_eq!(bottom - previous_top, 2 * PAGE_SIZE);
            previous_top = bottom + pages as u64 * PAGE_SIZE;
        }
        assert_eq!(previous_top + PAGE_SIZE, slot_base(index) + STACK_STRIDE);
    }
    // The last slot's upper guard ends below the shared window.
    assert!(previous_top + PAGE_SIZE <= paging::SHARED_BASE);
    let state = AtomicU32::new(STARTING);
    assert!(
        state
            .compare_exchange(STARTING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    );
    assert!(
        state
            .compare_exchange(STARTING, ONLINE, Ordering::Release, Ordering::Acquire)
            .is_err()
    );
    assert_ne!(proof(123, 7, 1), proof(123, 2, 1));
    assert_ne!(proof(123, 7, 1), proof(123, 7, 2));
    assert!(shared_page_valid(paging::SHARED_BASE));
    assert!(shared_page_valid(
        paging::SHARED_BASE + (paging::SHARED_PAGES as u64 - 1) * PAGE_SIZE
    ));
    for address in [
        0,
        paging::SHARED_BASE - PAGE_SIZE,
        paging::SHARED_BASE + 1,
        paging::SHARED_BASE + paging::SHARED_PAGES as u64 * PAGE_SIZE,
        0x0000_8000_0000_0000,
    ] {
        assert!(!shared_page_valid(address));
        assert!(!operation_valid(OP_INVALIDATE_RANGE, address, 1, 1));
        assert!(!operation_valid(OP_READ_SHARED, address, 0, 1));
    }
    for (start, bytes) in [
        (paging::SHARED_BASE, paging::SHARED_PAGES as u64 * PAGE_SIZE),
        (crate::heap::HEAP_START, crate::heap::HEAP_MAX_SIZE as u64),
    ] {
        let end = start.checked_add(bytes).unwrap();
        for (address, pages) in [
            (start, 1),
            (start, MAX_INVALIDATE_PAGES),
            (end - PAGE_SIZE, 1),
            (end - MAX_INVALIDATE_PAGES * PAGE_SIZE, MAX_INVALIDATE_PAGES),
        ] {
            assert!(invalidation_range_valid(address, pages));
            assert!(operation_valid(OP_INVALIDATE_RANGE, address, pages, 1));
        }
        for (address, pages) in [
            (start, 0),
            (start, MAX_INVALIDATE_PAGES + 1),
            (start, u64::MAX),
            (start + 1, 1),
            (start - PAGE_SIZE, 2),
            (end, 1),
            (end - PAGE_SIZE, 2),
            (
                end - (MAX_INVALIDATE_PAGES - 1) * PAGE_SIZE,
                MAX_INVALIDATE_PAGES,
            ),
        ] {
            assert!(!invalidation_range_valid(address, pages));
            assert!(!operation_valid(OP_INVALIDATE_RANGE, address, pages, 1));
        }
    }
    // Even a bounded batch may wrap the address space; an accepted range must
    // never turn that overflow into an apparently low exclusive end.
    let last_address_page = u64::MAX - (PAGE_SIZE - 1);
    assert!(!invalidation_range_valid(last_address_page, 1));
    assert!(!operation_valid(
        OP_INVALIDATE_RANGE,
        last_address_page,
        1,
        1
    ));
    assert!(operation_valid(
        OP_READ_SHARED,
        crate::heap::HEAP_START,
        0,
        1
    ));
    assert!(!shared_page_valid(crate::heap::HEAP_START));
    assert!(!operation_valid(
        OP_UNMAPPED_FAULT,
        crate::heap::HEAP_START,
        0,
        1
    ));
    assert!(!operation_valid(0, 0, 0, 1));
    assert!(!operation_valid(u32::MAX, 0, 0, 1));
    assert!(!operation_valid(OP_PROOF, paging::SHARED_BASE, 0, 1));
    assert!(!operation_valid(OP_GUARD_FAULT, 0, 0, 0));
    assert!(operation_valid(OP_READ_CPU_DATA, 0, 0, 0));
    assert!(operation_valid(OP_READ_CPU_DATA, 0, 0, 1));
    assert!(!operation_valid(
        OP_READ_CPU_DATA,
        paging::SHARED_BASE,
        0,
        1
    ));
    assert!(!operation_valid(OP_READ_CPU_DATA, 0, 1, 1));
    for test in CrashTest::ALL {
        assert!(operation_valid(OP_CRASH_TEST, test as u64, 0, 1));
        assert!(!operation_valid(OP_CRASH_TEST, test as u64, 0, 0));
        assert!(!operation_valid(OP_CRASH_TEST, test as u64, 1, 1));
    }
    for argument in [0, 4, u64::MAX] {
        assert!(!operation_valid(OP_CRASH_TEST, argument, 0, 1));
    }
    assert!(operation_valid(OP_PRINT, 0b11, 0, 1));
    assert!(operation_valid(OP_PRINT, u64::MAX, 0, MAX_CPUS - 1));
    assert!(!operation_valid(OP_PRINT, 0b11, 0, 0));
    assert!(!operation_valid(OP_PRINT, 0b10, 0, 1));
    assert!(!operation_valid(OP_PRINT, 0b101, 0, 1));
    assert!(!operation_valid(OP_PRINT, 0b11, 1, 1));
    // A user flush names a user root, never the kernel's, and goes to an AP.
    let mask = PHYSICAL_MASK.load(Ordering::Relaxed);
    let kernel_root = ROOT.load(Ordering::Relaxed);
    let user_root = if kernel_root == 0x1000 {
        0x2000
    } else {
        0x1000
    };
    assert!(operation_valid(OP_FLUSH_USER, user_root, 0, 1));
    for (root, payload, index) in [
        (user_root, 0, 0),
        (user_root, 1, 1),
        (0, 0, 1),
        (user_root + 8, 0, 1),
        (kernel_root, 0, 1),
        (mask + PAGE_SIZE, 0, 1),
    ] {
        assert!(!operation_valid(OP_FLUSH_USER, root, payload, index));
    }
    assert!(operation_valid(OP_USER_ENTER, user_root, PAGE_SIZE, 1));
    for (root, address, index) in [
        (user_root, 0, 1),
        (user_root, PAGE_SIZE + 4, 1),
        (user_root, 1 << 47, 1),
        (user_root, PAGE_SIZE, 0),
        (kernel_root, PAGE_SIZE, 1),
    ] {
        assert!(!operation_valid(OP_USER_ENTER, root, address, index));
    }
    assert!(operation_valid(OP_USER_LEAVE, 0, 0, 1));
    assert!(!operation_valid(OP_USER_LEAVE, user_root, 0, 1));
    assert!(!operation_valid(OP_USER_LEAVE, 0, 0, 0));
    assert!(!operation_valid(
        OP_UNMAPPED_FAULT,
        paging::SHARED_BASE,
        0,
        0
    ));
    assert_eq!(next_sequence(0, 0, 0), Ok(1));
    assert_eq!(next_sequence(7, 7, 7), Ok(8));
    assert_eq!(
        next_sequence(8, 7, 7),
        Err("kernel command still outstanding")
    );
    assert_eq!(
        next_sequence(7, 7, 6),
        Err("kernel command completion mismatch")
    );
    assert_eq!(
        next_sequence(u64::MAX, u64::MAX, u64::MAX),
        Err("kernel command sequence exhausted")
    );
    assert!(target_complete(ONLINE, 4, 4));
    assert!(!target_complete(ONLINE, 3, 4));
    assert!(!target_complete(ONLINE, 5, 4));
    let candidate = Slot::new();
    for state in [ABSENT, STARTING, FAILED, CANCELLED] {
        candidate.state.store(state, Ordering::Relaxed);
        assert!(!target_complete(state, 4, 4));
        assert_eq!(
            ready_sequence(&candidate),
            Err("kernel command target not online")
        );
    }
    candidate.state.store(ONLINE, Ordering::Relaxed);
    assert_eq!(ready_sequence(&candidate), Ok(1));
    let guard = CommandGuard::acquire().expect("command guard self-test");
    assert!(
        CommandGuard::acquire().is_err(),
        "nested command submission"
    );
    drop(guard);
    drop(CommandGuard::acquire().expect("command guard released"));
    // MIRROR_SMP_TESTS_OK follows the print check, once the APs are online.
}
