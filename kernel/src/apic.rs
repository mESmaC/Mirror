//! xAPIC initialization, targeted IPIs, and BSP timer/device routing.
//!
//! Preparation validates every route before controller configuration;
//! unsupported platforms keep the legacy PIC path. MMIO mappings
//! are supervisor-only, NX, PAT3 UC, and precede all private userspace roots.
//! No AML, SCI, PCI interrupt routing, or x2APIC yet. The timer driver owns
//! BSP local-timer calibration; secondary local timers remain stopped.
//!
//! Register contracts: Intel SDM vol. 3A chapter 10 and Intel 82093AA datasheet.
//! https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
//! QEMU register implementation: https://github.com/qemu/qemu/blob/v10.1.0/hw/intc/ioapic.c

use core::{
    arch::{asm, x86_64::__cpuid},
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering, fence},
};
use limine::memory_map::{Entry, EntryType};
use mirror_acpi::{LocalNmi, Polarity, Topology, Trigger};

use crate::{
    arch,
    boot::BootInfo,
    memory::{FrameAllocator, PAGE_SIZE},
    paging::PageTables,
    serial,
};

// Dedicated platform mappings must not consume reserved heap growth pages.
const DEVICE_WINDOW: u64 = 0xffff_d300_0000_0000;
const _: () = assert!(
    DEVICE_WINDOW >= crate::heap::HEAP_START + crate::heap::HEAP_MAX_SIZE as u64 + PAGE_SIZE
);
const APIC_BASE: u32 = 0x1b;
const MASKED: u32 = 1 << 16;
const IRQ_LINES: [u8; 3] = [0, 1, 12];
const ID: u32 = 0x20;
const VERSION: u32 = 0x30;
const TPR: u32 = 0x80;
const EOI: u32 = 0xb0;
const SVR: u32 = 0xf0;
const ICR_LOW: u32 = 0x300;
const ICR_HIGH: u32 = 0x310;
const SEND_PENDING: u32 = 1 << 12;
const ICR_SPINS: usize = 1_000_000;
const TIMER_LVT: u32 = 0x320;
const TIMER_INITIAL: u32 = 0x380;
const TIMER_CURRENT: u32 = 0x390;
const TIMER_DIVIDE: u32 = 0x3e0;
const TIMER_VECTOR: u32 = 241;
const TIMER_PERIODIC: u32 = 1 << 17;
const TIMER_DIVIDE_16: u32 = 0x3;
const LINT0: u32 = 0x350;

#[derive(Clone, Copy)]
struct Mmio {
    address: u64,
}

impl Mmio {
    fn read(self, offset: u32) -> u32 {
        // SAFETY: Only validated controller pages construct Mmio. All register
        // offsets below are aligned and within their page. The local APIC page
        // addresses the current CPU's controller, even when its VA is shared.
        unsafe { ((self.address + u64::from(offset)) as *const u32).read_volatile() }
    }

    fn write(self, offset: u32, value: u32) {
        // SAFETY: As above. IF=0 serializes this CPU's local APIC accesses;
        // only the BSP accesses the shared IOREGSEL/window controller pages.
        unsafe {
            ((self.address + u64::from(offset)) as *mut u32).write_volatile(value);
        }
    }
}

#[derive(Clone, Copy)]
struct IoApic {
    mmio: Mmio,
    gsi_base: u32,
    inputs: u32,
}

impl IoApic {
    fn read(&self, register: u32) -> u32 {
        self.mmio.write(0, register);
        self.mmio.read(0x10)
    }

    fn write(&self, register: u32, value: u32) {
        self.mmio.write(0, register);
        self.mmio.write(0x10, value);
    }

    fn low_register(&self, gsi: u32) -> u32 {
        0x10 + 2 * (gsi - self.gsi_base)
    }

    fn contains(&self, gsi: u32) -> bool {
        self.gsi_base <= gsi
            && self
                .gsi_base
                .checked_add(self.inputs)
                .is_some_and(|end| gsi < end)
    }
}

#[derive(Clone, Copy)]
struct Route {
    irq: u8,
    io: usize,
    gsi: u32,
    low: u32,
}

struct Controller {
    local: Mmio,
    local_physical: u64,
    bsp: u8,
    io: [IoApic; mirror_acpi::MAX_IO_APICS],
    io_count: usize,
    routes: [Route; 3],
    lint: [u32; 2],
    max_lvt: u32,
}

