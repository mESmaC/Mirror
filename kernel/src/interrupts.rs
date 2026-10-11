//! Per-CPU exception descriptors and BSP synchronous ring-3 entry/return delivery.
//!
//! Descriptors, TLS bases and frames follow Intel SDM vol. 3A, §§3.4.4, 6.14, 7.7:
//! https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
//! Newer error-code exceptions (#CP/#VC/#SX) also follow AMD APM vol. 2, chapter 8:
//! https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2

use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of};
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "boot-smoke")]
use core::sync::atomic::{AtomicU64, AtomicUsize};

use crate::{arch, fpu, serial, stacks::IST_COUNT, timer};

global_asm!(
    include_str!("interrupts.S"),
    tls_prefix = const offset_of!(TrapFrame, r15),
    fs_base = const offset_of!(TrapFrame, fs_base),
    gs_base = const offset_of!(TrapFrame, gs_base),
    fs_selector = const offset_of!(TrapFrame, fs_selector),
    gs_selector = const offset_of!(TrapFrame, gs_selector),
);
global_asm!(
    include_str!("userspace_entry.S"),
    kernel_data = const DATA_SELECTOR,
    user_data = const USER_DATA_SELECTOR,
    frame_size = const size_of::<TrapFrame>(),
    frame_words = const size_of::<TrapFrame>() / size_of::<u64>(),
);

const CODE_SELECTOR: u16 = 0x08;
const DATA_SELECTOR: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x18;
const USER_DATA_SELECTOR: u16 = 0x2b;
const USER_CODE_SELECTOR: u16 = 0x33;
// CF/PF/AF/ZF/SF/TF/DF/OF/RF/AC/ID. Force IF and bit 1 separately; never
// restore IOPL, NT, VM, VIF, VIP, or reserved bits from a saved user context.
const USER_RFLAGS_MASK: u64 = 0x25_0dd5;
const DOUBLE_FAULT_IST: u8 = 1;
const NMI_IST: u8 = 2;
const MACHINE_CHECK_IST: u8 = 3;
const EARLY_IST_STACK_SIZE: usize = 16 * 1024;
const CR4_MCE: u64 = 1 << 6;
const MAX_CPUS: usize = 64;
const IPI_VECTOR: usize = 240;
const LOCAL_TIMER_VECTOR: usize = 241;
const APIC_SPURIOUS_VECTOR: usize = 255;

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

#[repr(C, packed)]
struct TaskStateSegment {
    reserved0: u32,
    privilege_stacks: [u64; 3],
    reserved1: u64,
    interrupt_stacks: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    io_map_offset: u16,
}

impl TaskStateSegment {
    const EMPTY: Self = Self {
        reserved0: 0,
        // set_kernel_stack installs RSP0 before any ring-3 transition.
        privilege_stacks: [0; 3],
        reserved1: 0,
        interrupt_stacks: [0; 7],
        reserved2: 0,
        reserved3: 0,
        // Beyond the descriptor limit: no I/O permission bitmap is present.
        io_map_offset: size_of::<Self>() as u16,
    };
}

#[repr(C, align(16))]
struct ExceptionStack([u8; EARLY_IST_STACK_SIZE]);

#[derive(Clone, Copy)]
#[repr(C)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const MISSING: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_middle: 0,
        offset_high: 0,
        reserved: 0,
    };

    fn interrupt(address: u64, ist: u8) -> Self {
        Self {
            offset_low: address as u16,
            selector: CODE_SELECTOR,
            ist,
            attributes: 0x8e, // Present, DPL0, 64-bit interrupt gate (clears IF).
            offset_middle: (address >> 16) as u16,
            offset_high: (address >> 32) as u32,
            reserved: 0,
        }
    }
}

// Match interrupts.S exactly. Long mode always saves SS:RSP, including CPL0
// entries without an IST switch. The common stub saves every integer register
// and both visible selectors and actual hidden TLS bases before clearing them.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct TrapFrame {
    pub fs_base: u64,
    pub gs_base: u64,
    pub fs_selector: u64,
    pub gs_selector: u64,
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

const _: () = assert!(size_of::<DescriptorPointer>() == 10);
const _: () = assert!(size_of::<TaskStateSegment>() == 104);
const _: () = assert!(offset_of!(TaskStateSegment, interrupt_stacks) == 36);
const _: () = assert!(offset_of!(TaskStateSegment, io_map_offset) == 102);
const _: () = assert!(size_of::<IdtEntry>() == 16);
const _: () = assert!(size_of::<TrapFrame>() == 26 * 8);
const _: () = assert!(offset_of!(TrapFrame, fs_base) == 0);
const _: () = assert!(offset_of!(TrapFrame, gs_base) == 8);
const _: () = assert!(offset_of!(TrapFrame, fs_selector) == 16);
const _: () = assert!(offset_of!(TrapFrame, gs_selector) == 24);
const _: () = assert!(offset_of!(TrapFrame, r15) == 4 * 8);
const _: () = assert!(offset_of!(TrapFrame, vector) == 19 * 8);
const _: () = assert!(offset_of!(TrapFrame, error) == 20 * 8);
const _: () = assert!(offset_of!(TrapFrame, rip) == 21 * 8);
const _: () = assert!(offset_of!(TrapFrame, cs) == 22 * 8);
const _: () = assert!(offset_of!(TrapFrame, rflags) == 23 * 8);
const _: () = assert!(offset_of!(TrapFrame, rsp) == 24 * 8);
const _: () = assert!(offset_of!(TrapFrame, ss) == 25 * 8);

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut TSS: TaskStateSegment = TaskStateSegment::EMPTY;
// The BSP's IST1-3 share this stack during early boot, until install_bsp_ists
// moves them to their guarded stacks. Afterwards it stays idle.
static mut EARLY_IST_STACK: ExceptionStack = ExceptionStack([0; EARLY_IST_STACK_SIZE]);
static mut GDT: [u64; 7] = [0; 7];
static mut IDT: [IdtEntry; 256] = [IdtEntry::MISSING; 256];

