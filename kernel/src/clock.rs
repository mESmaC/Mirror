//! Optional HPET elapsed-time clock, independent of delivered timer interrupts.
//!
//! The BSP takes ownership before AP startup and before private roots exist.
//! Only the free-running 64-bit main counter is enabled; comparator interrupts
//! and legacy replacement stay disabled. Published configuration is immutable.
//! No wall-clock epoch, suspend/resume, 32-bit extension or TSC policy yet.
//!
//! Register contracts: Intel HPET 1.0a sections 2.3.2 through 2.3.8:
//! https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/software-developers-hpet-spec-1-0a.pdf
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/timer/hpet.c

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use limine::memory_map::{Entry, EntryType};
use mirror_acpi::{Hpet, Topology};
use mirror_clock_protocol::ClockSnapshot;

use crate::{arch, boot::BootInfo, memory::FrameAllocator, paging::PageTables, serial};

const WINDOW: u64 = 0xffff_d300_0010_0000;
const PAGE_SIZE: u64 = crate::memory::PAGE_SIZE;
const CAPABILITIES: u64 = 0x00;
const CONFIGURATION: u64 = 0x10;
const INTERRUPT_STATUS: u64 = 0x20;
const COUNTER: u64 = 0xf0;
const TIMER_BASE: u64 = 0x100;
const TIMER_STRIDE: u64 = 0x20;
const ENABLE: u64 = 1;
const LEGACY: u64 = 2;
const COUNTER_64: u64 = 1 << 13;
const INTERRUPT_ENABLE: u64 = 1 << 2;
const FSB_ENABLE: u64 = 1 << 14;
const MAX_PERIOD_FS: u32 = 100_000_000;
// Restrict this first 64-bit source to counters that take at least ~584 years
// to wrap. Faster counters require a maintained rollover epoch before use.
const MIN_PERIOD_FS: u32 = 1_000_000;
const FS_PER_NS: u128 = 1_000_000;
const PROGRESS_POLLS: usize = 2_000_000;
const _: () =
    assert!(WINDOW >= crate::heap::HEAP_START + crate::heap::HEAP_MAX_SIZE as u64 + PAGE_SIZE);
const _: () = assert!(TIMER_BASE + 31 * TIMER_STRIDE + 8 <= PAGE_SIZE);

static INITIALIZED: AtomicBool = AtomicBool::new(false);
// Nonzero period publishes both the permanent UC mapping and initialized device.
static PERIOD_FS: AtomicU32 = AtomicU32::new(0);
static LAST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Establish a clock when supported firmware and hardware are available.
/// Call on the BSP with IF=0 before PIT initialization and root publication.
pub fn init(
    descriptor: Option<&Hpet>,
    topology: Option<&Topology>,
    boot: &BootInfo,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
) {
    assert!(!arch::interrupts_enabled());
    assert!(
        !INITIALIZED.swap(true, Ordering::SeqCst),
        "clock initialized twice"
    );
    #[cfg(feature = "boot-smoke")]
    policy_self_test();
    let Some(descriptor) = descriptor else {
        serial::println!("MIRROR_CLOCK_SKIPPED reason=no_hpet");
        return;
    };
    let mut mapped = false;
    let result = prepare(descriptor, topology, boot, tables, frames, &mut mapped);
    match result {
        Ok((period, timers)) => {
            PERIOD_FS.store(period, Ordering::Release);
            serial::println!(
                "MIRROR_CLOCK_READY source=hpet width=64 period_fs={} resolution_ns={} timers={} interrupts=0 legacy=0",
                period,
                resolution_ns(period),
                timers
            );
        }
        Err(reason) => {
            if mapped {
                // SAFETY: No clock was published and prepare leaves the device
                // quiescent after every activation failure. No MMIO borrow lives.
                let physical =
                    unsafe { tables.unmap_page(WINDOW) }.expect("revoke failed HPET mapping");
                assert_eq!(physical, descriptor.address);
            }
            serial::println!("MIRROR_CLOCK_SKIPPED reason={}", reason);
        }
    }
}