struct BootController(UnsafeCell<Option<Controller>>);
// SAFETY: The BSP initializes this exactly once and publishes it with ENABLED's
// release store before AP entry. Readers acquire that publication; the Rust
// state then remains immutable. Local MMIO accesses act on each CPU separately,
// and only the BSP may access the shared IOAPIC registers, always with IF=0.
unsafe impl Sync for BootController {}
static CONTROLLER: BootController = BootController(UnsafeCell::new(None));
static ATTEMPTED: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn init(
    topology: Option<&Topology>,
    boot: &BootInfo,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
) {
    assert!(!arch::interrupts_enabled());
    assert!(!ATTEMPTED.swap(true, Ordering::SeqCst));
    let mut mapped = 0;
    let result = prepare(topology, boot, tables, frames, &mut mapped);
    match result {
        Ok(controller) => {
            // No recoverable errors beyond this boundary: all hardware and
            // route checks have completed. Readback failures stop the kernel.
            controller.configure();
            serial::println!(
                "MIRROR_APIC_READY mode=xapic bsp={} ioapics={} routes=3",
                controller.bsp,
                controller.io_count
            );
            // SAFETY: Single initialization, IF=0, no other CPU accesses state.
            unsafe {
                *CONTROLLER.0.get() = Some(controller);
            }
            ENABLED.store(true, Ordering::Release);
        }
        Err(reason) => {
            for index in 0..mapped {
                // SAFETY: Preparation failed before controller activation; no
                // MMIO references survive. Revoke only this attempt's aliases.
                unsafe { tables.unmap_page(DEVICE_WINDOW + index as u64 * PAGE_SIZE) }
                    .expect("APIC mapping rollback");
            }
            serial::println!("MIRROR_APIC_SKIPPED reason={}", reason);
        }
    }
}

fn prepare(
    topology: Option<&Topology>,
    boot: &BootInfo,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
    mapped: &mut usize,
) -> Result<Controller, &'static str> {
    if crate::boot::legacy_pic() {
        return Err("boot_policy");
    }
    if cpuid_edx() & (1 << 9) == 0 {
        return Err("unsupported_cpu");
    }
    let topology = topology.ok_or("missing_topology")?;
    if !topology.legacy_pic {
        return Err("unsupported_platform");
    }
    if !topology.nmi_sources().is_empty() {
        return Err("global_nmi_unsupported");
    }
    if topology.io_apics().is_empty() {
        return Err("missing_ioapic");
    }
    let base = apic_base();
    validate_base(base, topology.lapic_address)?;
    if base & (1 << 8) == 0 {
        return Err("not_boot_cpu");
    }
    let local = map(topology.lapic_address, boot, tables, frames, mapped)?;
    let bsp = (local.read(ID) >> 24) as u8;
    if bsp == 255 {
        return Err("broadcast_cpu_id");
    }
    let cpu = topology
        .cpus()
        .iter()
        .find(|cpu| cpu.apic_id == u32::from(bsp) && cpu.enabled)
        .ok_or("missing_boot_cpu")?;
    let max_lvt = local_max_lvt(local.read(VERSION))?;
    // Base revision 3 does not guarantee an empty LAPIC ISR. An inherited
    // in-service vector can block our IRQs even after TPR is lowered. Do not
    // retire a firmware-owned interrupt with a blind EOI; retain the PIC path.
    if !isr_clear(|offset| local.read(offset)) {
        return Err("lapic_inherited_isr");
    }
    let lint = lint_configuration(cpu.uid, topology.local_nmis());
    let mut io = [IoApic {
        mmio: Mmio { address: 0 },
        gsi_base: 0,
        inputs: 0,
    }; mirror_acpi::MAX_IO_APICS];
    let io_count = topology.io_apics().len();
    for (index, entry) in topology.io_apics().iter().enumerate() {
        if entry.address == topology.lapic_address {
            return Err("overlapping_controllers");
        }
        let mmio = map(entry.address, boot, tables, frames, mapped)?;
        let mut chip = IoApic {
            mmio,
            gsi_base: entry.gsi_base,
            inputs: 0,
        };
        chip.inputs = ioapic_inputs(chip.read(1))?;
        // The MADT address and GSI base identify the controller, and the
        // register is never rewritten (0013-D9). Redirection entries address
        // the BSP's local APIC, so the IOAPIC's own ID plays no part in
        // delivery; a disagreement is only reported.
        if let Some(hardware) = ioapic_id_mismatch(entry.id, chip.read(0)) {
            serial::println!(
                "MIRROR_APIC_IOAPIC_ID index={} firmware={} hardware={}",
                index,
                entry.id,
                hardware
            );
        }
        validate_gsi_range(chip.gsi_base, chip.inputs, &io[..index])?;
        io[index] = chip;
    }
    let mut routes = [Route {
        irq: 0,
        io: 0,
        gsi: 0,
        low: 0,
    }; 3];
    for (index, irq) in IRQ_LINES.into_iter().enumerate() {
        let route = topology.isa_irq(irq).map_err(|_| "invalid_isa_route")?;
        if routes[..index].iter().any(|other| other.gsi == route.gsi) {
            return Err("shared_isa_route");
        }
        let target = io[..io_count]
            .iter()
            .position(|chip| chip.contains(route.gsi))
            .ok_or("uncovered_gsi")?;
        routes[index] = Route {
            irq,
            io: target,
            gsi: route.gsi,
            low: u32::from(32 + irq) | electrical(route.polarity, route.trigger),
        };
    }
    Ok(Controller {
        local,
        local_physical: topology.lapic_address,
        bsp,
        io,
        io_count,
        routes,
        lint,
        max_lvt,
    })
}

