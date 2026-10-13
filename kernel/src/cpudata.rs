//! Per-CPU data reached through GS (spec 0050-D1).
//!
//! `CPU_DATA` is a static table indexed by SMP slot, 0 for the BSP. Nothing
//! per-CPU is ever allocated: APs cannot allocate (smp.rs). Each CPU installs
//! its own entry once, early in its bring-up, and from then on the MSR roles
//! are fixed:
//!
//! - Whenever kernel code runs, IA32_GS_BASE names this CPU's `CpuData`.
//! - IA32_KERNEL_GS_BASE is scratch in kernel mode. The user's GS base lives
//!   only in the thread's trap frame.
//! - Whenever IA32_GS_BASE is not a kernel-half address, IA32_KERNEL_GS_BASE
//!   holds this CPU's `CpuData`. Install establishes this, and so does every
//!   CPL3 exit, which rewrites IA32_KERNEL_GS_BASE before user execution. The
//!   paranoid NMI, #DF and #MC entries rely on it: they swap only when
//!   IA32_GS_BASE is not kernel-half (interrupts.S).
//!
//! Before install both GS MSRs are 0. Code that can run before install, the
//! exception dispatch and the fatal paths, identifies its CPU with
//! `try_current_slot`, which reads the MSR and never dereferences GS, and
//! falls back to smp.rs's CPUID scan. Everything else uses `current_cpu`, one
//! GS-relative read.

use core::arch::asm;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub(crate) const MAX_CPUS: usize = 64;
const IA32_GS_BASE: u32 = 0xc000_0101;
const IA32_KERNEL_GS_BASE: u32 = 0xc000_0102;
/// Canonical kernel-half addresses have bit 63 set.
#[cfg(feature = "boot-smoke")]
const KERNEL_HALF: u64 = 0xffff_8000_0000_0000;

/// One CPU's data. Its first four words have fixed offsets that the entry
/// assembly uses through GS. The scheduler's per-CPU state follows them
/// (spec 0051, point 2).
#[repr(C, align(64))]
pub(crate) struct CpuData {
    /// This entry's own address, so assembly can name it through GS.
    self_address: AtomicU64,
    /// The SMP slot, 0 for the BSP.
    slot: AtomicU64,
    /// The suspended scheduler stack while a user thread runs on this CPU:
    /// `mirror_resume_user` stores it through GS and `resume_user` clears it.
    user_kernel_rsp: AtomicU64,
    /// The ID of the thread running in user mode on this CPU, 0 for none.
    current_thread: AtomicU64,
    /// The process and thread table slots that this CPU runs, or whose wait
    /// or request its scheduler pass completes; `NO_SLOT` for none.
    process: AtomicU32,
    thread: AtomicU32,
    /// The process this CPU's scheduler dispatched last, for its switch count.
    last_process: AtomicU32,
    /// Set when an event signals a wait during this CPU's user slice, so the
    /// syscall that signaled returns to the scheduler (0030-D3).
    need_resched: AtomicBool,
    /// A user context is entered on this CPU: from user entry until the
    /// suspended scheduler resumes, its trap handlers included.
    user_active: AtomicBool,
    /// This CPU's scheduler halts for want of an eligible thread.
    idle: AtomicBool,
    /// This CPU holds the scheduler lock. No IPI acknowledgement wait may
    /// start while it is set (spec 0051, open point 3).
    sched_held: AtomicBool,
    installed: AtomicBool,
    /// This CPU's scheduler pass statistics: dispatches that changed process,
    /// and timer ticks spent halted.
    switches: AtomicU64,
    idle_ticks: AtomicU64,
}

/// Marks no process or thread slot in a `CpuData` slot field.
pub(crate) const NO_SLOT: u32 = u32::MAX;

impl CpuData {
    const fn new() -> Self {
        Self {
            self_address: AtomicU64::new(0),
            slot: AtomicU64::new(0),
            user_kernel_rsp: AtomicU64::new(0),
            current_thread: AtomicU64::new(0),
            process: AtomicU32::new(NO_SLOT),
            thread: AtomicU32::new(NO_SLOT),
            last_process: AtomicU32::new(NO_SLOT),
            need_resched: AtomicBool::new(false),
            user_active: AtomicBool::new(false),
            idle: AtomicBool::new(false),
            sched_held: AtomicBool::new(false),
            installed: AtomicBool::new(false),
            switches: AtomicU64::new(0),
            idle_ticks: AtomicU64::new(0),
        }
    }
}