// Each secondary owns one element exclusively after claiming its slot. The
// writable GDT cannot be shared: LTR marks the local TSS descriptor busy. Only
// the IDT is shared, and it is immutable after INITIALIZED publishes it.
static SECONDARY_CLAIMED: [AtomicBool; MAX_CPUS - 1] =
    [const { AtomicBool::new(false) }; MAX_CPUS - 1];
static mut SECONDARY_TSS: [TaskStateSegment; MAX_CPUS - 1] =
    [const { TaskStateSegment::EMPTY }; MAX_CPUS - 1];
static mut SECONDARY_GDT: [[u64; 7]; MAX_CPUS - 1] = [[0; 7]; MAX_CPUS - 1];

static USER_ACTIVE: AtomicBool = AtomicBool::new(false);

// Assembly saves the BSP's suspended kernel stack here, with IF clear. User
// pages cannot access this supervisor-only kernel object. APs do not execute
// userspace yet. The BSP scheduler resumes after each yield or preemption;
// it retains user trap frames rather than suspended Rust handler stacks.
#[unsafe(no_mangle)]
static mut MIRROR_USER_KERNEL_RSP: u64 = 0;

#[cfg(feature = "boot-smoke")]
static BREAKPOINT_ARMED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "boot-smoke")]
static BREAKPOINT_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "boot-smoke")]
static EXPECTED_PAGE_FAULT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "boot-smoke")]
static PAGE_FAULT_ARMED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "boot-smoke")]
static APIC_SPURIOUS_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" {
    static mirror_exception_entries: [u64; 48];
    fn mirror_exception_128();
    fn mirror_exception_240();
    fn mirror_exception_241();
    fn mirror_exception_255();
    fn mirror_resume_user(frame: *const TrapFrame, extended: *mut fpu::State);
    fn mirror_resume_kernel();
}

pub(crate) fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

/// Report the TLS execution contract after fpu::init disabled FSGSBASE. Only
/// the syscall ABI may install arbitrary bases; selector loads remain legal.
pub(crate) fn tls_ready() {
    assert!(fpu::is_initialized(), "TLS setup requires CPU state setup");
    assert!(!arch::interrupts_enabled(), "TLS setup requires IF=0");
    assert_kernel_tls_clean();
    let cr4: u64;
    // SAFETY: CPL0. This only reads CR4; no feature is enabled here.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    // Check the maximum basic leaf before reading structured feature flags.
    let supports_fsgsbase = core::arch::x86_64::__cpuid(0).eax >= 7
        && core::arch::x86_64::__cpuid_count(7, 0).ebx & 1 != 0;
    assert_eq!(cr4 & (1 << 16), 0, "userspace FSGSBASE must stay disabled");
    serial::println!(
        "MIRROR_TLS_READY cpuid_fsgsbase={} fsgsbase=0 selectors=1",
        u8::from(supports_fsgsbase)
    );
}

/// Every user-accessible data/readable-code descriptor can be loaded with
/// any RPL: CPL3 supplies the effective privilege check. Null selectors 0..3
/// are also legal. Reject every other selector before the CPL0 restore MOV.
pub(crate) fn valid_user_tls(frame: &TrapFrame) -> bool {
    let valid_selector = |selector: u64| {
        selector <= 3
            || selector & !3 == u64::from(USER_DATA_SELECTOR & !3)
            || selector & !3 == u64::from(USER_CODE_SELECTOR & !3)
    };
    frame.fs_base < (1_u64 << 47)
        && frame.gs_base < (1_u64 << 47)
        && valid_selector(frame.fs_selector)
        && valid_selector(frame.gs_selector)
}

/// Kernel Rust must never inherit user TLS. Reading the actual MSRs also
/// catches selector loads that changed a base without a TLS syscall.
fn assert_kernel_tls_clean() {
    assert!(kernel_tls_clean(), "kernel FS/GS state is not clean");
}

fn kernel_tls_clean() -> bool {
    let fs_selector: u16;
    let gs_selector: u16;
    let fs_low: u32;
    let fs_high: u32;
    let gs_low: u32;
    let gs_high: u32;
    // SAFETY: Called only at CPL0. These MSRs exist in x86-64 and reads do not
    // alter processor state. No FS/GS-relative access is made by kernel Rust.
    unsafe {
        asm!("mov {:x}, fs", out(reg) fs_selector,
            options(nomem, nostack, preserves_flags));
        asm!("mov {:x}, gs", out(reg) gs_selector,
            options(nomem, nostack, preserves_flags));
        asm!("rdmsr", in("ecx") 0xc000_0100_u32,
            out("eax") fs_low, out("edx") fs_high,
            options(nomem, nostack, preserves_flags));
        asm!("rdmsr", in("ecx") 0xc000_0101_u32,
            out("eax") gs_low, out("edx") gs_high,
            options(nomem, nostack, preserves_flags));
    }
    fs_selector == 0 && gs_selector == 0 && (fs_low | fs_high | gs_low | gs_high) == 0
}

