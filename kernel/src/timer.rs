//! Bootstrap-CPU timer using QEMU q35's PC-compatible 8259 PIC and 8254 PIT.
//!
//! This is an IRQ delivery foundation, not a scheduler or a calibrated clock.
//! Ticks count delivered IRQs; long interrupt-disabled sections can lose ticks.
//! APIC/IOAPIC routing, ACPI hardware discovery and SMP are not implemented.
//!
//! Register definitions: Intel ICH7 datasheet, legacy interrupt/timer chapters:
//! https://www.intel.com/content/dam/doc/datasheet/i-o-controller-hub-7-datasheet.pdf
//! PIC interrupt acknowledgement/spurious IRQ behavior: Intel 8259A datasheet:
//! https://www.cs.umb.edu/cs341/Intel8259/I8259APIC.pdf
//! Emulated devices verified against QEMU's i8259.c and i8254.c:
//! https://github.com/qemu/qemu/tree/v10.1.0/hw/intc
//! https://github.com/qemu/qemu/tree/v10.1.0/hw/timer

#[cfg(feature = "boot-smoke")]
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{arch, interrupts, serial};

const MASTER_COMMAND: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_COMMAND: u16 = 0xa0;
const SLAVE_DATA: u16 = 0xa1;
const PIT_CHANNEL_ZERO: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;
const IO_DELAY_PORT: u16 = 0x80;
const MASTER_VECTOR: u8 = 32;
const SLAVE_VECTOR: u8 = 40;
const PIC_EOI: u8 = 0x20;
const PIC_READ_ISR: u8 = 0x0b;
const PIT_INPUT_HZ: u32 = 1_193_182;
const TARGET_HZ: u32 = 100;
const PIT_DIVISOR: u32 = (PIT_INPUT_HZ + TARGET_HZ / 2) / TARGET_HZ;
const _: () = assert!(PIT_DIVISOR > 0 && PIT_DIVISOR <= u16::MAX as u32);

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Initialize the legacy PIC/PIT exactly once on the boot CPU with IF clear.
/// The IRQ IDT must already be installed. Leaves IF clear and only IRQ0 unmasked.
/// This kernel owns these fixed PC I/O ports; no other PIC/PIT driver may run.
pub fn init() {
    assert!(!arch::interrupts_enabled(), "timer init requires IF clear");
    assert!(
        interrupts::is_initialized(),
        "timer requires an installed IDT"
    );
    assert!(
        !INITIALIZED.swap(true, Ordering::SeqCst),
        "timer already initialized"
    );

    // SAFETY: This single-use BSP driver owns both PICs, PIT channel 0 and the
    // PC delay port. IF is clear, so no IRQ handler races the command sequence.
    unsafe {
        write_delayed(MASTER_DATA, 0xff);
        write_delayed(SLAVE_DATA, 0xff);

        // ICW1: edge-triggered cascaded PICs, ICW4 follows. Initialization clears
        // the hardware masks; IF stays clear until both PICs are masked again.
        write_delayed(MASTER_COMMAND, 0x11);
        write_delayed(SLAVE_COMMAND, 0x11);
        // ICW2: remap outside CPU exception vectors 0..31.
        write_delayed(MASTER_DATA, MASTER_VECTOR);
        write_delayed(SLAVE_DATA, SLAVE_VECTOR);
        // ICW3: slave is connected to master's IRQ2.
        write_delayed(MASTER_DATA, 1 << 2);
        write_delayed(SLAVE_DATA, 2);
        // ICW4: 8086 mode; explicit EOI, no automatic EOI.
        write_delayed(MASTER_DATA, 1);
        write_delayed(SLAVE_DATA, 1);
        write_delayed(MASTER_DATA, 0xff);
        write_delayed(SLAVE_DATA, 0xff);

        // Channel 0, low byte then high byte, mode 2 rate generator, binary.
        write_delayed(PIT_COMMAND, 0x34);
        write_delayed(PIT_CHANNEL_ZERO, PIT_DIVISOR as u8);
        write_delayed(PIT_CHANNEL_ZERO, (PIT_DIVISOR >> 8) as u8);

        // Only the PIT is serviced. IRQ2 (cascade) remains masked as well.
        write_delayed(MASTER_DATA, 0xfe);
    }
    serial::println!(
        "MIRROR_TIMER_READY source=pit irq=0 vector=32 hz~{} divisor={}",
        TARGET_HZ,
        PIT_DIVISOR
    );
}