/// Offsets of the fields the entry assembly reads or writes through GS.
pub(crate) const SELF_OFFSET: usize = offset_of!(CpuData, self_address);
pub(crate) const USER_KERNEL_RSP_OFFSET: usize = offset_of!(CpuData, user_kernel_rsp);
const SLOT_OFFSET: usize = offset_of!(CpuData, slot);

const _: () = assert!(SELF_OFFSET == 0);
const _: () = assert!(SLOT_OFFSET == 8);
const _: () = assert!(USER_KERNEL_RSP_OFFSET == 16);
// The entry words and the scheduler state take two cache lines (spec 0051).
const _: () = assert!(size_of::<CpuData>() == 128);

static CPU_DATA: [CpuData; MAX_CPUS] = [const { CpuData::new() }; MAX_CPUS];

/// The address of `slot`'s entry, the value its IA32_GS_BASE holds once
/// installed. Panics for a slot outside the table.
pub(crate) fn address(slot: usize) -> u64 {
    core::ptr::from_ref(&CPU_DATA[slot]) as u64
}

/// The slot whose installed entry `base` names exactly, if any.
fn slot_of(base: u64) -> Option<usize> {
    let offset = base.checked_sub(address(0))?;
    if !offset.is_multiple_of(size_of::<CpuData>() as u64) {
        return None;
    }
    let slot = usize::try_from(offset / size_of::<CpuData>() as u64).ok()?;
    (slot < MAX_CPUS && CPU_DATA[slot].installed.load(Ordering::Acquire)).then_some(slot)
}

fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: Called only at CPL0. Both GS base MSRs exist on every x86-64
    // CPU, and a read changes no processor state.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// # Safety
/// CPL0 with IF clear. `value` is canonical. Writing IA32_GS_BASE changes
/// what every later GS-relative access on this CPU reads, so the caller keeps
/// the D1 roles above; IA32_KERNEL_GS_BASE is scratch in kernel mode.
unsafe fn write_msr(msr: u32, value: u64) {
    // SAFETY: The caller's contract: CPL0, IF clear and a canonical value, so
    // WRMSR cannot fault on either GS base MSR.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags));
    }
}

/// This CPU's IA32_GS_BASE.
pub(crate) fn gs_base() -> u64 {
    read_msr(IA32_GS_BASE)
}

/// This CPU's IA32_KERNEL_GS_BASE.
pub(crate) fn kernel_gs_base() -> u64 {
    read_msr(IA32_KERNEL_GS_BASE)
}

/// Write IA32_KERNEL_GS_BASE, which is scratch in kernel mode, for a self-test.
///
/// # Safety
/// CPL0 with IF clear on an installed CPU, and `value` is canonical. While
/// it differs from this CPU's `CpuData`, nothing may enter user mode or run
/// with IA32_GS_BASE outside the kernel half: the caller restores it first.
#[cfg(feature = "boot-smoke")]
pub(crate) unsafe fn set_kernel_gs_base(value: u64) {
    // SAFETY: The caller's contract; IA32_GS_BASE is unchanged.
    unsafe { write_msr(IA32_KERNEL_GS_BASE, value) };
}

/// Install `slot`'s entry on the executing CPU: IA32_KERNEL_GS_BASE first,
/// then IA32_GS_BASE, so at every instruction either IA32_GS_BASE is still
/// 0 with the scratch MSR naming the entry, or both name it. Returns false,
/// changing nothing, for a slot out of range or already installed.
///
/// # Safety
/// CPL0 with IF clear, on the CPU that owns `slot`, after its descriptor
/// load zeroed both GS MSRs and before anything reads through GS. Called once
/// per slot: the BSP for 0, and each AP for its own claimed slot.
pub(crate) unsafe fn install(slot: usize) -> bool {
    if slot >= MAX_CPUS {
        return false;
    }
    let data = &CPU_DATA[slot];
    if data.installed.swap(true, Ordering::AcqRel) {
        return false;
    }
    let address = address(slot);
    data.self_address.store(address, Ordering::Relaxed);
    data.slot.store(slot as u64, Ordering::Relaxed);
    data.user_kernel_rsp.store(0, Ordering::Relaxed);
    data.current_thread.store(0, Ordering::Relaxed);
    data.process.store(NO_SLOT, Ordering::Relaxed);
    data.thread.store(NO_SLOT, Ordering::Relaxed);
    data.last_process.store(NO_SLOT, Ordering::Relaxed);
    data.need_resched.store(false, Ordering::Relaxed);
    data.user_active.store(false, Ordering::Relaxed);
    data.idle.store(false, Ordering::Relaxed);
    data.sched_held.store(false, Ordering::Relaxed);
    data.switches.store(0, Ordering::Relaxed);
    data.idle_ticks.store(0, Ordering::Relaxed);
    // SAFETY: CPL0 with IF clear per the caller. The entry is a static and so
    // a canonical kernel address. Nothing reads through GS before this, and
    // the order keeps the D1 invariant at every instruction.
    unsafe {
        write_msr(IA32_KERNEL_GS_BASE, address);
        write_msr(IA32_GS_BASE, address);
    }
    true
}