/// Returns the IOAPIC ID register's bits 31:24 when they differ from the
/// MADT's ID for that controller, and `None` when they agree. VMware's
/// firmware writes the MADT ID as the first unused CPU slot while the register
/// resets to 0 (0013-D9).
fn ioapic_id_mismatch(firmware: u8, id_register: u32) -> Option<u8> {
    let hardware = (id_register >> 24) as u8;
    (hardware != firmware).then_some(hardware)
}

fn ioapic_inputs(version: u32) -> Result<u32, &'static str> {
    let maximum = (version >> 16) & 0xff;
    // IOREGSEL is 8 bits; the last redirection pair must fit 0xfe/0xff.
    if version & 0xff < 0x10 || version & 0xff == 0xff || maximum > 119 {
        return Err("unsupported_ioapic");
    }
    Ok(maximum + 1)
}

fn validate_gsi_range(base: u32, inputs: u32, previous: &[IoApic]) -> Result<(), &'static str> {
    let end = base.checked_add(inputs).ok_or("gsi_overflow")?;
    for other in previous {
        let other_end = other
            .gsi_base
            .checked_add(other.inputs)
            .ok_or("gsi_overflow")?;
        if other.gsi_base < end && base < other_end {
            return Err("overlapping_gsi_ranges");
        }
    }
    Ok(())
}

fn lint_configuration(uid: u32, nmis: &[LocalNmi]) -> [u32; 2] {
    let mut lint = [MASKED | 254; 2];
    // The parser validates LINT0/1 and keeps at most one record per LINT for
    // the wildcard and for each UID (0013-D3). Apply wildcards first, then
    // UID-specific records, so that a specific record overrides a wildcard on
    // the same LINT whatever their table order. UID identifies the CPU
    // independently of either MADT record encoding.
    for specific in [false, true] {
        for nmi in nmis {
            if nmi.processor_uid.is_some() == specific
                && (nmi.processor_uid.is_none() || nmi.processor_uid == Some(uid))
            {
                lint[nmi.lint as usize] = (4 << 8)
                    | electrical(
                        nmi.polarity.unwrap_or(Polarity::ActiveHigh),
                        nmi.trigger.unwrap_or(Trigger::Edge),
                    );
            }
        }
    }
    lint
}

fn isr_clear(mut read: impl FnMut(u32) -> u32) -> bool {
    (0..8).all(|bank| read(0x100 + bank * 0x10) == 0)
}

fn irr_clear(mut read: impl FnMut(u32) -> u32) -> bool {
    (0..8).all(|bank| read(0x200 + bank * 0x10) == 0)
}

fn local_max_lvt(version: u32) -> Result<u32, &'static str> {
    let maximum = (version >> 16) & 0xff;
    if version & 0xff < 0x10 || version & 0xff == 0xff || !(3..=6).contains(&maximum) {
        return Err("unsupported_lapic");
    }
    Ok(maximum)
}

fn validate_base(base: u64, physical: u64) -> Result<(), &'static str> {
    if base & (1 << 10) != 0 {
        return Err("x2apic_active");
    }
    if base & (1 << 11) == 0 {
        return Err("lapic_disabled");
    }
    if base & 0x000f_ffff_ffff_f000 != physical {
        return Err("lapic_base_mismatch");
    }
    Ok(())
}

fn apic_base() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: Callers first verify the CPUID architectural local APIC bit.
    unsafe {
        asm!("rdmsr", in("ecx") APIC_BASE, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
    }
    u64::from(low) | (u64::from(high) << 32)
}

fn configure_local(local: Mmio, max_lvt: u32, lint: [u32; 2]) -> Result<(), &'static str> {
    // Mask every supported local source before replacing inherited ExtINT.
    for offset in [TIMER_LVT, 0x350, 0x360, 0x370] {
        local.write(offset, MASKED | 254);
        if local.read(offset) & MASKED == 0 {
            return Err("lapic_mask_readback");
        }
    }
    for (minimum, offset) in [(4, 0x340), (5, 0x330), (6, 0x2f0)] {
        if max_lvt >= minimum {
            local.write(offset, MASKED | 254);
            if local.read(offset) & MASKED == 0 {
                return Err("lapic_mask_readback");
            }
        }
    }
    // This also runs on APs, whose timer must stay stopped throughout runtime.
    local.write(TIMER_INITIAL, 0);
    if local.read(TIMER_INITIAL) != 0 || local.read(TIMER_CURRENT) != 0 {
        return Err("lapic_timer_stop_readback");
    }
    local.write(TPR, 0);
    // Vector255, software enable, focus checking enabled, EOI broadcast on.
    // The dedicated spurious gate sends no EOI.
    local.write(SVR, (local.read(SVR) & !0x13ff) | 0x1ff);
    if local.read(SVR) & 0x11ff != 0x1ff || local.read(TPR) & 0xff != 0 {
        return Err("lapic_enable_readback");
    }
    for (index, value) in lint.into_iter().enumerate() {
        let offset = LINT0 + 0x10 * index as u32;
        local.write(offset, value);
        // Delivery status and remote IRR are hardware-owned read-only bits.
        if local.read(offset) & !(1 << 12 | 1 << 14) != value {
            return Err("lapic_lint_readback");
        }
    }
    Ok(())
}