/// Install the boot CPU's GDT, TSS and exception/IRQ IDT once at CPL0, then
/// set CR4.MCE when CPUID reports machine-check support, panicking if CR4 does
/// not read back as expected. All descriptor storage and the early IST stack
/// must remain mapped.
/// Secondary CPUs load this shared IDT only after initialization is published.
pub fn init() {
    arch::disable_interrupts();
    assert!(!is_initialized(), "IDT already loaded");

    let tss_address = addr_of!(TSS) as u64;
    let stack_top = addr_of!(EARLY_IST_STACK) as u64 + EARLY_IST_STACK_SIZE as u64;

    // SAFETY: Only the boot CPU runs, IF is clear, and init is single-use. These
    // static objects have not been installed yet and no references alias writes.
    unsafe {
        addr_of_mut!(TSS).write(TaskStateSegment {
            interrupt_stacks: [stack_top, stack_top, stack_top, 0, 0, 0, 0],
            ..TaskStateSegment::EMPTY
        });
        addr_of_mut!(GDT).write(gdt_entries(tss_address));
        let handlers = addr_of!(mirror_exception_entries).cast::<u64>();
        let entries = addr_of_mut!(IDT).cast::<IdtEntry>();
        for vector in 0..48 {
            // NMI, #DF and #MC never run on the stack they interrupted.
            let ist = match vector {
                2 => NMI_IST,
                8 => DOUBLE_FAULT_IST,
                18 => MACHINE_CHECK_IST,
                _ => 0,
            };
            entries
                .add(vector)
                .write(IdtEntry::interrupt(handlers.add(vector).read(), ist));
        }
        let mut syscall = IdtEntry::interrupt(mirror_exception_128 as *const () as u64, 0);
        syscall.attributes |= 3 << 5; // CPL3 may invoke INT 0x80.
        entries.add(0x80).write(syscall);
        entries.add(IPI_VECTOR).write(IdtEntry::interrupt(
            mirror_exception_240 as *const () as u64,
            0,
        ));
        // Hardware may interrupt CPL3, but INT 241 must remain privileged so
        // userspace cannot manufacture scheduler ticks or LAPIC EOIs.
        entries.add(LOCAL_TIMER_VECTOR).write(IdtEntry::interrupt(
            mirror_exception_241 as *const () as u64,
            0,
        ));
        // Hardware may deliver a spurious LAPIC interrupt at either CPL, but
        // userspace must not invoke this vector with the INT instruction.
        entries.add(APIC_SPURIOUS_VECTOR).write(IdtEntry::interrupt(
            mirror_exception_255 as *const () as u64,
            0,
        ));
    }

    let gdtr = DescriptorPointer {
        limit: (size_of::<[u64; 7]>() - 1) as u16,
        base: addr_of!(GDT) as u64,
    };
    let idtr = DescriptorPointer {
        limit: (size_of::<[IdtEntry; 256]>() - 1) as u16,
        base: addr_of!(IDT) as u64,
    };

    // SAFETY: The BSP exclusively initialized these permanently mapped tables
    // with IF clear, and no secondary may load the IDT until publication below.
    unsafe { load_descriptors(&gdtr, &idtr) };
    // SAFETY: The BSP runs at CPL0 with IF clear and has just loaded the IDT,
    // whose #MC gate uses IST3 on the mapped early IST stack.
    let machine_check = unsafe { enable_machine_check() };
    assert!(machine_check, "CR4.MCE does not match CPUID");
    INITIALIZED.store(true, Ordering::Release);
    serial::println!("MIRROR_IDT_READY exceptions=32 double_fault_ist=1 irqs=16");
}

fn gdt_entries(tss_address: u64) -> [u64; 7] {
    let tss_limit = (size_of::<TaskStateSegment>() - 1) as u64;
    let tss_low = tss_limit
        | ((tss_address & 0xffff) << 16)
        | (((tss_address >> 16) & 0xff) << 32)
        | (0x89_u64 << 40) // Present, available 64-bit TSS.
        | (((tss_address >> 24) & 0xff) << 56);
    [
        0,
        0x00af_9b00_0000_ffff, // Accessed, executable/readable 64-bit code.
        0x00cf_9300_0000_ffff, // Accessed, writable data; L bit clear.
        tss_low,
        tss_address >> 32,
        0x00cf_f300_0000_ffff, // DPL3 accessed, writable data.
        0x00af_fb00_0000_ffff, // DPL3 accessed, 64-bit code.
    ]
}

/// Set CR4.MCE (bit 6) when CPUID 1 EDX[7] reports machine-check support, so
/// a machine check reaches the #MC gate instead of shutting the CPU down. CR4
/// is read back: returns whether its MCE bit equals the CPUID bit and every
/// other bit kept its value. Reports only; the caller decides how to fail.
///
/// # Safety
/// The caller runs at CPL0 with IF clear and has loaded the shared IDT on this
/// CPU, so a machine check from here on is delivered through vector 18.
unsafe fn enable_machine_check() -> bool {
    let supported = core::arch::x86_64::__cpuid(1).edx & (1 << 7) != 0;
    let before: u64;
    // SAFETY: CPL0 per the caller; reading CR4 changes no processor state.
    unsafe { asm!("mov {}, cr4", out(reg) before, options(nomem, nostack, preserves_flags)) };
    let expected = if supported { before | CR4_MCE } else { before };
    if expected != before {
        // SAFETY: CPL0 with IF clear per the caller. CPUID reports MCE, so
        // bit 6 is defined and setting it cannot fault; every other bit keeps
        // the value just read. The IDT is loaded, so a machine check from now
        // on reaches its handler instead of shutting the CPU down.
        unsafe { asm!("mov cr4, {}", in(reg) expected, options(nostack, preserves_flags)) };
    }
    let after: u64;
    // SAFETY: As for the first read.
    unsafe { asm!("mov {}, cr4", out(reg) after, options(nomem, nostack, preserves_flags)) };
    after == expected && (after & CR4_MCE != 0) == supported
}