/// Delivered timer interrupts, not elapsed wall-clock time. No data publication
/// is associated with this counter, so relaxed atomic ordering is sufficient.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Called only by installed IRQ gates for vectors 32..47, with IF clear.
/// No allocation, locks, printing, scheduling or interrupt nesting here.
pub(crate) fn handle_irq(vector: u8) {
    // SAFETY: Only this BSP driver accesses the PICs; interrupt gates clear IF.
    // The caller already bounds the vector to the remapped legacy IRQ range.
    unsafe {
        // An unasserted request during PIC acknowledgement yields IRQ7 with no
        // ISR bit. No EOI is allowed on that controller for a spurious request.
        if vector == MASTER_VECTOR + 7 && read_isr(MASTER_COMMAND) & 0x80 == 0 {
            return;
        }
        if vector == SLAVE_VECTOR + 7 && read_isr(SLAVE_COMMAND) & 0x80 == 0 {
            // Master did acknowledge the cascade even though the slave had no
            // pending request, so the master alone still needs its EOI.
            arch::out8(MASTER_COMMAND, PIC_EOI);
            return;
        }

        if vector == MASTER_VECTOR {
            TICKS.fetch_add(1, Ordering::Relaxed);
        }
        if vector >= SLAVE_VECTOR {
            arch::out8(SLAVE_COMMAND, PIC_EOI);
        }
        arch::out8(MASTER_COMMAND, PIC_EOI);
    }
}

/// Caller owns the PC legacy device port and delay port and excludes IRQ races.
unsafe fn write_delayed(port: u16, value: u8) {
    // SAFETY: Inherits exclusive port ownership and CPL0 from init.
    unsafe {
        arch::out8(port, value);
        arch::out8(IO_DELAY_PORT, 0);
    }
}

/// Caller owns this PIC command port and excludes concurrent PIC operations.
unsafe fn read_isr(command: u16) -> u8 {
    // SAFETY: Inherits PIC ownership and interrupt exclusion from the caller.
    unsafe {
        arch::out8(command, PIC_READ_ISR);
        arch::in8(command)
    }
}

/// Prove repeated hardware IRQ delivery and wakeup from HLT. Starts and returns
/// with IF clear. The QEMU runner's timeout catches absent or stuck IRQ delivery.
#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    assert!(INITIALIZED.load(Ordering::SeqCst), "timer not initialized");
    assert!(!arch::interrupts_enabled(), "timer test requires IF clear");
    let before = ticks();
    while ticks().wrapping_sub(before) < 3 {
        // SAFETY: IDT/PIC/PIT are ready and only IRQ0 is unmasked. IF remains
        // clear from the counter check until STI. STI delays recognition until
        // after HLT, closing the lost-wakeup race. CLI after IRET leaves IF clear
        // for the next check and subsequent deliberate exception smoke tests.
        unsafe { asm!("sti", "hlt", "cli", options(nostack)) };
    }
    let delivered = ticks().wrapping_sub(before);
    assert!(!arch::interrupts_enabled());
    // SAFETY: IF is clear and the BSP owns both PICs. The returning IRQ0 handler
    // must have acknowledged the interrupt before resuming this test.
    let pending_service = unsafe { read_isr(MASTER_COMMAND) };
    assert_eq!(pending_service & 1, 0, "IRQ0 EOI did not clear ISR");
    serial::println!("MIRROR_TIMER_OK ticks={} if_restored=0", delivered);
}