fn electrical(polarity: Polarity, trigger: Trigger) -> u32 {
    (if polarity == Polarity::ActiveLow {
        1 << 13
    } else {
        0
    }) | (if trigger == Trigger::Level {
        1 << 15
    } else {
        0
    })
}

fn map(
    physical: u64,
    boot: &BootInfo,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
    mapped: &mut usize,
) -> Result<Mmio, &'static str> {
    validate_mmio_page(physical, boot.memory_map.entries())?;
    let direct = boot
        .direct_map_offset
        .checked_add(physical)
        .ok_or("mmio_alias_overflow")?;
    if tables
        .translate(direct)
        .map_err(|_| "mmio_alias_invalid")?
        .is_some()
    {
        return Err("mmio_cached_alias");
    }
    let address = DEVICE_WINDOW + *mapped as u64 * PAGE_SIZE;
    // SAFETY: MADT/MSR identify the owned device; reserved-memory and HHDM
    // checks exclude RAM/cache aliases. IF=0, no APs/userspace roots exist yet.
    unsafe { tables.map_device_page(address, physical, frames) }
        .map_err(|_| "mmio_mapping_failed")?;
    *mapped += 1;
    Ok(Mmio { address })
}

fn validate_mmio_page(physical: u64, entries: &[&Entry]) -> Result<(), &'static str> {
    let end = physical.checked_add(PAGE_SIZE).ok_or("mmio_overflow")?;
    if physical == 0 || physical & (PAGE_SIZE - 1) != 0 {
        return Err("unaligned_mmio");
    }
    // A firmware device address is not authority to overwrite RAM or alias a
    // framebuffer with a conflicting cache type. Holes/reserved MMIO are okay.
    for entry in entries {
        let limit = entry
            .base
            .checked_add(entry.length)
            .ok_or("memory_map_overflow")?;
        if entry.base < end && physical < limit && entry.entry_type != EntryType::RESERVED {
            return Err("mmio_overlaps_ram");
        }
    }
    Ok(())
}

impl Controller {
    fn configure(&self) {
        // SAFETY: The q35-compatible BSP owns both legacy PICs, with IF=0.
        unsafe {
            arch::out8(0x21, 0xff);
            arch::out8(0xa1, 0xff);
        }
        for chip in &self.io[..self.io_count] {
            for input in 0..chip.inputs {
                let low = 0x10 + 2 * input;
                chip.write(low, chip.read(low) | MASKED);
                assert_ne!(chip.read(low) & MASKED, 0, "IOAPIC mask readback");
            }
        }
        configure_local(self.local, self.max_lvt, self.lint).expect("BSP local APIC configuration");
        for route in self.routes {
            let chip = &self.io[route.io];
            let register = chip.low_register(route.gsi);
            // Destination is changed only while masked; all routing starts
            // masked until its PIT/PS2 device driver explicitly enables it.
            chip.write(register, route.low | MASKED);
            chip.write(register + 1, u32::from(self.bsp) << 24);
            chip.write(register, route.low | MASKED);
            assert_eq!(
                chip.read(register) & !(1 << 12 | 1 << 14),
                route.low | MASKED
            );
            assert_eq!(chip.read(register + 1) >> 24, u32::from(self.bsp));
            serial::println!(
                "MIRROR_APIC_IRQ irq={} gsi={} vector={} polarity={} trigger={}",
                route.irq,
                route.gsi,
                32 + route.irq,
                if route.low & (1 << 13) != 0 {
                    "low"
                } else {
                    "high"
                },
                if route.low & (1 << 15) != 0 {
                    "level"
                } else {
                    "edge"
                }
            );
        }
    }
}

fn controller() -> &'static Controller {
    published_controller().expect("APIC publication with interrupts disabled")
}

fn published_controller() -> Result<&'static Controller, &'static str> {
    if arch::interrupts_enabled() {
        return Err("lapic_interrupts_enabled");
    }
    if !is_enabled() {
        return Err("lapic_not_ready");
    }
    // SAFETY: Acquire observes the one BSP publication. The retained state is
    // immutable thereafter; only local MMIO may be accessed by secondary CPUs.
    unsafe { (*CONTROLLER.0.get()).as_ref().ok_or("lapic_not_ready") }
}