/// The caller owns this CPU's writable GDT/TSS with IF clear, and both tables
/// remain mapped. CS/data/TSS selectors must use the layout from gdt_entries.
unsafe fn load_descriptors(gdtr: &DescriptorPointer, idtr: &DescriptorPointer) {
    // SAFETY: CPL0, interrupts disabled, persistent valid descriptors. The far
    // return reloads CS with balanced stack use; data selectors are replaced
    // before LTR. Kernel Rust uses zero FS/GS bases and selectors. LTR marks
    // our writable TSS descriptor busy. No ring3 transitions or IRQs are enabled
    // here; APIC/timer setup configures routing before the caller may set IF.
    unsafe {
        asm!(
            "lgdt [{gdt}]",
            "push {code}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            "mov ax, {data}",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            "xor eax, eax",
            "mov fs, ax",
            "mov gs, ax",
            // A null selector can retain the bootloader's hidden base.
            "xor edx, edx",
            "mov ecx, 0xc0000100",
            "wrmsr",
            "inc ecx",
            "wrmsr",
            "mov ax, {tss}",
            "ltr ax",
            "lidt [{idt}]",
            gdt = in(reg) gdtr,
            idt = in(reg) idtr,
            code = const CODE_SELECTOR,
            data = const DATA_SELECTOR,
            tss = const TSS_SELECTOR,
            out("rax") _,
            out("rcx") _,
            out("rdx") _,
        );
    }
}

/// Install and verify a secondary CPU's private GDT, TSS, RSP0 and IST1-3,
/// then set CR4.MCE when CPUID reports it and verify CR4 by readback.
/// Failures are returned without logging or entering the BSP panic path.
///
/// # Safety
/// The caller runs at CPL0 on the identified secondary with IF clear, exactly
/// once for a slot in 1..64. The BSP has completed init. `kernel_stack_top` is
/// immediately above this CPU's future private supervisor-writable stack, and
/// `ist_tops` are immediately above its own private IST1 (#DF), IST2 (NMI) and
/// IST3 (#MC) stacks. This routine only stores those addresses in the TSS; it
/// may run on Limine's reserved AP bootstrap stack and root, before the private
/// stacks are mapped, and an exception through an IST gate in that window
/// triple-faults the CPU. The current stack and all descriptor storage must
/// remain mapped throughout this call. Before setting IF or entering CPL3,
/// activate a root that maps the private stacks and switch to the kernel stack.
/// The bootstrap stack remains reserved until its last use has ended.
/// The secondary must not enter userspace or call BSP-only interrupt routines.
pub(crate) unsafe fn init_secondary(
    slot: usize,
    kernel_stack_top: u64,
    ist_tops: [u64; IST_COUNT],
) -> bool {
    if !(1..MAX_CPUS).contains(&slot)
        || !INITIALIZED.load(Ordering::Acquire)
        || arch::interrupts_enabled()
        || kernel_stack_top == 0
        || !kernel_stack_top.is_multiple_of(16)
        || ((1_u64 << 47)..0xffff_8000_0000_0000).contains(&kernel_stack_top)
        || ist_tops
            .iter()
            .any(|&top| top < 0xffff_8000_0000_0000 || !top.is_multiple_of(16))
    {
        return false;
    }
    let index = slot - 1;
    if SECONDARY_CLAIMED[index].swap(true, Ordering::AcqRel) {
        return false;
    }

    // SAFETY: The checked slot's atomic claim grants exclusive access to these
    // distinct objects, including while other CPUs load their own descriptors.
    // No whole-array reference is formed. All arrays have permanent mappings.
    let (tss, gdt) = unsafe {
        (
            addr_of_mut!(SECONDARY_TSS)
                .cast::<TaskStateSegment>()
                .add(index),
            addr_of_mut!(SECONDARY_GDT).cast::<[u64; 7]>().add(index),
        )
    };
    let expected_gdt = gdt_entries(tss as u64);
    // IST1-3 hold the caller's tops; IST4-7 stay unused.
    let mut expected_ists = [0; 7];
    expected_ists[..IST_COUNT].copy_from_slice(&ist_tops);
    // SAFETY: This CPU exclusively owns these uninstalled descriptor objects.
    // The caller supplies this CPU's private kernel and IST stacks, distinct
    // from every other CPU's; hardware reads these fields only on CPL3 entry
    // or on delivery through an IST gate.
    unsafe {
        tss.write(TaskStateSegment {
            privilege_stacks: [kernel_stack_top, 0, 0],
            interrupt_stacks: expected_ists,
            ..TaskStateSegment::EMPTY
        });
        gdt.write(expected_gdt);
    }
    let gdtr = DescriptorPointer {
        limit: (size_of::<[u64; 7]>() - 1) as u16,
        base: gdt as u64,
    };
    let idtr = DescriptorPointer {
        limit: (size_of::<[IdtEntry; 256]>() - 1) as u16,
        base: addr_of!(IDT) as u64,
    };
    // SAFETY: The private GDT/TSS are initialized and writable, the shared IDT
    // was acquired after BSP publication, and IF remains clear on this CPU.
    unsafe { load_descriptors(&gdtr, &idtr) };
    // SAFETY: This AP runs at CPL0 with IF clear and has just loaded the
    // shared IDT. Until the caller activates the root that maps IST3, a
    // machine check triple-faults, as it would with CR4.MCE clear.
    let machine_check = unsafe { enable_machine_check() };

    let mut actual_gdt = DescriptorPointer { limit: 0, base: 0 };
    let mut actual_idt = DescriptorPointer { limit: 0, base: 0 };
    let task: u16;
    let code: u16;
    let data: u16;
    let extra: u16;
    let stack: u16;
    // SAFETY: CPL0 readback of the installed CPU-local architectural state.
    // These instructions do not change tables or selectors. Pointer operands
    // refer to writable local storage and the outputs are independent registers.
    unsafe {
        asm!(
            "sgdt [{gdt}]",
            "sidt [{idt}]",
            "str {task:x}",
            "mov {code:x}, cs",
            "mov {data:x}, ds",
            "mov {extra:x}, es",
            "mov {stack:x}, ss",
            gdt = in(reg) &mut actual_gdt,
            idt = in(reg) &mut actual_idt,
            task = out(reg) task,
            code = out(reg) code,
            data = out(reg) data,
            extra = out(reg) extra,
            stack = out(reg) stack,
            options(nostack, preserves_flags),
        );
    }
    // SAFETY: The current CPU owns these objects, and hardware only changed
    // the TSS descriptor's busy bit during LTR. Packed TSS fields need unaligned
    // accesses; volatile GDT reads observe the processor's descriptor update.
    let storage_valid = unsafe {
        gdt.cast::<u64>().add(3).read_volatile() == (expected_gdt[3] | (1 << 41))
            && gdt.cast::<u64>().add(4).read_volatile() == expected_gdt[4]
            && addr_of!((*tss).privilege_stacks)
                .cast::<u64>()
                .read_unaligned()
                == kernel_stack_top
            && addr_of!((*tss).interrupt_stacks).read_unaligned() == expected_ists
    };
    actual_gdt.limit == gdtr.limit
        && actual_gdt.base == gdtr.base
        && actual_idt.limit == idtr.limit
        && actual_idt.base == idtr.base
        && task == TSS_SELECTOR
        && code == CODE_SELECTOR
        && data == DATA_SELECTOR
        && extra == DATA_SELECTOR
        && stack == DATA_SELECTOR
        && storage_valid
        && machine_check
        && kernel_tls_clean()
}