fn prepare(
    descriptor: &Hpet,
    topology: Option<&Topology>,
    boot: &BootInfo,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
    mapped: &mut bool,
) -> Result<(u32, u32), &'static str> {
    validate_page(descriptor.address, boot.memory_map.entries())?;
    if topology.is_some_and(|topology| {
        topology.lapic_address == descriptor.address
            || topology
                .io_apics()
                .iter()
                .any(|io| io.address == descriptor.address)
    }) {
        return Err("hpet_controller_overlap");
    }
    let direct = boot
        .direct_map_offset
        .checked_add(descriptor.address)
        .ok_or("hpet_alias_overflow")?;
    if tables
        .translate(direct)
        .map_err(|_| "hpet_alias_invalid")?
        .is_some()
    {
        return Err("hpet_cached_alias");
    }
    // SAFETY: Firmware identifies the owned block, the complete page excludes
    // RAM/framebuffer and HHDM aliases, and no AP or private root exists yet.
    unsafe { tables.map_device_page(WINDOW, descriptor.address, frames) }
        .map_err(|_| "hpet_mapping_failed")?;
    *mapped = true;
    // SAFETY: The temporary UC page is live and the capability register is RO.
    let capability = unsafe { read(CAPABILITIES) };
    let timers = identify(capability, descriptor.event_timer_block_id)?;
    // From the first write onward any inability to stop the device is fatal.
    // We must never expose inherited or partially configured comparator IRQs.
    quiesce(timers).expect("quiesce HPET before clock setup");
    let (period, _) = capabilities(capability, descriptor.event_timer_block_id)?;
    #[cfg(feature = "boot-smoke")]
    {
        let rollover = counter_rollover_self_test();
        quiesce(timers).expect("quiesce HPET after rollover test");
        rollover?;
    }
    let result = activate();
    if result.is_err() {
        quiesce(timers).expect("quiesce rejected HPET clock");
        #[cfg(feature = "boot-smoke")]
        if result == Err("injected_clock_failure") {
            // SAFETY: The device remains privately mapped and quiesced. Verify
            // its main counter is stopped before returning the unavailable path.
            let before = unsafe { read(COUNTER) };
            for _ in 0..256 {
                core::hint::spin_loop();
            }
            // SAFETY: Same live, privately owned UC register.
            assert_eq!(unsafe { read(COUNTER) }, before);
            serial::println!("MIRROR_CLOCK_INJECTED active=1 cleanup=1");
        }
    }
    #[cfg(feature = "boot-smoke")]
    if result.is_ok() {
        serial::println!("MIRROR_CLOCK_COUNTER_OK width=64 rollover=1 reset=1");
    }
    result.map(|()| (period, timers))
}

fn identify(value: u64, firmware_id: u32) -> Result<u32, &'static str> {
    if value as u32 != firmware_id || value & 0xff == 0 || value & (1 << 14) != 0 {
        return Err("hpet_identity");
    }
    Ok(((value >> 8) as u32 & 31) + 1)
}

fn capabilities(value: u64, firmware_id: u32) -> Result<(u32, u32), &'static str> {
    let timers = identify(value, firmware_id)?;
    if value & COUNTER_64 == 0 {
        return Err("hpet_counter_width");
    }
    let period = (value >> 32) as u32;
    if !(MIN_PERIOD_FS..=MAX_PERIOD_FS).contains(&period) {
        return Err("hpet_period");
    }
    Ok((period, timers))
}

fn validate_page(physical: u64, entries: &[&Entry]) -> Result<(), &'static str> {
    let end = physical
        .checked_add(PAGE_SIZE)
        .ok_or("hpet_address_overflow")?;
    if physical == 0 || !physical.is_multiple_of(PAGE_SIZE) {
        return Err("hpet_page_alignment");
    }
    for entry in entries {
        let limit = entry
            .base
            .checked_add(entry.length)
            .ok_or("hpet_memory_map_overflow")?;
        if entry.base < end && physical < limit && entry.entry_type != EntryType::RESERVED {
            return Err("hpet_overlaps_ram");
        }
    }
    Ok(())
}