/// Return this CPU's physical xAPIC ID after validating its MMIO mode and base.
/// Call with IF=0; this can also validate an AP before its local initialization.
pub(crate) fn local_apic_id() -> Result<u32, &'static str> {
    let controller = published_controller()?;
    if cpuid_edx() & (1 << 9) == 0 {
        return Err("unsupported_cpu");
    }
    validate_base(apic_base(), controller.local_physical)?;
    let id = controller.local.read(ID) >> 24;
    if id == 255 {
        return Err("broadcast_cpu_id");
    }
    Ok(id)
}

/// Configure only the calling secondary CPU's local controller. The BSP must
/// publish the kernel's controller mappings and install this CPU's IDT first.
/// No allocation, logging, shared IOAPIC access, or EOI occurs in this path.
pub(crate) fn init_secondary(
    expected_apic_id: u32,
    topology: &Topology,
) -> Result<(), &'static str> {
    let controller = published_controller()?;
    let id = local_apic_id()?;
    if id != expected_apic_id || id == u32::from(controller.bsp) {
        return Err("secondary_id_mismatch");
    }
    if apic_base() & (1 << 8) != 0 {
        return Err("secondary_is_boot_cpu");
    }
    if topology.lapic_address != controller.local_physical {
        return Err("lapic_base_mismatch");
    }
    let cpu = topology
        .cpus()
        .iter()
        .find(|cpu| cpu.enabled && cpu.apic_id == id)
        .ok_or("missing_secondary_cpu")?;
    let max_lvt = local_max_lvt(controller.local.read(VERSION))?;
    // An inherited in-service interrupt must not be retired blindly. Pending
    // firmware vectors are also rejected before IF can expose the new IDT.
    if !isr_clear(|offset| controller.local.read(offset)) {
        return Err("lapic_inherited_isr");
    }
    if !irr_clear(|offset| controller.local.read(offset)) {
        return Err("lapic_inherited_irr");
    }
    configure_local(
        controller.local,
        max_lvt,
        lint_configuration(cpu.uid, topology.local_nmis()),
    )
}

fn ipi_command(apic_id: u32, vector: u8) -> Result<(u32, u32), &'static str> {
    if apic_id >= 255 {
        return Err("invalid_ipi_destination");
    }
    // Only the installed SMP rendezvous gate is available at this stage.
    if vector != 240 {
        return Err("unsupported_ipi_vector");
    }
    // Explicit physical destination, fixed delivery, edge trigger, asserted
    // level. Write ICR high first; the low write actually submits the command.
    Ok((apic_id << 24, (1 << 14) | u32::from(vector)))
}

fn wait_for_delivery(mut read: impl FnMut() -> u32, spins: usize) -> bool {
    for _ in 0..spins {
        if read() & SEND_PENDING == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Submit a fixed physical IPI from the BSP, including a targeted self-IPI.
/// The caller must publish any work before this call and await its own target
/// acknowledgement: an idle ICR proves submission completion, not execution.
pub(crate) fn send_ipi(apic_id: u32, vector: u8) -> Result<(), &'static str> {
    let (high, low) = ipi_command(apic_id, vector)?;
    let controller = published_controller()?;
    if local_apic_id()? != u32::from(controller.bsp) {
        return Err("ipi_sender_not_bsp");
    }
    let local = controller.local;
    if !wait_for_delivery(|| local.read(ICR_LOW), ICR_SPINS) {
        return Err("ipi_previous_delivery_timeout");
    }
    // The fence orders normal-memory mailbox publication before the UC MMIO
    // notification. IF=0 and BSP-only submission serialize the ICR register pair.
    fence(Ordering::SeqCst);
    local.write(ICR_HIGH, high);
    local.write(ICR_LOW, low);
    if !wait_for_delivery(|| local.read(ICR_LOW), ICR_SPINS) {
        return Err("ipi_delivery_timeout");
    }
    Ok(())
}

fn bsp_local() -> Result<Mmio, &'static str> {
    let controller = published_controller()?;
    if local_apic_id()? != u32::from(controller.bsp) {
        return Err("lapic_timer_not_bsp");
    }
    Ok(controller.local)
}

fn timer_lvt(periodic: bool, masked: bool) -> u32 {
    TIMER_VECTOR | if periodic { TIMER_PERIODIC } else { 0 } | if masked { MASKED } else { 0 }
}

fn timer_configuration(
    lvt: u32,
    divider: u32,
    periodic: bool,
    masked: bool,
) -> Result<(), &'static str> {
    // Delivery status is hardware-owned. All other LVT bits must match,
    // including the vector, mask, and both timer-mode bits (never deadline).
    if lvt & !SEND_PENDING != timer_lvt(periodic, masked) {
        return Err("lapic_timer_lvt_readback");
    }
    if divider & 0xb != TIMER_DIVIDE_16 {
        return Err("lapic_timer_divide_readback");
    }
    Ok(())
}

