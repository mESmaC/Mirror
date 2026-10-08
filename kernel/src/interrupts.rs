//! BSP exception, legacy PIC IRQ, and synchronous ring-3 entry/return delivery.
//!
//! Descriptor and frame layouts follow Intel SDM vol. 3A, sections 6.14 and 7.7:
//! https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
//! Newer error-code exceptions (#CP/#VC/#SX) also follow AMD APM vol. 2, chapter 8:
//! https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2

use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of};
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "boot-smoke")]
use core::sync::atomic::{AtomicU64, AtomicUsize};

use crate::{arch, serial, timer};

global_asm!(include_str!("interrupts.S"));
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
const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024;

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
struct ExceptionStack([u8; DOUBLE_FAULT_STACK_SIZE]);

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
// entries without an IST switch. The common stub saves every integer register.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct TrapFrame {
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
const _: () = assert!(size_of::<TrapFrame>() == 22 * 8);
const _: () = assert!(offset_of!(TrapFrame, vector) == 15 * 8);
const _: () = assert!(offset_of!(TrapFrame, error) == 16 * 8);
const _: () = assert!(offset_of!(TrapFrame, rip) == 17 * 8);
const _: () = assert!(offset_of!(TrapFrame, rsp) == 20 * 8);

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut TSS: TaskStateSegment = TaskStateSegment::EMPTY;
static mut DOUBLE_FAULT_STACK: ExceptionStack = ExceptionStack([0; DOUBLE_FAULT_STACK_SIZE]);
static mut GDT: [u64; 7] = [0; 7];
static mut IDT: [IdtEntry; 256] = [IdtEntry::MISSING; 256];

static USER_ACTIVE: AtomicBool = AtomicBool::new(false);

// Assembly saves the BSP's suspended kernel stack here, with IF clear. User
// pages cannot access this supervisor-only kernel object. SMP will need a
// per-CPU context. The BSP scheduler resumes after each yield or preemption;
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

unsafe extern "C" {
    static mirror_exception_entries: [u64; 48];
    fn mirror_exception_128();
    fn mirror_resume_user(frame: *const TrapFrame);
    fn mirror_resume_kernel();
}

pub(crate) fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

/// Install the boot CPU's GDT, TSS and exception/IRQ IDT once at CPL0.
/// All descriptor storage and the double-fault stack must remain mapped.
/// This bring-up kernel has one CPU; later SMP needs per-CPU TSS/IST storage.
pub fn init() {
    arch::disable_interrupts();
    assert!(
        !INITIALIZED.swap(true, Ordering::SeqCst),
        "IDT already loaded"
    );

    let tss_address = addr_of!(TSS) as u64;
    let stack_top = addr_of!(DOUBLE_FAULT_STACK) as u64 + DOUBLE_FAULT_STACK_SIZE as u64;
    let tss_limit = (size_of::<TaskStateSegment>() - 1) as u64;
    let tss_low = tss_limit
        | ((tss_address & 0xffff) << 16)
        | (((tss_address >> 16) & 0xff) << 32)
        | (0x89_u64 << 40) // Present, available 64-bit TSS.
        | (((tss_address >> 24) & 0xff) << 56);

    // SAFETY: Only the boot CPU runs, IF is clear, and init is single-use. These
    // static objects have not been installed yet and no references alias writes.
    unsafe {
        addr_of_mut!(TSS).write(TaskStateSegment {
            interrupt_stacks: [stack_top, 0, 0, 0, 0, 0, 0],
            ..TaskStateSegment::EMPTY
        });
        addr_of_mut!(GDT).write([
            0,
            0x00af_9b00_0000_ffff, // Accessed, executable/readable 64-bit code.
            0x00cf_9300_0000_ffff, // Accessed, writable data; L bit clear.
            tss_low,
            tss_address >> 32,
            0x00cf_f300_0000_ffff, // DPL3 accessed, writable data.
            0x00af_fb00_0000_ffff, // DPL3 accessed, 64-bit code.
        ]);
        let handlers = addr_of!(mirror_exception_entries).cast::<u64>();
        let entries = addr_of_mut!(IDT).cast::<IdtEntry>();
        for vector in 0..48 {
            let ist = if vector == 8 { DOUBLE_FAULT_IST } else { 0 };
            entries
                .add(vector)
                .write(IdtEntry::interrupt(handlers.add(vector).read(), ist));
        }
        let mut syscall = IdtEntry::interrupt(mirror_exception_128 as *const () as u64, 0);
        syscall.attributes |= 3 << 5; // CPL3 may invoke INT 0x80.
        entries.add(0x80).write(syscall);
    }

    let gdtr = DescriptorPointer {
        limit: (size_of::<[u64; 7]>() - 1) as u16,
        base: addr_of!(GDT) as u64,
    };
    let idtr = DescriptorPointer {
        limit: (size_of::<[IdtEntry; 256]>() - 1) as u16,
        base: addr_of!(IDT) as u64,
    };

    // SAFETY: CPL0, interrupts disabled, persistent valid descriptors. The far
    // return reloads CS with balanced stack use; data selectors are replaced
    // before LTR. FS/GS have no TLS users in this single-CPU kernel. LTR marks
    // our writable TSS descriptor busy. No ring3 transitions or IRQs are enabled
    // here; timer::init configures the PIC/PIT before the caller may set IF.
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
            "mov ax, {tss}",
            "ltr ax",
            "lidt [{idt}]",
            gdt = in(reg) &gdtr,
            idt = in(reg) &idtr,
            code = const CODE_SELECTOR,
            data = const DATA_SELECTOR,
            tss = const TSS_SELECTOR,
            out("rax") _,
        );
    }
    serial::println!("MIRROR_IDT_READY exceptions=32 double_fault_ist=1 irqs=16");
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