/// Point the BSP's IST1-3 at their guarded stacks, replacing the early static
/// stack, and verify each TSS field by reading it back.
///
/// # Safety
/// Only the BSP calls this, once, with IF clear, after `init`. Each top is
/// 16-byte aligned and immediately above its own mapped, writable,
/// supervisor-only stack, which stays mapped in every address space.
pub(crate) unsafe fn install_bsp_ists(tops: [u64; IST_COUNT]) {
    assert!(is_initialized(), "IDT not initialized");
    assert!(!arch::interrupts_enabled(), "IST update requires IF=0");
    for top in tops {
        assert!(
            top >= 0xffff_8000_0000_0000 && top.is_multiple_of(16),
            "invalid IST stack"
        );
    }
    // SAFETY: The caller supplies exclusive BSP access with IF clear. TSS is
    // packed, so its IST fields need unaligned raw-pointer accesses, and no
    // reference is formed. Hardware reads a field only to deliver an
    // exception through a gate that names it, and the old stack stays mapped.
    let fields = unsafe { addr_of_mut!(TSS.interrupt_stacks).cast::<u64>() };
    for (index, top) in tops.into_iter().enumerate() {
        // SAFETY: Index 0..3 stays inside the seven IST fields; see above.
        unsafe { fields.add(index).write_unaligned(top) };
    }
    for (index, top) in tops.into_iter().enumerate() {
        // SAFETY: The same in-bounds field. A volatile read of a byte array
        // needs no alignment and makes this an actual load from the TSS.
        let actual = unsafe { fields.add(index).cast::<[u8; 8]>().read_volatile() };
        assert_eq!(u64::from_ne_bytes(actual), top, "BSP IST readback");
    }
}

/// Set the dedicated kernel stack used by hardware on CPL3 -> CPL0 entry.
///
/// # Safety
/// Only the BSP may call, with interrupts disabled and no active user context.
/// `top` must be 16-byte aligned and immediately above a writable, supervisor
/// stack large enough for hardware frames, handlers, and nested exceptions.
/// The stack must stay mapped in every address space until replaced.
pub(crate) unsafe fn set_kernel_stack(top: u64) {
    assert!(is_initialized(), "IDT not initialized");
    assert!(!arch::interrupts_enabled(), "RSP0 update requires IF=0");
    assert!(
        !USER_ACTIVE.load(Ordering::SeqCst),
        "user context is active"
    );
    assert!(top != 0 && top.is_multiple_of(16), "invalid RSP0");
    // SAFETY: Caller supplies exclusive BSP access and a persistent stack.
    // TSS is packed, so the RSP0 field requires an unaligned raw-pointer write.
    unsafe {
        addr_of_mut!(TSS.privilege_stacks)
            .cast::<u64>()
            .write_unaligned(top);
    }
}