fn stop_timer(local: Mmio) -> Result<(), &'static str> {
    // Intel SDM vol. 3A, section 12.5.4: zero initial count stops one-shot
    // and periodic timers. Selecting one-shot also disarms inherited deadline
    // mode. Mask before stopping; a previously pending vector is not EOI'd.
    // https://cdrdv2-public.intel.com/843836/325384-sdm-vol-3abcd-dec-24.pdf
    local.write(TIMER_LVT, timer_lvt(false, true));
    local.write(TIMER_INITIAL, 0);
    if local.read(TIMER_LVT) & !SEND_PENDING != timer_lvt(false, true)
        || local.read(TIMER_INITIAL) != 0
        || local.read(TIMER_CURRENT) != 0
    {
        return Err("lapic_timer_stop_readback");
    }
    Ok(())
}

/// Start the BSP's masked, divide-by-16 one-shot calibration counter.
/// Call with IF=0 and IRQ0 still masked. Any error requires a successful
/// `stop_local_timer` before a fallback source may be enabled.
pub(crate) fn begin_timer_calibration() -> Result<(), &'static str> {
    let local = bsp_local()?;
    stop_timer(local)?;
    local.write(TIMER_DIVIDE, TIMER_DIVIDE_16);
    timer_configuration(local.read(TIMER_LVT), local.read(TIMER_DIVIDE), false, true)?;
    local.write(TIMER_INITIAL, u32::MAX);
    if local.read(TIMER_INITIAL) != u32::MAX {
        return Err("lapic_timer_initial_readback");
    }
    Ok(())
}

/// Read the BSP's calibration counter with IF=0.
pub(crate) fn timer_current_count() -> u32 {
    bsp_local()
        .expect("BSP local timer with interrupts disabled")
        .read(TIMER_CURRENT)
}

/// Mask and stop the BSP's local timer with IF=0. The caller must fail-stop
/// if cleanup fails: another timer source cannot safely replace this timer.
/// An already pending interrupt remains the interrupt handler's responsibility.
pub(crate) fn stop_local_timer() -> Result<(), &'static str> {
    stop_timer(bsp_local()?)
}

/// Start the BSP's divide-by-16 periodic timer on vector241 with IF=0.
/// The IDT and selected-source policy must be ready before interrupts resume.
/// Any error requires successful `stop_local_timer` cleanup before fallback.
pub(crate) fn start_periodic_timer(count: u32) -> Result<(), &'static str> {
    if count == 0 {
        return Err("lapic_timer_zero_count");
    }
    let local = bsp_local()?;
    stop_timer(local)?;
    local.write(TIMER_DIVIDE, TIMER_DIVIDE_16);
    local.write(TIMER_LVT, timer_lvt(true, true));
    timer_configuration(local.read(TIMER_LVT), local.read(TIMER_DIVIDE), true, true)?;
    local.write(TIMER_LVT, timer_lvt(true, false));
    timer_configuration(local.read(TIMER_LVT), local.read(TIMER_DIVIDE), true, false)?;
    // Writing initial count starts the timer; changing its mode alone does not.
    local.write(TIMER_INITIAL, count);
    if local.read(TIMER_INITIAL) != count {
        return Err("lapic_timer_initial_readback");
    }
    Ok(())
}

pub fn enable_irq(irq: u8) {
    let controller = controller();
    assert_eq!(controller.local.read(ID) >> 24, u32::from(controller.bsp));
    let route = controller
        .routes
        .iter()
        .find(|route| route.irq == irq)
        .expect("unconfigured IRQ");
    let chip = &controller.io[route.io];
    let register = chip.low_register(route.gsi);
    chip.write(register, route.low);
    assert_eq!(
        chip.read(register) & !(1 << 12 | 1 << 14),
        route.low,
        "IRQ route readback"
    );
}

pub fn eoi() {
    controller().local.write(EOI, 0);
}

#[cfg(feature = "boot-smoke")]
pub fn timer_in_service(vector: u8) -> bool {
    assert!(matches!(vector, 32 | 241));
    let controller = controller();
    assert_eq!(controller.local.read(ID) >> 24, u32::from(controller.bsp));
    let (offset, mask) = isr_location(vector);
    controller.local.read(offset) & mask != 0
}

#[cfg(feature = "boot-smoke")]
fn isr_location(vector: u8) -> (u32, u32) {
    (0x100 + u32::from(vector / 32) * 0x10, 1 << (vector % 32))
}