/// Whether the BSP has installed its entry.
pub(crate) fn bsp_installed() -> bool {
    CPU_DATA[0].installed.load(Ordering::Acquire)
}

/// Whether `slot` has installed its entry. Panics for a slot outside the
/// table.
pub(crate) fn is_installed(slot: usize) -> bool {
    CPU_DATA[slot].installed.load(Ordering::Acquire)
}

/// The IA32_GS_BASE value the BSP must hold now: its entry once installed,
/// 0 before (D1 (e)).
pub(crate) fn expected_bsp_gs_base() -> u64 {
    if bsp_installed() { address(0) } else { 0 }
}

/// This CPU's slot from IA32_GS_BASE, without dereferencing GS. None before
/// this CPU installed its entry, when IA32_GS_BASE is still 0; the caller then
/// falls back to the CPUID scan. Exception dispatch and the fatal paths use
/// this because they can run before install.
pub(crate) fn try_current_slot() -> Option<usize> {
    slot_of(gs_base())
}

/// This CPU's slot, read through GS. Only for code that runs after this
/// CPU's install, which is all of it except the early entry and fatal paths
/// (`try_current_slot`).
pub(crate) fn current_cpu() -> usize {
    let slot: u64;
    // SAFETY: After install IA32_GS_BASE names this CPU's entry whenever
    // kernel code runs, so this reads its slot word. The read has no side
    // effects.
    unsafe {
        asm!("mov {}, qword ptr gs:[{offset}]", out(reg) slot, offset = const SLOT_OFFSET,
            options(nostack, readonly, preserves_flags));
    }
    let slot = usize::try_from(slot).expect("per-CPU slot");
    assert!(slot < MAX_CPUS, "per-CPU slot out of range");
    slot
}

/// This CPU's entry, through GS.
fn current() -> &'static CpuData {
    &CPU_DATA[current_cpu()]
}

/// The self address and slot words, both read through GS, for the boot-smoke
/// OP_READ_CPU_DATA probe.
#[cfg(feature = "boot-smoke")]
pub(crate) fn read_through_gs() -> (u64, u64) {
    let address: u64;
    let slot: u64;
    // SAFETY: As in current_cpu; both words belong to this CPU's entry.
    unsafe {
        asm!(
            "mov {address}, qword ptr gs:[{self_offset}]",
            "mov {slot}, qword ptr gs:[{slot_offset}]",
            address = out(reg) address,
            slot = out(reg) slot,
            self_offset = const SELF_OFFSET,
            slot_offset = const SLOT_OFFSET,
            options(nostack, readonly, preserves_flags),
        );
    }
    (address, slot)
}

/// The suspended scheduler stack recorded by `mirror_resume_user`, or 0.
pub(crate) fn user_kernel_rsp() -> u64 {
    current().user_kernel_rsp.load(Ordering::Relaxed)
}

/// Clear the suspended scheduler stack once the user context returned.
pub(crate) fn clear_user_kernel_rsp() {
    current().user_kernel_rsp.store(0, Ordering::Relaxed);
}

/// Record the thread this CPU runs in user mode, 0 for none.
pub(crate) fn set_current_thread(id: u64) {
    current().current_thread.store(id, Ordering::Relaxed);
}

/// The thread this CPU runs in user mode, 0 for none.
#[cfg(feature = "boot-smoke")]
pub(crate) fn current_thread() -> u64 {
    current().current_thread.load(Ordering::Relaxed)
}

/// Record the process and thread slots this CPU runs or completes for, or
/// `None` once it is done with them (spec 0051, point 2).
pub(crate) fn set_running(slots: Option<(u32, u32)>) {
    let (process, thread) = slots.unwrap_or((NO_SLOT, NO_SLOT));
    let data = current();
    data.process.store(process, Ordering::Relaxed);
    data.thread.store(thread, Ordering::Relaxed);
}