/// Construct a fresh integer register context with no inherited registers.
/// RDI contains the explicit argument, IF=1, and IOPL=0. The caller must map the
/// image and stack; resume_user checks canonical addresses before CPU entry.
pub(crate) fn user_frame(entry: u64, stack: u64, argument: u64) -> TrapFrame {
    TrapFrame {
        fs_base: 0,
        gs_base: 0,
        fs_selector: 0,
        gs_selector: 0,
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rbp: 0,
        rdi: argument,
        rsi: 0,
        rdx: 0,
        rcx: 0,
        rbx: 0,
        rax: 0,
        vector: 0,
        error: 0,
        rip: entry,
        cs: u64::from(USER_CODE_SELECTOR),
        rflags: 0x202,
        rsp: stack,
        ss: u64::from(USER_DATA_SELECTOR),
    }
}

/// Resume saved integer and x87/SIMD registers until a handler requests return.
/// User status flags are preserved, IF is forced on, and privilege bits are
/// cleared. FS/GS selectors and bases are restored independently; a fresh
/// context starts with all four zero. XSAVE/AVX remain unsupported.
/// Returns on the original kernel stack with IF=0, callee-saved GPRs intact,
/// and the extended state saved before restoring clean kernel register state.
///
/// # Safety
/// Only the BSP may call; interrupts must be disabled. The active page tables
/// must isolate user mappings from kernel mappings and belong to this context.
/// The frame must contain the intended user's state; set_kernel_stack must have
/// installed a mapped kernel stack. IRQ handlers and scheduling state must be
/// initialized. No interrupt-disabling guard may span this call: user mode
/// deliberately sets IF. The saved frame and kernel caller remain mapped.
/// `extended` must identify this user's aligned, initialized, supervisor-only
/// State and remain writable/mapped until return. No other reference may access
/// it while assembly restores/saves it. A raw pointer avoids retaining a Rust
/// field borrow while handlers access the surrounding process independently.
pub(crate) unsafe fn resume_user(frame: &TrapFrame, extended: *mut fpu::State) {
    assert!(is_initialized(), "IDT not initialized");
    assert!(fpu::is_initialized(), "FPU not initialized");
    assert!(!arch::interrupts_enabled(), "user entry requires IF=0");
    assert!(!extended.is_null(), "user entry requires extended state");
    assert!(
        (extended as usize).is_multiple_of(16),
        "unaligned extended state"
    );
    assert_eq!(frame.cs, u64::from(USER_CODE_SELECTOR), "invalid user CS");
    assert_eq!(frame.ss, u64::from(USER_DATA_SELECTOR), "invalid user SS");
    assert!(valid_user_tls(frame), "invalid user TLS state");
    assert!(
        (4096..(1 << 47)).contains(&frame.rip),
        "invalid user instruction pointer"
    );
    assert!(
        (4096..(1 << 47)).contains(&frame.rsp),
        "invalid user stack pointer"
    );
    let mut prepared = *frame;
    prepared.rflags = (prepared.rflags & USER_RFLAGS_MASK) | 0x202;
    // SAFETY: Exclusive BSP call with IF clear; no mutable reference is formed.
    let rsp0 = unsafe {
        addr_of!(TSS.privilege_stacks)
            .cast::<u64>()
            .read_unaligned()
    };
    assert_ne!(rsp0, 0, "user entry requires a kernel stack");
    assert!(
        !USER_ACTIVE.swap(true, Ordering::SeqCst),
        "nested user entry"
    );
    // SAFETY: Caller guarantees this context's address space and kernel stacks.
    // The validated copy has only CPL3 selectors and unprivileged flags. Assembly
    // copies it before restoring registers, preserves kernel callee-saved GPRs,
    // and resumes here only through the trusted return trampoline. The caller
    // provides a trusted, mapped state image with valid MXCSR reserved bits.
    unsafe { mirror_resume_user(&prepared, extended) };
    assert!(!arch::interrupts_enabled(), "user return requires IF=0");
    #[cfg(feature = "boot-smoke")]
    assert_kernel_tls_clean();
    // SAFETY: Assembly has returned to the saved stack with IF clear.
    unsafe { addr_of_mut!(MIRROR_USER_KERNEL_RSP).write(0) };
    assert!(USER_ACTIVE.swap(false, Ordering::SeqCst));
}

