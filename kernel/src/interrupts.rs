//! BSP exception and legacy PIC IRQ delivery; no userspace yet.
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

const CODE_SELECTOR: u16 = 0x08;
const DATA_SELECTOR: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x18;
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
        // No ring transitions are supported until userspace gets its own RSP0.
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
struct ExceptionFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    vector: u64,
    error: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

const _: () = assert!(size_of::<DescriptorPointer>() == 10);
const _: () = assert!(size_of::<TaskStateSegment>() == 104);
const _: () = assert!(offset_of!(TaskStateSegment, interrupt_stacks) == 36);
const _: () = assert!(offset_of!(TaskStateSegment, io_map_offset) == 102);
const _: () = assert!(size_of::<IdtEntry>() == 16);
const _: () = assert!(size_of::<ExceptionFrame>() == 22 * 8);
const _: () = assert!(offset_of!(ExceptionFrame, vector) == 15 * 8);
const _: () = assert!(offset_of!(ExceptionFrame, error) == 16 * 8);
const _: () = assert!(offset_of!(ExceptionFrame, rip) == 17 * 8);
const _: () = assert!(offset_of!(ExceptionFrame, rsp) == 20 * 8);

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut TSS: TaskStateSegment = TaskStateSegment::EMPTY;
static mut DOUBLE_FAULT_STACK: ExceptionStack = ExceptionStack([0; DOUBLE_FAULT_STACK_SIZE]);
static mut GDT: [u64; 5] = [0; 5];
static mut IDT: [IdtEntry; 256] = [IdtEntry::MISSING; 256];

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
        ]);
        let handlers = addr_of!(mirror_exception_entries).cast::<u64>();
        let entries = addr_of_mut!(IDT).cast::<IdtEntry>();
        for vector in 0..48 {
            let ist = if vector == 8 { DOUBLE_FAULT_IST } else { 0 };
            entries
                .add(vector)
                .write(IdtEntry::interrupt(handlers.add(vector).read(), ist));
        }
    }

    let gdtr = DescriptorPointer {
        limit: (size_of::<[u64; 5]>() - 1) as u16,
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
extern "C" fn mirror_exception_dispatch(frame: &ExceptionFrame) {
    // IRQs must never consume an armed synchronous-exception expectation.
    // The IRQ path does not allocate, print or take a lock.
    if (32..48).contains(&frame.vector) {
        timer::handle_irq(frame.vector as u8);
        return;
    }

    let cr2: u64;
    // SAFETY: Every gate targets this CPL0 handler. Capture CR2 before printing
    // diagnostics, so a secondary fault cannot silently replace its value.
    unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)) };

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
