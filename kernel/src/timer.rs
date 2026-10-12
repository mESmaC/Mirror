//! Bootstrap-CPU local APIC timer, with I/O APIC or legacy PIC/PIT fallback.
//!
//! This is an IRQ delivery foundation, not a scheduler or a calibrated clock.
//! Ticks count delivered IRQs; long interrupt-disabled sections can lose ticks.
//! The local timer is calibrated against polled PIT channel 2 with IF clear.
//! IRQ0 stays masked during calibration; ticks count only the selected source.
//! Secondary timers remain stopped; APs still service only IPIs and faults.
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

use crate::{apic, arch, interrupts, serial};

const MASTER_COMMAND: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_COMMAND: u16 = 0xa0;
const SLAVE_DATA: u16 = 0xa1;
const PIT_CHANNEL_ZERO: u16 = 0x40;
const PIT_CHANNEL_TWO: u16 = 0x42;
const PIT_COMMAND: u16 = 0x43;
const SPEAKER_CONTROL: u16 = 0x61;
const PIT_OUTPUT_TWO: u8 = 1 << 5;
const IO_DELAY_PORT: u16 = 0x80;
const MASTER_VECTOR: u8 = 32;
const SLAVE_VECTOR: u8 = 40;
const PIC_EOI: u8 = 0x20;
const PIC_READ_ISR: u8 = 0x0b;
const PIT_LATCH_ZERO: u8 = 0x00;
pub(crate) const PIT_INPUT_HZ: u32 = 1_193_182;
const TARGET_HZ: u32 = 100;
const PIT_DIVISOR: u32 = (PIT_INPUT_HZ + TARGET_HZ / 2) / TARGET_HZ;
const REFERENCE_COUNT: u16 = 23864; // Approximately 20 ms at 1,193,182 Hz.
const REFERENCE_POLLS: usize = 2_000_000;
const LOCAL_VECTOR: u8 = 241;
const _: () = assert!(PIT_DIVISOR > 0 && PIT_DIVISOR <= u16::MAX as u32);

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU64 = AtomicU64::new(0);
static LOCAL_TIMER: AtomicBool = AtomicBool::new(false);

/// Select a calibrated local timer when possible, otherwise enable PIT IRQ0.
/// Runs once on BSP with IF clear and all timer gates installed. Owns PIT0,
/// PIT2 and the speaker gate during calibration; no competing driver may run.
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
    }
    #[cfg(feature = "boot-smoke")]
    policy_self_test();
    let local = if !apic::is_enabled() {
        Err("legacy_pic")
    } else if crate::boot::pit_timer() {
        Err("boot_policy")
    } else {
        calibrate_and_start()
    };
    if let Ok(initial) = local {
        // IF remains clear; publish the selected source before a timer interrupt
        // can reach the handler. The unused IRQ0 route has never been unmasked.
        LOCAL_TIMER.store(true, Ordering::Release);
        serial::println!(
            "MIRROR_TIMER_READY source=lapic irq=local vector=241 hz~{} divide=16 initial={} controller=lapic",
            TARGET_HZ,
            initial
        );
        assert_eq!(ticks(), 0, "calibration delivered a timer interrupt");
        return;
    }
    let reason = local.unwrap_err();
    if apic::is_enabled() {
        // A failed attempt may have touched the local LVT. Verify it is masked
        // and stopped before enabling fallback; a stale vector241 is ignored.
        apic::stop_local_timer().expect("stop LAPIC timer before PIT fallback");
    }
    serial::println!("MIRROR_LAPIC_TIMER_SKIPPED reason={}", reason);
    if apic::is_enabled() {
        apic::enable_irq(0);
    } else {
        // SAFETY: IF=0, initialized PIC/PIT, sole ownership. Only IRQ0 unmasked.
        unsafe { write_delayed(MASTER_DATA, 0xfe) };
    }
    serial::println!(
        "MIRROR_TIMER_READY source=pit irq=0 vector=32 hz~{} divisor={} controller={}",
        TARGET_HZ,
        PIT_DIVISOR,
        if apic::is_enabled() { "ioapic" } else { "pic" }
    );
    assert_eq!(ticks(), 0, "timer selection changed delivered ticks");
}

fn calibrate_and_start() -> Result<u32, &'static str> {
    let mut elapsed = [0; 3];
    for value in &mut elapsed {
        *value = calibration_sample()?;
    }
    elapsed.sort_unstable();
    let initial = calibrated_count(elapsed)?;
    serial::println!(
        "MIRROR_LAPIC_TIMER_CALIBRATED samples=3 reference_hz={} reference_count={} divide=16 elapsed_min={} elapsed_median={} elapsed_max={} reload={}",
        PIT_INPUT_HZ,
        REFERENCE_COUNT,
        elapsed[0],
        elapsed[1],
        elapsed[2],
        initial
    );
    apic::start_periodic_timer(initial)?;
    Ok(initial)
}