fn quiesce(timers: u32) -> Result<(), &'static str> {
    // SAFETY: Boot-only exclusive HPET ownership; mapped aligned UC registers.
    unsafe {
        write(CONFIGURATION, read(CONFIGURATION) & !(ENABLE | LEGACY));
        if read(CONFIGURATION) & (ENABLE | LEGACY) != 0 {
            return Err("hpet_stop_readback");
        }
        let mut level_status = 0;
        for timer in 0..timers {
            let register = TIMER_BASE + u64::from(timer) * TIMER_STRIDE;
            let config = read(register);
            write(register, config & !(INTERRUPT_ENABLE | FSB_ENABLE));
            if read(register) & (INTERRUPT_ENABLE | FSB_ENABLE) != 0 {
                return Err("hpet_interrupt_readback");
            }
            if config & (1 << 1) != 0 {
                level_status |= 1 << timer;
            }
        }
        // Only level-configured timers have writable pending-status bits.
        write(INTERRUPT_STATUS, level_status);
    }
    Ok(())
}

fn activate() -> Result<(), &'static str> {
    // SAFETY: Main counter is halted, comparator/FSB interrupts are disabled.
    unsafe {
        write(COUNTER, 0);
        if read(COUNTER) != 0 {
            return Err("hpet_counter_reset");
        }
        write(CONFIGURATION, (read(CONFIGURATION) & !LEGACY) | ENABLE);
        if read(CONFIGURATION) & (ENABLE | LEGACY) != ENABLE {
            return Err("hpet_enable_readback");
        }
        let start = read(COUNTER);
        for _ in 0..PROGRESS_POLLS {
            let current = read(COUNTER);
            if current > start {
                #[cfg(feature = "boot-smoke")]
                if matches!(
                    crate::boot::smoke_mode(),
                    crate::boot::SmokeMode::ClockActivation
                ) {
                    return Err("injected_clock_failure");
                }
                return Ok(());
            }
            if current < start {
                return Err("hpet_counter_regressed");
            }
            core::hint::spin_loop();
        }
    }
    Err("hpet_counter_stalled")
}

#[cfg(feature = "boot-smoke")]
fn counter_rollover_self_test() -> Result<(), &'static str> {
    let boundary = u64::from(u32::MAX);
    // SAFETY: Unpublished device, halted counter, comparator interrupts off.
    // The caller always quiesces it again before checking the result, and the
    // ordinary activation resets the epoch before publishing a usable clock.
    unsafe {
        write(COUNTER, boundary - 32);
        if read(COUNTER) != boundary - 32 {
            return Err("hpet_rollover_setup");
        }
        write(CONFIGURATION, (read(CONFIGURATION) & !LEGACY) | ENABLE);
        let mut previous = boundary - 32;
        for _ in 0..PROGRESS_POLLS {
            let current = read(COUNTER);
            if current < previous {
                return Err("hpet_rollover_regressed");
            }
            if current > boundary {
                return Ok(());
            }
            previous = current;
            core::hint::spin_loop();
        }
    }
    Err("hpet_rollover_stalled")
}

pub fn is_available() -> bool {
    PERIOD_FS.load(Ordering::Acquire) != 0
}

/// Nanoseconds since this boot's counter initialization, with no IRQ dependency.
/// The counter is never reset after publication. Conversion saturates at u64::MAX
/// instead of wrapping (~584 years). A raw counter reset/regression is fatal;
/// suspend/resume and extending a full 64-bit rollover are not yet supported.
/// Current consumers are BSP-only with IF clear, serializing sample observation.
pub fn snapshot() -> Option<ClockSnapshot> {
    assert!(!arch::interrupts_enabled(), "clock read requires IF clear");
    assert!(
        !crate::smp::is_secondary(),
        "clock read requires BSP ownership"
    );
    let period = PERIOD_FS.load(Ordering::Acquire);
    if period == 0 {
        return None;
    }
    // SAFETY: Acquire observed the permanent UC mapping and enabled 64-bit
    // counter. Reads do not mutate device state; no later writer is permitted.
    let counter = unsafe { read(COUNTER) };
    assert!(
        LAST_COUNTER.fetch_max(counter, Ordering::Relaxed) <= counter,
        "HPET main counter regressed or was reset"
    );
    Some(ClockSnapshot {
        monotonic_ns: nanoseconds(counter, period),
        resolution_ns: resolution_ns(period),
    })
}

fn nanoseconds(counter: u64, period: u32) -> u64 {
    (u128::from(counter) * u128::from(period) / FS_PER_NS).min(u128::from(u64::MAX)) as u64
}

fn resolution_ns(period: u32) -> u64 {
    u64::from(period).div_ceil(FS_PER_NS as u64)
}

