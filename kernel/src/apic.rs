//! BSP xAPIC/IOAPIC routing for the PIT and i8042 devices discovered by MADT.
//!
//! APs remain stopped. Preparation validates every route before controller
//! configuration; unsupported platforms keep the legacy PIC path. MMIO mappings
//! are supervisor-only, NX, PAT3 UC, and precede all private userspace roots.
//! No AML, SCI, PCI interrupt routing, x2APIC, or local timer calibration yet.
//!
//! Register contracts: Intel SDM vol. 3A chapter 10 and Intel 82093AA datasheet.
//! https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
//! QEMU register implementation: https://github.com/qemu/qemu/blob/v10.1.0/hw/intc/ioapic.c

use core::{
    arch::{asm, x86_64::__cpuid},
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
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

const DEVICE_WINDOW: u64 = 0xffff_d000_0020_0000;
const APIC_BASE: u32 = 0x1b;
const MASKED: u32 = 1 << 16;
const IRQ_LINES: [u8; 3] = [0, 1, 12];
const ID: u32 = 0x20;
const VERSION: u32 = 0x30;
const TPR: u32 = 0x80;
const EOI: u32 = 0xb0;
const SVR: u32 = 0xf0;
const LINT0: u32 = 0x350;

#[derive(Clone, Copy)]
struct Mmio {
    address: u64,
}

impl Mmio {
    fn read(self, offset: u32) -> u32 {
        // SAFETY: Only validated, exclusively owned controller pages construct
        // Mmio. All register offsets below are aligned and within their page.
        unsafe { ((self.address + u64::from(offset)) as *const u32).read_volatile() }
    }

    fn write(self, offset: u32, value: u32) {
        // SAFETY: As above. IF=0 serializes IOREGSEL/window accesses on the BSP;
        // no other CPU or driver is permitted to access these controllers.
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
    bsp: u8,
    io: [IoApic; mirror_acpi::MAX_IO_APICS],
    io_count: usize,
    routes: [Route; 3],
    lint: [u32; 2],
    max_lvt: u32,
}

struct BootController(UnsafeCell<Option<Controller>>);
// SAFETY: Only the boot CPU runs, all calls occur with IF=0, and the state is
// published once before IRQ enablement. No handler holds a reference on return.
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
    let low: u32;
    let high: u32;
    // SAFETY: CPUID reports the architectural local APIC and its base MSR.
    unsafe {
        asm!("rdmsr", in("ecx") APIC_BASE, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
    }
    let base = u64::from(low) | (u64::from(high) << 32);
    if base & (1 << 10) != 0 {
        return Err("x2apic_active");
    }
    if base & (1 << 11) == 0 {
        return Err("lapic_disabled");
    }
    if base & (1 << 8) == 0 {
        return Err("not_boot_cpu");
    }
    if base & 0x000f_ffff_ffff_f000 != topology.lapic_address {
        return Err("lapic_base_mismatch");
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
    let version = local.read(VERSION);
    let max_lvt = (version >> 16) & 0xff;
    if version & 0xff < 0x10 || version & 0xff == 0xff || !(3..=6).contains(&max_lvt) {
        return Err("unsupported_lapic");
    }
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
        if (chip.read(0) >> 24) as u8 != entry.id {
            return Err("ioapic_id_mismatch");
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
        bsp,
        io,
        io_count,
        routes,
        lint,
        max_lvt,
    })
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
    for nmi in nmis {
        // The parser validates LINT0/1 and rejects overlapping descriptions.
        // UID identifies the CPU independently of either MADT record encoding.
        if nmi.processor_uid.is_none() || nmi.processor_uid == Some(uid) {
            lint[nmi.lint as usize] = (4 << 8)
                | electrical(
                    nmi.polarity.unwrap_or(Polarity::ActiveHigh),
                    nmi.trigger.unwrap_or(Trigger::Edge),
                );
        }
    }
    lint
}

fn isr_clear(mut read: impl FnMut(u32) -> u32) -> bool {
    (0..8).all(|bank| read(0x100 + bank * 0x10) == 0)
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
        // Mask every supported local source before replacing legacy ExtINT.
        for offset in [0x320, 0x350, 0x360, 0x370] {
            self.local.write(offset, MASKED | 254);
        }
        if self.max_lvt >= 4 {
            self.local.write(0x340, MASKED | 254);
        }
        if self.max_lvt >= 5 {
            self.local.write(0x330, MASKED | 254);
        }
        if self.max_lvt >= 6 {
            self.local.write(0x2f0, MASKED | 254);
        }
        self.local.write(0x380, 0); // The uncalibrated LAPIC timer stays stopped.
        self.local.write(TPR, 0);
        // Use vector255, software enable, focus checking enabled, EOI broadcast
        // enabled. The dedicated spurious gate sends no EOI.
        self.local
            .write(SVR, (self.local.read(SVR) & !0x13ff) | 0x1ff);
        assert_eq!(self.local.read(SVR) & 0x11ff, 0x1ff);
        for (index, value) in self.lint.into_iter().enumerate() {
            self.local.write(LINT0 + 0x10 * index as u32, value);
        }
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
    assert!(is_enabled());
    assert!(!arch::interrupts_enabled());
    // SAFETY: Publication precedes IRQ enablement; immutable state is retained
    // forever and all accesses to controller registers remain BSP-only, IF=0.
    unsafe { (*CONTROLLER.0.get()).as_ref().expect("APIC publication") }
}

pub fn enable_irq(irq: u8) {
    let controller = controller();
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
pub fn timer_in_service() -> bool {
    controller().local.read(0x110) & 1 != 0
}

/// Exercise the decisions that precede any controller mutation. These fixtures
/// contain no live MMIO addresses and never read or write device registers.
#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    assert!(!arch::interrupts_enabled());
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
    }

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
    // SAFETY: CPUID leaf1 exists on all supported x86-64 CPUs. The explicit
    // unsafe block supports the workspace MSRV, before this intrinsic was safe.
    #[allow(unused_unsafe)]
    unsafe {
        __cpuid(1).edx
    }
}