/// The process and thread slots this CPU runs or completes for, if any.
pub(crate) fn running() -> Option<(u32, u32)> {
    let data = current();
    let process = data.process.load(Ordering::Relaxed);
    let thread = data.thread.load(Ordering::Relaxed);
    assert_eq!(
        process == NO_SLOT,
        thread == NO_SLOT,
        "per-CPU running slots disagree"
    );
    (process != NO_SLOT).then_some((process, thread))
}

/// Ask this CPU's running syscall to return to the scheduler (0030-D3).
pub(crate) fn set_need_resched() {
    current().need_resched.store(true, Ordering::Relaxed);
}

/// Whether this CPU was asked to reschedule since the last take, clearing it.
pub(crate) fn take_need_resched() -> bool {
    current().need_resched.swap(false, Ordering::Relaxed)
}

/// Set or clear this CPU's user-context flag, returning its previous value.
pub(crate) fn swap_user_active(active: bool) -> bool {
    current().user_active.swap(active, Ordering::Relaxed)
}

/// Whether a user context is entered on this CPU.
pub(crate) fn user_active() -> bool {
    current().user_active.load(Ordering::Relaxed)
}

/// Mark this CPU's scheduler halted for want of work, or running again.
pub(crate) fn set_idle(idle: bool) {
    current().idle.store(idle, Ordering::Relaxed);
}

/// Record whether this CPU holds the scheduler lock. Only the lock calls it.
pub(crate) fn set_sched_held(held: bool) {
    current().sched_held.store(held, Ordering::Relaxed);
}

/// Whether this CPU holds the scheduler lock. Safe before install, when no
/// CPU can hold it: it identifies the CPU from IA32_GS_BASE, without GS.
pub(crate) fn sched_held() -> bool {
    try_current_slot().is_some_and(|slot| CPU_DATA[slot].sched_held.load(Ordering::Relaxed))
}

/// Start this CPU's scheduler pass statistics from zero.
pub(crate) fn reset_pass_stats() {
    let data = current();
    data.switches.store(0, Ordering::Relaxed);
    data.idle_ticks.store(0, Ordering::Relaxed);
    data.last_process.store(NO_SLOT, Ordering::Relaxed);
}

/// Count a dispatch of `process` by this CPU's scheduler, as a switch when
/// it differs from the last one.
pub(crate) fn note_dispatch(process: u32) {
    let data = current();
    if data.last_process.swap(process, Ordering::Relaxed) != process {
        data.switches.fetch_add(1, Ordering::Relaxed);
    }
}

/// Add timer ticks this CPU's scheduler spent halted.
pub(crate) fn add_idle_ticks(ticks: u64) {
    current().idle_ticks.fetch_add(ticks, Ordering::Relaxed);
}

/// This CPU's switches and idle ticks since its last reset.
pub(crate) fn pass_stats() -> (u64, u64) {
    let data = current();
    (
        data.switches.load(Ordering::Relaxed),
        data.idle_ticks.load(Ordering::Relaxed),
    )
}

/// Whether `base` is a kernel-half address, as the paranoid entries test it.
#[cfg(feature = "boot-smoke")]
pub(crate) fn kernel_half(base: u64) -> bool {
    base >= KERNEL_HALF
}

/// Boot-smoke checks of the address arithmetic, before any AP installs.
#[cfg(feature = "boot-smoke")]
pub(crate) fn self_test() {
    assert!(bsp_installed());
    assert_eq!(try_current_slot(), Some(0));
    assert_eq!(current_cpu(), 0);
    assert_eq!(slot_of(address(0)), Some(0));
    assert_eq!(slot_of(0), None);
    let stride = size_of::<CpuData>() as u64;
    assert_eq!(slot_of(address(0) + 8), None);
    assert_eq!(slot_of(address(0) + 64), None);
    assert_eq!(slot_of(address(0) - stride), None);
    assert_eq!(slot_of(address(MAX_CPUS - 1) + stride), None);
    for (slot, data) in CPU_DATA.iter().enumerate().skip(1) {
        assert_eq!(address(slot) - address(slot - 1), stride);
        // No AP has installed yet, and an uninstalled entry never resolves.
        assert!(!data.installed.load(Ordering::Acquire));
        assert_eq!(slot_of(address(slot)), None);
    }
    assert!(kernel_half(address(0)) && !kernel_half(KERNEL_HALF - 1) && !kernel_half(0));
    let (address_word, slot_word) = read_through_gs();
    assert_eq!((address_word, slot_word), (address(0), 0));
}