/// Request a return to the suspended kernel caller after this handler unwinds.
/// Never long-jumps through live Rust frames or interrupt/allocator guards.
pub(crate) fn return_from_user(frame: &mut TrapFrame) {
    assert_eq!(frame.cs & 3, 3, "kernel return requires a user frame");
    assert!(!arch::interrupts_enabled(), "user handler requires IF=0");
    assert!(USER_ACTIVE.load(Ordering::SeqCst), "no active user context");
    // SAFETY: Only the BSP runs, and IF is clear. The entry assembly published
    // this trusted supervisor-only stack pointer before enabling user execution.
    let kernel_rsp = unsafe { addr_of!(MIRROR_USER_KERNEL_RSP).read() };
    assert_ne!(kernel_rsp, 0, "no suspended kernel stack");
    frame.rip = mirror_resume_kernel as *const () as u64;
    frame.cs = u64::from(CODE_SELECTOR);
    frame.rflags = 2; // Reserved bit 1 only: IF/DF/TF/AC/IOPL/NT are all clear.
    frame.rsp = kernel_rsp;
    frame.ss = u64::from(DATA_SELECTOR);
    // suspend() already copied the original user state, when it is resumable.
    // The common epilogue must install clean bases before the kernel trampoline.
    frame.fs_base = 0;
    frame.gs_base = 0;
    frame.fs_selector = 0;
    frame.gs_selector = 0;
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    assert!(INITIALIZED.load(Ordering::SeqCst), "IDT not initialized");
    let before = BREAKPOINT_COUNT.load(Ordering::SeqCst);
    assert!(!BREAKPOINT_ARMED.swap(true, Ordering::SeqCst));
    let restored_rax: u64;
    let restored_r10: u64;
    let restored_flags: u64;
    let restored_fs_selector: u64;
    let restored_gs_selector: u64;
    let restored_fs_base: u64;
    let restored_gs_base: u64;
    // SAFETY: Vector3 is installed and armed for a returning test. Assembly
    // installs valid selectors and canonical sentinel bases without using
    // them for memory accesses. Entry clears DF/TLS before Rust; return must
    // restore all four TLS fields even for CPL0. Capture the results and clear
    // DF/TLS again before Rust resumes, while checking two volatile GPRs.
    unsafe {
        asm!(
            "push rax",
            "mov ax, {user_data}",
            "mov fs, ax",
            "mov ax, {user_code}",
            "mov gs, ax",
            "mov ecx, 0xc0000100",
            "mov eax, 0x56789000",
            "mov edx, 0x1234",
            "wrmsr",
            "inc ecx",
            "mov eax, 0x6789a000",
            "mov edx, 0x2345",
            "wrmsr",
            "pop rax",
            "std",
            "int3",
            "pushfq",
            "pop r11",
            "cld",
            "push rax",
            "xor r9d, r9d",
            "mov r9w, fs",
            "xor r8d, r8d",
            "mov r8w, gs",
            "mov ecx, 0xc0000100",
            "rdmsr",
            "shl rdx, 32",
            "or rax, rdx",
            "mov rsi, rax",
            "inc ecx",
            "rdmsr",
            "shl rdx, 32",
            "or rax, rdx",
            "mov rdi, rax",
            "xor eax, eax",
            "mov fs, ax",
            "mov gs, ax",
            "xor edx, edx",
            "mov ecx, 0xc0000100",
            "wrmsr",
            "inc ecx",
            "wrmsr",
            "pop rax",
            user_data = const USER_DATA_SELECTOR,
            user_code = const USER_CODE_SELECTOR,
            inout("rax") 0x1234_5678_9abc_def0_u64 => restored_rax,
            inout("r10") 0xfedc_ba98_7654_3210_u64 => restored_r10,
            lateout("r11") restored_flags,
            lateout("r9") restored_fs_selector,
            lateout("r8") restored_gs_selector,
            lateout("rsi") restored_fs_base,
            lateout("rdi") restored_gs_base,
            out("rcx") _,
            out("rdx") _,
        );
    }
    assert_eq!(BREAKPOINT_COUNT.load(Ordering::SeqCst), before + 1);
    assert!(!BREAKPOINT_ARMED.load(Ordering::SeqCst));
    assert_eq!(restored_rax, 0x1234_5678_9abc_def0);
    assert_eq!(restored_r10, 0xfedc_ba98_7654_3210);
    assert_ne!(restored_flags & (1 << 10), 0, "IRET did not restore DF");
    assert_eq!(restored_fs_selector, u64::from(USER_DATA_SELECTOR));
    assert_eq!(restored_gs_selector, u64::from(USER_CODE_SELECTOR));
    assert_eq!(restored_fs_base, 0x1234_5678_9000);
    assert_eq!(restored_gs_base, 0x2345_6789_a000);
    assert_kernel_tls_clean();
    serial::println!("MIRROR_EXCEPTIONS_OK");
    serial::println!("MIRROR_TLS_KERNEL_OK selectors=1 bases=1 clean=1");
    if crate::apic::is_enabled() {
        spurious_self_test();
    }
}

/// The spurious gate uses the shared entry/return path, but consumes neither
/// a synchronous-exception expectation nor an interrupt-controller EOI.
#[cfg(feature = "boot-smoke")]
fn spurious_self_test() {
    assert!(!arch::interrupts_enabled());
    let before = APIC_SPURIOUS_COUNT.load(Ordering::Relaxed);
    let restored_rax: u64;
    let restored_r10: u64;
    let restored_flags: u64;
    // SAFETY: The installed DPL0 vector returns without touching APIC state.
    // The common entry clears DF before Rust and IRET must restore it. Clear
    // DF again before returning to compiler-generated code; IF remains clear.
    unsafe {
        asm!(
            "std",
            "int 0xff",
            "pushfq",
            "pop r11",
            "cld",
            inout("rax") 0x1234_5678_9abc_def0_u64 => restored_rax,
            inout("r10") 0xfedc_ba98_7654_3210_u64 => restored_r10,
            lateout("r11") restored_flags,
        );
    }
    assert_eq!(APIC_SPURIOUS_COUNT.load(Ordering::Relaxed), before + 1);
    assert_eq!(restored_rax, 0x1234_5678_9abc_def0);
    assert_eq!(restored_r10, 0xfedc_ba98_7654_3210);
    assert_ne!(restored_flags & (1 << 10), 0, "spurious return lost DF");
    assert_eq!(restored_flags & (1 << 9), 0, "spurious return changed IF");
    assert_kernel_tls_clean();
    serial::println!("MIRROR_APIC_SPURIOUS_OK vector=255 registers=1 flags=1");
}