fn calibrated_count(elapsed: [u32; 3]) -> Result<u32, &'static str> {
    let [minimum, median, maximum] = elapsed;
    if minimum == 0 || minimum > median || median > maximum || maximum == u32::MAX {
        return Err("invalid_calibration_sample");
    }
    if u64::from(maximum - minimum) * 100 > u64::from(median) * 25 {
        return Err("unstable_calibration");
    }
    let denominator = u64::from(REFERENCE_COUNT) * u64::from(TARGET_HZ);
    let reload = (u64::from(median) * u64::from(PIT_INPUT_HZ) + denominator / 2) / denominator;
    u32::try_from(reload)
        .ok()
        .filter(|&value| value != 0)
        .ok_or("invalid_timer_reload")
}

fn calibration_sample() -> Result<u32, &'static str> {
    // Claim PIT2 with its speaker output disabled. Preserve unrelated port61
    // control bits; leave gate and speaker disabled on every exit. Mode1 starts
    // precisely at the gate's rising edge (QEMU's mode0 gate pause is incomplete).
    // https://github.com/qemu/qemu/blob/v10.1.0/hw/timer/i8254.c
    // ICH7 port61 writes require bits7:4 zero; preserve only NMI control bits3:2.
    // SAFETY: Boot-only IF=0, exclusive channel2/command/speaker-port ownership.
    let control = unsafe { arch::in8(SPEAKER_CONTROL) } & 0x0c;
    // SAFETY: Count and mode are installed with the gate low and speaker silent.
    unsafe {
        arch::out8(SPEAKER_CONTROL, control);
        arch::out8(PIT_COMMAND, 0xb2); // Channel2, lo/hi, retriggerable one-shot.
        arch::out8(PIT_CHANNEL_TWO, REFERENCE_COUNT as u8);
        arch::out8(PIT_CHANNEL_TWO, (REFERENCE_COUNT >> 8) as u8);
    }
    let result = (|| {
        apic::begin_timer_calibration()?;
        let before = apic::timer_current_count();
        // SAFETY: The programmed PIT2 one-shot starts on this rising gate.
        unsafe { arch::out8(SPEAKER_CONTROL, control | 1) };
        #[cfg(feature = "boot-smoke")]
        if matches!(
            crate::boot::smoke_mode(),
            crate::boot::SmokeMode::TimerCalibration
        ) {
            assert_ne!(before, 0, "injected failure requires a running counter");
            return Err("injected_calibration_failure");
        }
        if !wait_reference(false) || !wait_reference(true) {
            return Err("pit_reference_timeout");
        }
        let after = apic::timer_current_count();
        if after == 0 {
            return Err("local_counter_expired");
        }
        before
            .checked_sub(after)
            .filter(|&delta| delta != 0)
            .ok_or("local_counter_stalled")
    })();
    // SAFETY: Relinquish the reference in a quiescent, silent state even if its
    // output never transitioned. PIT channel0 configuration is untouched.
    unsafe { arch::out8(SPEAKER_CONTROL, control) };
    apic::stop_local_timer().expect("stop masked calibration timer");
    #[cfg(feature = "boot-smoke")]
    if result == Err("injected_calibration_failure") {
        // SAFETY: Same exclusive calibration port; verify both controls really
        // went low before publishing successful cleanup and selecting fallback.
        assert_eq!(unsafe { arch::in8(SPEAKER_CONTROL) } & 3, 0);
        serial::println!("MIRROR_LAPIC_TIMER_INJECTED active=1 cleanup=1");
    }
    result
}