/// Caller owns a live UC HPET mapping; register offset is valid and aligned.
unsafe fn read(offset: u64) -> u64 {
    // SAFETY: HPET supports aligned 64-bit accesses, including atomic main
    // counter reads. Volatile prevents combining/omitting MMIO operations.
    unsafe { ((WINDOW + offset) as *const u64).read_volatile() }
}

/// Caller owns HPET with IF clear, and the register permits this 64-bit write.
unsafe fn write(offset: u64, value: u64) {
    // SAFETY: Exclusive device access, aligned writable HPET register in UC MMIO.
    unsafe { ((WINDOW + offset) as *mut u64).write_volatile(value) };
}

#[cfg(feature = "boot-smoke")]
fn policy_self_test() {
    let id = 0x8086_a201;
    let valid = u64::from(10_000_000_u32) << 32 | u64::from(id);
    assert_eq!(capabilities(valid, id), Ok((10_000_000, 3)));
    assert_eq!(capabilities(valid, id ^ 1), Err("hpet_identity"));
    assert_eq!(
        capabilities(valid & !COUNTER_64, id & !(COUNTER_64 as u32)),
        Err("hpet_counter_width")
    );
    assert_eq!(capabilities(u64::from(id), id), Err("hpet_period"));
    assert_eq!(
        capabilities(u64::from(MIN_PERIOD_FS - 1) << 32 | u64::from(id), id),
        Err("hpet_period")
    );
    assert_eq!(
        capabilities(u64::from(MIN_PERIOD_FS) << 32 | u64::from(id), id),
        Ok((MIN_PERIOD_FS, 3))
    );
    assert_eq!(
        capabilities((u64::from(MAX_PERIOD_FS) + 1) << 32 | u64::from(id), id),
        Err("hpet_period")
    );
    assert_eq!(nanoseconds(0, MAX_PERIOD_FS), 0);
    assert_eq!(nanoseconds(1, 1), 0);
    assert_eq!(nanoseconds(1_000_001, 1), 1);
    assert_eq!(nanoseconds(123_456_789, 10_000_000), 1_234_567_890);
    assert_eq!(nanoseconds(u64::MAX, MAX_PERIOD_FS), u64::MAX);
    assert!(
        nanoseconds(u64::from(u32::MAX) + 1, 10_000_000)
            > nanoseconds(u64::from(u32::MAX), 10_000_000)
    );
    assert_eq!(resolution_ns(1), 1);
    assert_eq!(resolution_ns(1_000_001), 2);
    assert_eq!(resolution_ns(MAX_PERIOD_FS), 100);
    assert!(validate_page(0, &[]).is_err());
    assert!(validate_page(!(PAGE_SIZE - 1), &[]).is_err());
    assert!(validate_page(0xfed0_0008, &[]).is_err());
    let ram = Entry {
        base: 0xfed0_0000,
        length: PAGE_SIZE,
        entry_type: EntryType::USABLE,
    };
    assert_eq!(validate_page(ram.base, &[&ram]), Err("hpet_overlaps_ram"));
    let reserved = Entry {
        entry_type: EntryType::RESERVED,
        ..ram
    };
    assert_eq!(validate_page(reserved.base, &[&reserved]), Ok(()));
    serial::println!("MIRROR_CLOCK_POLICY_OK");
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    assert!(!arch::interrupts_enabled());
    let Some(start) = snapshot() else {
        serial::println!("MIRROR_CLOCK_TEST_SKIPPED reason=unavailable");
        return;
    };
    let ticks = crate::timer::ticks();
    let mut previous = start.monotonic_ns;
    for _ in 0..PROGRESS_POLLS {
        let now = snapshot().unwrap().monotonic_ns;
        assert!(now >= previous, "HPET clock regressed");
        previous = now;
        if now - start.monotonic_ns >= 20_000_000 {
            assert_eq!(
                crate::timer::ticks(),
                ticks,
                "clock test enabled interrupts"
            );
            assert!(!arch::interrupts_enabled());
            serial::println!(
                "MIRROR_CLOCK_OK elapsed_ns={} irq_ticks=0 if_restored=0",
                now - start.monotonic_ns
            );
            return;
        }
        core::hint::spin_loop();
    }
    panic!("HPET elapsed-time clock did not advance");
}