/// Construct a fresh integer-only context with no inherited general registers.
/// RDI contains the explicit argument, IF=1, and IOPL=0. The caller must map the
/// image and stack; resume_user checks canonical addresses before CPU entry.
pub(crate) fn user_frame(entry: u64, stack: u64, argument: u64) -> TrapFrame {
    TrapFrame {
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

/// Resume saved integer registers until a handler requests kernel return.
/// User status flags are preserved, IF is forced on, and privilege bits are
/// cleared. FS/GS bases start at zero; TLS and x87/SIMD state are unsupported.
/// Returns on the original kernel stack with IF=0 and callee-saved GPRs intact.
///
/// # Safety
/// Only the BSP may call; interrupts must be disabled. The active page tables
/// must isolate user mappings from kernel mappings and belong to this context.
/// The frame must contain the intended user's state; set_kernel_stack must have
/// installed a mapped kernel stack. IRQ handlers and scheduling state must be
/// initialized. No interrupt-disabling guard may span this call: user mode
/// deliberately sets IF. The saved frame and kernel caller remain mapped.
pub(crate) unsafe fn resume_user(frame: &TrapFrame) {
    assert!(is_initialized(), "IDT not initialized");
    assert!(!arch::interrupts_enabled(), "user entry requires IF=0");
    assert_eq!(frame.cs, u64::from(USER_CODE_SELECTOR), "invalid user CS");
    assert_eq!(frame.ss, u64::from(USER_DATA_SELECTOR), "invalid user SS");
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
    // and resumes here only through the trusted return trampoline.
    unsafe { mirror_resume_user(&prepared) };
    assert!(!arch::interrupts_enabled(), "user return requires IF=0");
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
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    assert!(INITIALIZED.load(Ordering::SeqCst), "IDT not initialized");
    let before = BREAKPOINT_COUNT.load(Ordering::SeqCst);
    assert!(!BREAKPOINT_ARMED.swap(true, Ordering::SeqCst));
    let restored_rax: u64;
    let restored_r10: u64;
    let restored_flags: u64;
    // SAFETY: Vector3 is installed and armed for a returning test. Assembly
    // clears DF before Rust runs and IRET restores it. This block clears DF
    // again before returning to Rust, while checking two volatile registers.
    unsafe {
        asm!(
            "std",
            "int3",
            "pushfq",
            "pop r11",
            "cld",
            inout("rax") 0x1234_5678_9abc_def0_u64 => restored_rax,
            inout("r10") 0xfedc_ba98_7654_3210_u64 => restored_r10,
            lateout("r11") restored_flags,
        );
    }
    assert_eq!(BREAKPOINT_COUNT.load(Ordering::SeqCst), before + 1);
    assert!(!BREAKPOINT_ARMED.load(Ordering::SeqCst));
    assert_eq!(restored_rax, 0x1234_5678_9abc_def0);
    assert_eq!(restored_r10, 0xfedc_ba98_7654_3210);
    assert_ne!(restored_flags & (1 << 10), 0, "IRET did not restore DF");
    serial::println!("MIRROR_EXCEPTIONS_OK");
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
    dispatch(frame);
    // User RSP is arbitrary even when the interrupt came from a valid RIP.
    // Validate every user return, including IRQs, before assembly reaches IRETQ.
    // Exit/fault handlers already rewrite CS to the trusted kernel continuation.
    if frame.cs & 3 == 3 {
        crate::userspace::prepare_return(frame);
    }
}

fn dispatch(frame: &mut TrapFrame) {
    // IRQs must never consume an armed synchronous-exception expectation.
    // Device handlers do not allocate, print or take a lock.
    if (32..48).contains(&frame.vector) {
        if matches!(frame.vector, 33 | 44) {
            // The controller routes bytes by source, independent of which PS/2
            // IRQ fired. Drain before the PIC owner acknowledges IRQ1 or IRQ12.
            crate::ps2::handle_irq(frame.vector as u8);
        }
        timer::handle_irq(frame.vector as u8);
        if frame.vector == 32 && frame.cs & 3 == 3 {
            // Acknowledge IRQ0 before scheduling. timer_tick must validate the
            // return frame before saving it, and unwind before switching roots.
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