/// The next exception must be a CPL0 read from this unmapped address. Its
/// hardware page-fault error code must be exactly zero. Success exits QEMU.
#[cfg(feature = "boot-smoke")]
pub fn arm_expected_page_fault(address: u64) {
    assert!(INITIALIZED.load(Ordering::SeqCst), "IDT not initialized");
    assert!(!PAGE_FAULT_ARMED.load(Ordering::SeqCst));
    EXPECTED_PAGE_FAULT.store(address, Ordering::SeqCst);
    PAGE_FAULT_ARMED.store(true, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
extern "C" fn mirror_exception_dispatch(frame: &mut TrapFrame) {
    if crate::smp::is_secondary() {
        // APs have no userspace or device-IRQ responsibilities yet. Keep their
        // exceptions out of BSP-only tests, scheduler state, logging and locks.
        if frame.vector == IPI_VECTOR as u64 {
            crate::smp::handle_ipi();
            return;
        }
        if frame.vector == APIC_SPURIOUS_VECTOR as u64 {
            return;
        }
        // Secondary local timers stay masked and stopped. An unexpected timer
        // must fail this CPU instead of entering the BSP's clock or scheduler.
        let cr2: u64;
        // SAFETY: Every installed gate enters at CPL0. The failure recorder
        // uses only atomics before halting this CPU, without serial output.
        unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)) };
        crate::smp::secondary_fault(frame.vector, frame.error, frame.rip, cr2);
    }
    #[cfg(feature = "boot-smoke")]
    assert_kernel_tls_clean();
    dispatch(frame);
    // User RSP is arbitrary even when the interrupt came from a valid RIP.
    // Validate every user return, including IRQs, before assembly reaches IRETQ.
    // Exit/fault handlers already rewrite CS to the trusted kernel continuation.
    if frame.cs & 3 == 3 {
        crate::userspace::prepare_return(frame);
    }
}

fn dispatch(frame: &mut TrapFrame) {
    if frame.vector == IPI_VECTOR as u64 {
        crate::smp::handle_ipi();
        return;
    }
    if frame.vector == APIC_SPURIOUS_VECTOR as u64 {
        // A spurious LAPIC request has no in-service bit. An EOI could retire
        // another interrupt, so it must return without acknowledging anything.
        #[cfg(feature = "boot-smoke")]
        APIC_SPURIOUS_COUNT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if frame.vector == LOCAL_TIMER_VECTOR as u64 {
        // The timer driver acknowledges before permitting a scheduler tick.
        // Kernel interrupts advance time without charging a suspended user.
        if timer::handle_local_timer() && frame.cs & 3 == 3 {
            crate::userspace::timer_tick(frame);
        }
        return;
    }
    // IRQs must never consume an armed synchronous-exception expectation.
    // Device handlers do not allocate, print or take a lock.
    if (32..48).contains(&frame.vector) {
        if matches!(frame.vector, 33 | 44) {
            // The controller routes bytes by source, independent of which PS/2
            // IRQ fired. Drain before the controller acknowledges IRQ1 or IRQ12.
            crate::ps2::handle_irq(frame.vector as u8);
        }
        let tick = timer::handle_irq(frame.vector as u8);
        if tick && frame.cs & 3 == 3 {
            // Only the selected clock source may charge a user; a stale PIT
            // IRQ after switching to LAPIC is acknowledged without scheduling.
            // Validate before saving and unwind before switching roots.
            crate::userspace::timer_tick(frame);
        }
        return;
    }

    if frame.vector == 0x80 {
        crate::userspace::syscall(frame);
        return;
    }

    let cr2: u64;
    // SAFETY: Every gate targets this CPL0 handler. Capture CR2 before printing
    // diagnostics, so a secondary fault cannot silently replace its value.
    unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)) };

    // NMI, double fault, and machine check are kernel/platform failures even
    // when delivered while CPL3 was running. Ordinary user exceptions are local.
    if frame.cs & 3 == 3 && !matches!(frame.vector, 2 | 8 | 18) {
        crate::userspace::fault(frame, cr2);
        return;
    }

    #[cfg(feature = "boot-smoke")]
    {
        if PAGE_FAULT_ARMED.swap(false, Ordering::SeqCst) {
            if frame.vector == 14
                && frame.error == 0
                && frame.cs == u64::from(CODE_SELECTOR)
                && cr2 == EXPECTED_PAGE_FAULT.load(Ordering::SeqCst)
            {
                serial::println!(
                    "MIRROR_PAGE_FAULT_OK cr2={:#x} error={:#x}",
                    cr2,
                    frame.error
                );
                serial::println!("MIRROR_BOOT_OK");
                arch::exit_qemu(true);
            }
            serial::println!("Expected page fault did not match");
        } else if frame.vector == 3 && BREAKPOINT_ARMED.swap(false, Ordering::SeqCst) {
            BREAKPOINT_COUNT.fetch_add(1, Ordering::SeqCst);
            return;
        }
    }

    serial::println!(
        "MIRROR_EXCEPTION vector={} error={:#x} rip={:#x} cr2={:#x}",
        frame.vector,
        frame.error,
        frame.rip,
        cr2
    );
    serial::println!(
        "Exception context: cs={:#x} rflags={:#x} rsp={:#x} ss={:#x}",
        frame.cs,
        frame.rflags,
        frame.rsp,
        frame.ss
    );

    #[cfg(feature = "boot-smoke")]
    arch::exit_qemu(false);

    #[cfg(not(feature = "boot-smoke"))]
    arch::halt()
}