fn wait_reference(high: bool) -> bool {
    for _ in 0..REFERENCE_POLLS {
        // SAFETY: Boot BSP owns this port; IF=0 excludes any device driver race.
        if (unsafe { arch::in8(SPEAKER_CONTROL) } & PIT_OUTPUT_TWO != 0) == high {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn selected_tick(vector: u8, local: bool) -> bool {
    vector == if local { LOCAL_VECTOR } else { MASTER_VECTOR }
}

/// Delivered timer interrupts, not elapsed wall-clock time. No data publication
/// is associated with this counter, so relaxed atomic ordering is sufficient.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Latch and read PIT channel 0's current count. init programs that channel in
/// mode 2 with PIT_DIVISOR whichever tick source it selects, so the count runs
/// from PIT_DIVISOR down to 1 about every 10 ms even while IF is clear and no
/// tick is delivered (spec 0017b-D5). Call on the BSP with IF clear.
pub fn pit_counter() -> u16 {
    assert!(INITIALIZED.load(Ordering::SeqCst), "timer not initialized");
    assert!(!arch::interrupts_enabled(), "PIT read requires IF clear");
    assert!(!crate::smp::is_secondary(), "PIT read requires the BSP");
    // SAFETY: init finished programming channel 0 for low/high byte access, and
    // nothing reprograms it later. The BSP owns the PIT ports and IF is clear,
    // so no other latch or read can split this latch-low-high sequence.
    unsafe {
        arch::out8(PIT_COMMAND, PIT_LATCH_ZERO);
        let low = arch::in8(PIT_CHANNEL_ZERO);
        let high = arch::in8(PIT_CHANNEL_ZERO);
        u16::from_le_bytes([low, high])
    }
}

/// PIT input ticks from one channel 0 sample to a later one, assuming at most
/// one wrap: `(previous - current) mod PIT_DIVISOR`. A count of PIT_DIVISOR is
/// the reload of the period that ends at 1, so it equals 0 here. Missing a
/// whole wrap can only make the result smaller, never larger.
pub fn pit_elapsed(previous: u16, current: u16) -> u32 {
    // Both counts are below 2^16, so the difference fits i32 exactly and the
    // Euclidean remainder lies in 0..PIT_DIVISOR.
    (i32::from(previous) - i32::from(current)).rem_euclid(PIT_DIVISOR as i32) as u32
}

/// Unmask IRQ1 after the keyboard driver has initialized its controller and
/// queue. The BSP owns both interrupt controllers; preserve all other routes.
pub fn enable_keyboard_irq() {
    assert!(INITIALIZED.load(Ordering::SeqCst), "timer not initialized");
    assert!(!arch::interrupts_enabled(), "IRQ1 setup requires IF clear");
    if apic::is_enabled() {
        apic::enable_irq(1);
        return;
    }
    // SAFETY: The BSP owns these PIC registers, IF is clear, and the IRQ1 gate
    // and initialized device handler are installed before this call.
    unsafe {
        let mask = arch::in8(MASTER_DATA);
        arch::out8(MASTER_DATA, mask & !(1 << 1));
        assert_eq!(arch::in8(MASTER_DATA), mask & !(1 << 1));
    }
}

/// Unmask mouse IRQ12 only after the shared PS/2 driver has published its input
/// state. The PIC fallback also needs the slave cascade unmasked.
pub fn enable_mouse_irq() {
    assert!(INITIALIZED.load(Ordering::SeqCst), "timer not initialized");
    assert!(!arch::interrupts_enabled(), "IRQ12 setup requires IF clear");
    if apic::is_enabled() {
        apic::enable_irq(12);
        return;
    }
    // SAFETY: Sole BSP ownership and IF exclusion serialize both PICs. IRQ12's
    // gate/controller handler are ready; enabling the cascade exposes only the
    // explicitly unmasked slave line. The existing IRQ1/IRQ0 masks are retained.
    unsafe {
        let slave_mask = arch::in8(SLAVE_DATA) & !(1 << 4);
        arch::out8(SLAVE_DATA, slave_mask);
        let master_mask = arch::in8(MASTER_DATA) & !(1 << 2);
        arch::out8(MASTER_DATA, master_mask);
        assert_eq!(arch::in8(SLAVE_DATA), slave_mask);
        assert_eq!(arch::in8(MASTER_DATA), master_mask);
    }
}

/// Called only by installed IRQ gates for vectors 32..47, with IF clear.
/// No allocation, locks, printing, scheduling or interrupt nesting here.
/// Return true only when this interrupt delivered a selected-source timer tick.
pub(crate) fn handle_irq(vector: u8) -> bool {
    let tick = selected_tick(vector, LOCAL_TIMER.load(Ordering::Acquire));
    if apic::is_enabled() {
        if tick {
            TICKS.fetch_add(1, Ordering::Relaxed);
        }
        // The I/O APIC routes these vectors directly to the local APIC. The
        // masked PICs did not service this interrupt and must not receive EOI.
        apic::eoi();
        return tick;
    }
    // SAFETY: Only this BSP driver accesses the PICs; interrupt gates clear IF.
    // The caller already bounds the vector to the remapped legacy IRQ range.
    unsafe {
        // An unasserted request during PIC acknowledgement yields IRQ7 with no
        // ISR bit. No EOI is allowed on that controller for a spurious request.
        if vector == MASTER_VECTOR + 7 && read_isr(MASTER_COMMAND) & 0x80 == 0 {
            return false;
        }
        if vector == SLAVE_VECTOR + 7 && read_isr(SLAVE_COMMAND) & 0x80 == 0 {
            // Master did acknowledge the cascade even though the slave had no
            // pending request, so the master alone still needs its EOI.
            arch::out8(MASTER_COMMAND, PIC_EOI);
            return false;
        }

        if tick {
            TICKS.fetch_add(1, Ordering::Relaxed);
        }
        if vector >= SLAVE_VECTOR {
            arch::out8(SLAVE_COMMAND, PIC_EOI);
        }
        arch::out8(MASTER_COMMAND, PIC_EOI);
    }
    tick
}

/// Acknowledge the dedicated BSP local-timer vector before any scheduling.
/// A stale local request after fallback is retired without advancing time.
pub(crate) fn handle_local_timer() -> bool {
    assert!(apic::is_enabled(), "local timer interrupt without APIC");
    let tick = selected_tick(LOCAL_VECTOR, LOCAL_TIMER.load(Ordering::Acquire));
    if tick {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    apic::eoi();
    tick
}

#[cfg(feature = "boot-smoke")]
fn policy_self_test() {
    for vector in 0..=u8::MAX {
        assert_eq!(selected_tick(vector, false), vector == MASTER_VECTOR);
        assert_eq!(selected_tick(vector, true), vector == LOCAL_VECTOR);
    }
    assert_eq!(
        calibrated_count([0, 1, 1]),
        Err("invalid_calibration_sample")
    );
    assert_eq!(
        calibrated_count([3, 2, 4]),
        Err("invalid_calibration_sample")
    );
    assert_eq!(
        calibrated_count([1, 3, 2]),
        Err("invalid_calibration_sample")
    );
    assert_eq!(
        calibrated_count([1, 1, u32::MAX]),
        Err("invalid_calibration_sample")
    );
    assert!(calibrated_count([100, 100, 125]).is_ok());
    assert_eq!(
        calibrated_count([100, 100, 126]),
        Err("unstable_calibration")
    );
    assert!(calibrated_count([1, 1, 1]).is_err());
    assert!(calibrated_count([u32::MAX - 1; 3]).is_ok());
    // A 20 ms reference and 62.5 MHz divided LAPIC yield about625k per10ms.
    let count = calibrated_count([1_249_999, 1_250_000, 1_250_001]).unwrap();
    assert!((624_000..=626_000).contains(&count));
    // PS/2 init sums these channel 0 deltas (spec 0017b-D5): no wrap, one
    // wrap, equal samples, and the divisor boundary, where the reload count
    // PIT_DIVISOR follows 1 by one input tick.
    let divisor = PIT_DIVISOR as u16;
    assert_eq!(pit_elapsed(5_000, 1_000), 4_000);
    assert_eq!(pit_elapsed(100, divisor - 50), 150);
    assert_eq!(pit_elapsed(1, divisor), 1);
    assert_eq!(pit_elapsed(divisor, 1), PIT_DIVISOR - 1);
    assert_eq!(pit_elapsed(7_777, 7_777), 0);
    assert_eq!(pit_elapsed(divisor, divisor), 0);
    serial::println!("MIRROR_TIMER_POLICY_OK");
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
        // SAFETY: The IDT, interrupt controller and PIT are ready. IF remains
        // clear from the counter check until STI. STI delays recognition until
        // after HLT, closing the lost-wakeup race. CLI after IRET leaves IF clear
        // for the next check and subsequent deliberate exception smoke tests.
        unsafe { asm!("sti", "hlt", "cli", options(nostack)) };
    }
    let delivered = ticks().wrapping_sub(before);
    assert!(!arch::interrupts_enabled());
    if apic::is_enabled() {
        let vector = if LOCAL_TIMER.load(Ordering::Acquire) {
            LOCAL_VECTOR
        } else {
            MASTER_VECTOR
        };
        assert!(
            !apic::timer_in_service(vector),
            "timer EOI did not clear LAPIC ISR"
        );
        // SAFETY: IF is clear and this BSP owns both PICs. APIC delivery must
        // leave every legacy PIC line masked, including the slave cascade.
        unsafe {
            assert_eq!(arch::in8(MASTER_DATA), 0xff);
            assert_eq!(arch::in8(SLAVE_DATA), 0xff);
        }
    } else {
        // SAFETY: IF is clear and the BSP owns both PICs. The IRQ0 handler must
        // have acknowledged the interrupt before resuming this test.
        let pending_service = unsafe { read_isr(MASTER_COMMAND) };
        assert_eq!(pending_service & 1, 0, "IRQ0 EOI did not clear PIC ISR");
    }
    serial::println!("MIRROR_TIMER_OK ticks={} if_restored=0", delivered);
}
