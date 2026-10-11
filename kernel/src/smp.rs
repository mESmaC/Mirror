//! Limine-assisted secondary CPU bring-up and bounded IPI rendezvous.
//!
//! APs own guarded kernel stacks, GDT/TSS/IST, local APIC and CPU controls. They
//! only execute this module and the AP exception path: no allocator, scheduler,
//! userspace, device queues or serial access. Shared mapping changes use bounded
//! synchronous shootdowns; retired frames remain owned until every target ACKs.
//! This establishes CPU ownership and interrupt delivery, not SMP scheduling.
//! https://github.com/Limine-Bootloader/limine-protocol/blob/trunk/PROTOCOL.md#mp-multiprocessor-feature

use crate::{
    apic, arch,
    boot::BootInfo,
    fpu, interrupts,
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{self, PageTables},
    serial, stacks, timer,
};
use core::{
    arch::{asm, x86_64::__cpuid},
    cell::UnsafeCell,
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
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
const WAIT_TICKS: u64 = 500;
const ABSENT: u32 = 0;
const STARTING: u32 = 1;
const ONLINE: u32 = 2;
const FAILED: u32 = 3;
const CANCELLED: u32 = 4;
const OP_PROOF: u32 = 1;
const OP_INVALIDATE_RANGE: u32 = 2;
const MAX_INVALIDATE_PAGES: u64 = 256;
const OP_READ_SHARED: u32 = 3;
#[cfg(feature = "boot-smoke")]
const OP_GUARD_FAULT: u32 = 4;
#[cfg(feature = "boot-smoke")]
const OP_UNMAPPED_FAULT: u32 = 5;

#[repr(align(64))]
struct Slot {
    apic_id: AtomicU32,
    state: AtomicU32,
    entered: AtomicBool,
    stack_top: AtomicU64,
    actual_stack: AtomicU64,
    actual_root: AtomicU64,
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
#[cfg(feature = "boot-smoke")]
static TIMEOUT_ARMED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "boot-smoke")]
static TIMEOUT_TARGET: AtomicUsize = AtomicUsize::new(MAX_CPUS);
#[cfg(feature = "boot-smoke")]
static TIMEOUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    #[cfg(feature = "boot-smoke")]
    self_test();
    if !apic::is_enabled() {
        serial::println!("MIRROR_SMP_SKIPPED reason=legacy_pic online=1");
        return;
    }
    let result = prepare(boot, topology, tables, frames);
    let (cpus, count) = match result {
        Ok(value) => value,
        Err(reason) => {
            serial::println!("MIRROR_SMP_SKIPPED reason={} online=1", reason);
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
    for (index, slot) in SLOTS[..count].iter().enumerate().skip(1) {
        let _ =
            slot.state
                .compare_exchange(STARTING, CANCELLED, Ordering::AcqRel, Ordering::Acquire);
        if slot.state.load(Ordering::Acquire) == ONLINE {
            online += 1;
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
    serial::println!(
        "MIRROR_SMP_READY discovered={} online={} bsp={} aps={} userspace_cpus=1",
        count,
        online,
        BSP_ID.load(Ordering::Relaxed),
        online - 1
    );
    #[cfg(feature = "boot-smoke")]
    assert_eq!(online, count, "not every secondary reached kernel idle");
    rendezvous("boot", 8);
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
    for slot in 1..count {
        // SAFETY: The BSP runs with IF=0 on the active root and owns the
        // allocator. No AP was released and the root is not yet published, so
        // kernel mappings are unsealed. Only this slot uses its arena range.
        // Each of its stacks owns a separate span of the slot, guards
        // included, so no two share a page; the self-test checks the layout.
        if let Err(reason) = unsafe { stacks::map_set(tables, frames, &slot_stacks(slot)) } {
            // map_set already rolled back this slot's stacks.
            for mapped in 1..slot {
                // SAFETY: Same BSP ownership. This slot's stacks were mapped
                // above and no AP was released to run on them.
                unsafe { stacks::unmap_set(tables, frames, &slot_stacks(mapped)) };
            }
            return Err(reason);
        }
    }
    Ok(())
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
    let top = slot.stack_top.load(Ordering::Relaxed);
    // SAFETY: Unique validated AP slot. Limine's reserved stack and the kernel
    // image, with this CPU's descriptors and the shared IDT, are mapped now and
    // in the sealed root. The RSP0 and IST1-3 tops sit above this slot's
    // guarded stacks, which the BSP mapped before publication, but only in the
    // sealed root: until activate_secondary below, a #DF finds its IST
    // unmapped and triple-faults this CPU instead of being recorded. LINT1
    // stays masked until apic::init_secondary. No CPL3 entry is allowed.
    if !unsafe { interrupts::init_secondary(index, top, ist_tops(index)) } {
        fail(index, 2);
    }
    if fpu::init_secondary().is_err() {
        fail(index, 3);
    }
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
    // SAFETY: CPU-local register inspection at CPL0, no mutation.
    unsafe {
        asm!("mov {}, rsp", out(reg) stack, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr3", out(reg) root, options(nomem, nostack, preserves_flags));
    }
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
    slot.actual_stack.store(stack, Ordering::Relaxed);
    slot.actual_root.store(root, Ordering::Relaxed);
    // Cancellation is terminal: a late AP may never resurrect an offline slot.
    if slot
        .state
        .compare_exchange(STARTING, ONLINE, Ordering::Release, Ordering::Acquire)
        .is_err()
    {
        arch::halt();
    }
    loop {
        if slot.state.load(Ordering::Acquire) != ONLINE {
            arch::halt();
        }
        // SAFETY: Private descriptors, validated local APIC and shared mappings
        // are ready. Only rendezvous/spurious interrupts return on an AP. CLI
        // closes each idle iteration so no Rust handler/state update nests.
        unsafe {
            asm!("sti", "hlt", "cli", options(nostack));
        }
    }
}

fn fail(index: usize, reason: u64) -> ! {
    SLOTS[index].fault_vector.store(u64::MAX, Ordering::Relaxed);
    SLOTS[index].fault_error.store(reason, Ordering::Relaxed);
    SLOTS[index].state.store(FAILED, Ordering::Release);
    arch::halt()
}

fn initial_apic_id() -> u32 {
    // xAPIC discovery rejects IDs >=255, where this 8-bit field would be ambiguous.
    __cpuid(1).ebx >> 24
}

pub fn is_secondary() -> bool {
    PUBLISHED.load(Ordering::Acquire) && initial_apic_id() != BSP_ID.load(Ordering::Relaxed)
}

fn current_slot() -> Option<usize> {
    if !PUBLISHED.load(Ordering::Acquire) {
        return None;
    }
    let id = initial_apic_id();
    SLOTS[..COUNT.load(Ordering::Relaxed)]
        .iter()
        .position(|slot| slot.apic_id.load(Ordering::Relaxed) == id)
}

/// The AP exception path never touches BSP serial, scheduler or panic state.
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

fn operation_valid(operation: u32, argument: u64, payload: u64, index: usize) -> bool {
    match operation {
        OP_PROOF => argument == 0,
        OP_INVALIDATE_RANGE => invalidation_range_valid(argument, payload),
        OP_READ_SHARED => invalidation_range_valid(argument, 1),
        #[cfg(feature = "boot-smoke")]
        OP_GUARD_FAULT => index != 0 && argument == 0,
        #[cfg(feature = "boot-smoke")]
        OP_UNMAPPED_FAULT => index != 0 && shared_page_valid(argument),
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
    /// Complete one already-published shared-window or heap PTE change.
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
    /// The caller owns every aligned page in the range, wholly inside either
    /// the shared window or reserved heap, and excludes live access throughout
    /// the update. No IRQ-needed lock or allocator guard may be held. Removed
    /// frames and page tables remain allocated until this call returns. Call
    /// on the same BSP/root with IF=0 that prepared this ticket.
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

/// Read one caller-owned shared-window or heap page on every CPU and verify the
/// value and per-CPU sequence proof. This deliberately warms each CPU's TLB.
///
/// # Safety
/// Run on BSP with IF=0, holding no IRQ-needed lock. The aligned page must be
/// initialized and mapped readable in the shared window or reserved heap. It
/// remains allocated, mapped and immutable until all commands complete. No
/// allocator guard may be held, and no Rust reference spans an IPI read.
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
        // SAFETY: Smoke-only command deliberately reads an absent shared alias.
        // Its ownership and completed unmap are verified by the BSP. A real
        // page fault records this AP's failure without invoking BSP test state.
        unsafe {
            asm!("mov {}, qword ptr [{}]", out(reg) _, in(reg) argument,
                options(nostack, readonly, preserves_flags));
        }
        fail(index, 10);
    }
    let value = if operation == OP_INVALIDATE_RANGE {
        // SAFETY: BSP published the PTE before the command release; operation
        // validation checked the entire canonical range and bounded page count.
        // Every page is invalidated before ACK without accessing its mapping.
        unsafe { invalidate_local_range(argument, payload) };
        #[cfg(feature = "boot-smoke")]
        if TIMEOUT_TARGET.load(Ordering::Acquire) == index {
            let _ =
                TIMEOUT_SEQUENCE.compare_exchange(0, sequence, Ordering::AcqRel, Ordering::Acquire);
            if TIMEOUT_SEQUENCE.load(Ordering::Acquire) == sequence {
                // Keep suppressing duplicates of this same command, otherwise
                // an unrelated IPI could turn the timeout probe into success.
                return;
            }
        }
        0
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

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
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
    TIMEOUT_TARGET.store(target, Ordering::Release);
    serial::println!("MIRROR_SMP_TLB_TIMEOUT_ARMED target={}", target);
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
    serial::println!("MIRROR_SMP_TESTS_OK");
}