/// Exercise the decisions that precede any controller mutation. These fixtures
/// contain no live MMIO addresses and never read or write device registers.
#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    assert!(!arch::interrupts_enabled());
    for maximum in 3..=6 {
        assert_eq!(local_max_lvt((maximum << 16) | 0x14), Ok(maximum));
    }
    for version in [0x0003_000f, 0x0003_00ff, 0x0002_0014, 0x0007_0014] {
        assert_eq!(local_max_lvt(version), Err("unsupported_lapic"));
    }
    let base = 0xfee0_0000;
    assert_eq!(validate_base(base | 1 << 11, base), Ok(()));
    assert_eq!(validate_base(base | 1 << 11 | 1 << 8, base), Ok(()));
    assert_eq!(validate_base(base, base), Err("lapic_disabled"));
    assert_eq!(
        validate_base(base | 1 << 11 | 1 << 10, base),
        Err("x2apic_active")
    );
    assert_eq!(
        validate_base(base | 1 << 11, base + PAGE_SIZE),
        Err("lapic_base_mismatch")
    );
    for id in [0, 1, 127, 254] {
        assert_eq!(ipi_command(id, 240), Ok((id << 24, 0x40f0)));
    }
    for id in [255, 256, u32::MAX] {
        assert_eq!(ipi_command(id, 240), Err("invalid_ipi_destination"));
    }
    for vector in [0, 31, 32, 128, 239, 241, 254, 255] {
        assert_eq!(ipi_command(1, vector), Err("unsupported_ipi_vector"));
    }
    assert!(!wait_for_delivery(|| panic!("zero poll budget"), 0));
    assert!(wait_for_delivery(|| 0x40f0, 1));
    let mut polls = 0;
    assert!(!wait_for_delivery(
        || {
            polls += 1;
            SEND_PENDING
        },
        4
    ));
    assert_eq!(polls, 4);
    polls = 0;
    assert!(wait_for_delivery(
        || {
            polls += 1;
            if polls == 4 { 0 } else { SEND_PENDING }
        },
        4
    ));
    assert_eq!(polls, 4);

    for periodic in [false, true] {
        for masked in [false, true] {
            let expected = timer_lvt(periodic, masked);
            for status in [0, SEND_PENDING] {
                assert_eq!(
                    timer_configuration(expected | status, TIMER_DIVIDE_16, periodic, masked),
                    Ok(())
                );
            }
            // A wrong vector, mask, mode, or reserved delivery bit is unsafe.
            for changed in [1, MASKED, TIMER_PERIODIC, 1 << 18, 1 << 8] {
                assert_eq!(
                    timer_configuration(expected ^ changed, TIMER_DIVIDE_16, periodic, masked),
                    Err("lapic_timer_lvt_readback")
                );
            }
            for divider in [0, 1, 2, 8, 9, 10, 11] {
                assert_eq!(
                    timer_configuration(expected, divider, periodic, masked),
                    Err("lapic_timer_divide_readback")
                );
            }
        }
    }
    assert_eq!(isr_location(32), (0x110, 1));
    assert_eq!(isr_location(240), (0x170, 1 << 16));
    assert_eq!(isr_location(241), (0x170, 1 << 17));
    assert_eq!(isr_location(255), (0x170, 1 << 31));
    assert_eq!(start_periodic_timer(0), Err("lapic_timer_zero_count"));

    assert_eq!(ioapic_inputs(0x10), Ok(1));
    assert_eq!(ioapic_inputs(0x0017_0011), Ok(24));
    assert_eq!(ioapic_inputs(0x0077_0020), Ok(120));
    for version in [0x0f, 0xff, 0x0078_0011, 0x00ff_0011] {
        assert_eq!(ioapic_inputs(version), Err("unsupported_ioapic"));
    }

    // The first/last GSI are inclusive; adjacent controllers must not overlap.
    let chip = IoApic {
        mmio: Mmio { address: 0 },
        gsi_base: 10,
        inputs: 24,
    };
    assert!(!chip.contains(9));
    assert!(chip.contains(10));
    assert!(chip.contains(33));
    assert!(!chip.contains(34));
    assert_eq!(chip.low_register(33), 0x3e);
    let widest = IoApic {
        inputs: 120,
        ..chip
    };
    assert_eq!(widest.low_register(129), 0xfe);
    assert_eq!(validate_gsi_range(34, 24, &[chip]), Ok(()));
    assert_eq!(validate_gsi_range(0, 10, &[chip]), Ok(()));
    for (base, inputs) in [(9, 2), (10, 24), (33, 1), (0, 40)] {
        assert_eq!(
            validate_gsi_range(base, inputs, &[chip]),
            Err("overlapping_gsi_ranges")
        );
    }
    assert_eq!(
        validate_gsi_range(u32::MAX - 23, 24, &[]),
        Err("gsi_overflow")
    );
    assert_eq!(validate_gsi_range(u32::MAX - 24, 24, &[]), Ok(()));
    let last = IoApic {
        gsi_base: u32::MAX - 24,
        ..chip
    };
    assert!(last.contains(u32::MAX - 1));
    assert!(!last.contains(u32::MAX));

    assert_eq!(electrical(Polarity::ActiveHigh, Trigger::Edge), 0);
    assert_eq!(electrical(Polarity::ActiveLow, Trigger::Edge), 1 << 13);
    assert_eq!(electrical(Polarity::ActiveHigh, Trigger::Level), 1 << 15);
    assert_eq!(electrical(Polarity::ActiveLow, Trigger::Level), 0xa000);
    let nmi = LocalNmi {
        processor_uid: Some(42),
        x2apic: false,
        lint: 0,
        polarity: Some(Polarity::ActiveLow),
        trigger: Some(Trigger::Level),
    };
    assert_eq!(lint_configuration(42, &[]), [MASKED | 254; 2]);
    assert_eq!(lint_configuration(43, &[nmi]), [MASKED | 254; 2]);
    // Record encoding never changes which UID a specific NMI belongs to.
    for x2apic in [false, true] {
        assert_eq!(
            lint_configuration(42, &[LocalNmi { x2apic, ..nmi }]),
            [0xa400, MASKED | 254]
        );
        let global = LocalNmi {
            processor_uid: None,
            x2apic,
            lint: 1,
            polarity: None,
            trigger: None,
        };
        assert_eq!(lint_configuration(42, &[nmi, global]), [0xa400, 0x400]);
        assert_eq!(
            lint_configuration(43, &[nmi, global]),
            [MASKED | 254, 0x400]
        );
        // 0013-D3: on one LINT the specific record overrides the wildcard for
        // its own CPU only, and table order never changes the result.
        let specific = LocalNmi { x2apic, ..nmi };
        let wildcard = LocalNmi { lint: 0, ..global };
        for uid in [42, 43] {
            assert_eq!(
                lint_configuration(uid, &[specific, wildcard]),
                lint_configuration(uid, &[wildcard, specific])
            );
        }
        assert_eq!(
            lint_configuration(42, &[wildcard, specific]),
            [0xa400, MASKED | 254]
        );
        assert_eq!(
            lint_configuration(43, &[specific, wildcard]),
            [0x400, MASKED | 254]
        );
    }

    // 0013-D9, with no MMIO. VMware's 128-slot MADT gives its IOAPIC ID 0x80
    // while the register reads 0: reported, not rejected. QEMU agrees at 0, and
    // register bits 23:0 are reserved.
    assert_eq!(ioapic_id_mismatch(0x80, 0), Some(0));
    assert_eq!(ioapic_id_mismatch(1, 0), Some(0));
    assert_eq!(ioapic_id_mismatch(0, 0x0100_0000), Some(1));
    assert_eq!(ioapic_id_mismatch(0, 0), None);
    assert_eq!(ioapic_id_mismatch(0x80, 0x8000_0000), None);
    assert_eq!(ioapic_id_mismatch(5, 0x05ff_ffff), None);

    let mut banks = 0;
    assert!(isr_clear(|offset| {
        assert_eq!(offset, 0x100 + banks * 0x10);
        banks += 1;
        0
    }));
    assert_eq!(banks, 8);
    for bank in 0..8 {
        assert!(!isr_clear(|offset| u32::from(
            offset == 0x100 + bank * 0x10
        )));
    }
    banks = 0;
    assert!(irr_clear(|offset| {
        assert_eq!(offset, 0x200 + banks * 0x10);
        banks += 1;
        0
    }));
    assert_eq!(banks, 8);
    for bank in 0..8 {
        assert!(!irr_clear(|offset| u32::from(
            offset == 0x200 + bank * 0x10
        )));
    }

    let reserved = Entry {
        base: 0x2000,
        length: PAGE_SIZE,
        entry_type: EntryType::RESERVED,
    };
    assert_eq!(validate_mmio_page(0x2000, &[]), Ok(()));
    assert_eq!(validate_mmio_page(0x2000, &[&reserved]), Ok(()));
    assert_eq!(validate_mmio_page(0, &[]), Err("unaligned_mmio"));
    assert_eq!(validate_mmio_page(0x2001, &[]), Err("unaligned_mmio"));
    assert_eq!(
        validate_mmio_page(!(PAGE_SIZE - 1), &[]),
        Err("mmio_overflow")
    );
    for entry_type in [
        EntryType::USABLE,
        EntryType::ACPI_RECLAIMABLE,
        EntryType::ACPI_NVS,
        EntryType::BAD_MEMORY,
        EntryType::BOOTLOADER_RECLAIMABLE,
        EntryType::EXECUTABLE_AND_MODULES,
        EntryType::FRAMEBUFFER,
    ] {
        // Even one conflicting byte at either edge rejects the entire page.
        for base in [0x2000, 0x2fff] {
            let conflict = Entry {
                base,
                length: 1,
                entry_type,
            };
            assert_eq!(
                validate_mmio_page(0x2000, &[&conflict]),
                Err("mmio_overlaps_ram")
            );
        }
        let adjacent = Entry {
            base: 0x3000,
            length: PAGE_SIZE,
            entry_type,
        };
        assert_eq!(validate_mmio_page(0x2000, &[&adjacent]), Ok(()));
    }
    let wrapped = Entry {
        base: u64::MAX - 2,
        length: 4,
        entry_type: EntryType::RESERVED,
    };
    assert_eq!(
        validate_mmio_page(0x2000, &[&wrapped]),
        Err("memory_map_overflow")
    );
    serial::println!("MIRROR_APIC_PREFLIGHT_TESTS_OK");
}

fn cpuid_edx() -> u32 {
    __cpuid(1).edx
}
