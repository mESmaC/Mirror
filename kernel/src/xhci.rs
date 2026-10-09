//! Native xHCI command transport and bounded USB2 descriptor enumeration.
//!
//! One firmware-decoded controller, at most one directly attached USB2 device,
//! one device slot, EP0 control ring and polled primary event ring. Device and
//! configuration descriptors are read before disabling the slot; interfaces
//! are not configured and interrupt endpoints are not started. BIOS
//! ownership, debug/unknown extended capabilities and oversized configurations
//! are refused before mutation. No vendor driver code is copied here.
//!
//! Intel xHCI 1.2 sections 4.2, 4.3, 4.6, 4.9, 4.19, 4.20, 5, 6.4 and 7:
//! https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf
//! Intel post-reset delay quirk, independently applied to Intel PCI functions:
//! https://github.com/torvalds/linux/blob/master/drivers/usb/host/xhci.c

use crate::{
    arch,
    boot::BootInfo,
    clock,
    dma::{DmaBuffer, DmaError},
    memory::FrameAllocator,
    paging::{PageTables, PagingError},
    pci::{Inventory, LegacyError, XhciController},
    usb::{self, UsbSpeed},
};
use core::{
    fmt,
    sync::atomic::{Ordering, fence},
};
use limine::memory_map::{Entry, EntryType};
use mirror_acpi::{Hpet, Topology};

const PAGE: u64 = 4096;
const WINDOW: u64 = 0xffff_d300_001c_0000;
const MMIO_LIMIT: u64 = 256 * 1024;
const MAX_PAGES: usize = (MMIO_LIMIT / PAGE) as usize;
const MAX_PORTS: u8 = 32;
const MAX_SCRATCHPADS: usize = 4;
const MAX_EXT_CAPS: usize = 64;
const POLL_LIMIT: usize = 50_000_000;
const RESET_NS: u64 = 1_000_000_000;
const COMMAND_NS: u64 = 1_000_000_000;
const INTEL_DELAY_NS: u64 = 1_000_000;
const COMMAND_OFFSET: usize = 0;
const EVENT_OFFSET: usize = 4096;
const CONTROL_OFFSET: usize = 8192;
const ERST_OFFSET: usize = CONTROL_OFFSET + 2048;
const SCRATCH_ARRAY_OFFSET: usize = CONTROL_OFFSET + 2112;
const DEVICE_CONTEXT: usize = 0;
const INPUT_CONTEXT: usize = 4096;
const EP0_RING: usize = 8192;
const DEVICE_DATA: usize = 12288;
const DEBOUNCE_NS: u64 = 100_000_000;
const RESET_RECOVERY_NS: u64 = 10_000_000;
const ADDRESS_RECOVERY_NS: u64 = 2_000_000;
const PORT_CHANGES: u32 = 0x00fe_0000;
const PORT_PRESERVE: u32 = (1 << 9) | (3 << 14) | (7 << 25);
const TRBS: usize = 256;
const COMMAND_SLOTS: usize = TRBS - 1;
const TRB_BYTES: usize = 16;
const RUN: u32 = 1;
const RESET: u32 = 1 << 1;
const INTERRUPT_ENABLE: u32 = 1 << 2;
const SYSTEM_ERROR_ENABLE: u32 = 1 << 3;
const HALTED: u32 = 1;
const HOST_ERROR: u32 = 1 << 2;
const NOT_READY: u32 = 1 << 11;
const CONTROLLER_ERROR: u32 = 1 << 12;
const USBCMD: u64 = 0;
const USBSTS: u64 = 4;
const PAGESIZE: u64 = 8;
const DNCTRL: u64 = 0x14;
const CRCR: u64 = 0x18;
const DCBAAP: u64 = 0x30;
const CONFIG: u64 = 0x38;
const PORTSC: u64 = 0x400;
const IMAN: u64 = 0x20;
const IMOD: u64 = 0x24;
const ERSTSZ: u64 = 0x28;
const ERSTBA: u64 = 0x30;
const ERDP: u64 = 0x38;
const _: () = assert!(SCRATCH_ARRAY_OFFSET + MAX_SCRATCHPADS * 8 <= 3 * PAGE as usize);
const _: () = assert!(WINDOW >= crate::heap::HEAP_START + crate::heap::HEAP_MAX_SIZE as u64 + PAGE);

#[cfg(feature = "boot-smoke")]
static INIT_FAILURE_INJECTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[cfg(feature = "boot-smoke")]
static DEVICE_FAILURE_INJECTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[derive(Debug)]
pub enum XhciError {
    Pci(LegacyError),
    Dma(DmaError),
    Paging(PagingError),
    InvalidMmio,
    MemoryOverlap,
    CachedAlias,
    ControllerOverlap,
    UnsupportedVersion,
    InvalidCapabilities,
    UnsupportedCapability,
    UnsupportedPageSize,
    TooManyScratchpads,
    FirmwareOwned,
    ConnectedPorts,
    ClockRequired,
    ControllerNotReady,
    Timeout,
    InvalidEvent,
    CommandFailed,
    EventStorm,
    DmaQuarantined,
    Descriptor(usb::DescriptorError),
    DeviceDisconnected,
    InvalidDeviceState,
    TransferFailed,
    #[cfg(feature = "boot-smoke")]
    InjectedInitialization,
    #[cfg(feature = "boot-smoke")]
    InjectedDeviceFailure,
}

impl fmt::Display for XhciError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pci(error) => write!(output, "PCI claim: {error:?}"),
            Self::Dma(error) => write!(output, "DMA allocation: {error:?}"),
            Self::Paging(error) => write!(output, "MMIO mapping: {error:?}"),
            Self::Descriptor(error) => write!(output, "USB descriptor: {error:?}"),
            error => write!(output, "{error:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    operational: u64,
    runtime: u64,
    runtime_end: u64,
    doorbell: u64,
    doorbell_end: u64,
    ports: u8,
    slots: u8,
    scratchpads: usize,
    extended: u64,
    addresses_64: bool,
    context_size: usize,
}

impl Layout {
    fn parse(
        version_length: u32,
        structural: u32,
        structural2: u32,
        capabilities: u32,
        doorbell: u32,
        runtime: u32,
    ) -> Result<Self, XhciError> {
        if !matches!(version_length >> 16, 0x0100 | 0x0110 | 0x0120) {
            return Err(XhciError::UnsupportedVersion);
        }
        let operational = u64::from(version_length as u8);
        let ports = (structural >> 24) as u8;
        let slots = structural as u8;
        let interrupters = (structural >> 8) & 0x7ff;
        let scratchpads = (((structural2 >> 27) & 31) | (((structural2 >> 21) & 31) << 5)) as usize;
        if operational < 0x20
            || !operational.is_multiple_of(4)
            || !(1..=MAX_PORTS).contains(&ports)
            || slots == 0
            || interrupters == 0
            || interrupters > 1024
            || doorbell & 3 != 0
            || runtime & 31 != 0
        {
            return Err(XhciError::InvalidCapabilities);
        }
        if scratchpads > MAX_SCRATCHPADS {
            return Err(XhciError::TooManyScratchpads);
        }
        let result = Self {
            operational,
            runtime: u64::from(runtime),
            runtime_end: u64::from(runtime) + 0x20 + u64::from(interrupters) * 0x20,
            doorbell: u64::from(doorbell),
            doorbell_end: u64::from(doorbell) + (u64::from(slots) + 1) * 4,
            ports,
            slots,
            scratchpads,
            extended: u64::from(capabilities >> 16) * 4,
            addresses_64: capabilities & 1 != 0,
            context_size: if capabilities & (1 << 2) != 0 { 64 } else { 32 },
        };
        let ranges = [
            (0, operational),
            (operational, operational + PORTSC + u64::from(ports) * 16),
            (result.runtime, result.runtime_end),
            (result.doorbell, result.doorbell_end),
        ];
        for (index, &(start, end)) in ranges.iter().enumerate() {
            if start >= end || end > MMIO_LIMIT {
                return Err(XhciError::InvalidMmio);
            }
            for &(other_start, other_end) in &ranges[..index] {
                if start < other_end && other_start < end {
                    return Err(XhciError::InvalidMmio);
                }
            }
        }
        Ok(result)
    }

    fn extended_range(self, offset: u64, length: u64) -> Result<(), XhciError> {
        let end = offset.checked_add(length).ok_or(XhciError::InvalidMmio)?;
        // Supported Protocol capabilities may occupy the read-only gap before
        // CAPLENGTH (QEMU exposes them at20h/30h). Mutable legacy capabilities
        // are restricted separately before any ownership-byte write.
        if offset < 0x20 || !offset.is_multiple_of(4) || end > MMIO_LIMIT {
            return Err(XhciError::InvalidMmio);
        }
        for (start, limit) in [
            (
                self.operational,
                self.operational + PORTSC + u64::from(self.ports) * 16,
            ),
            (self.runtime, self.runtime_end),
            (self.doorbell, self.doorbell_end),
        ] {
            if offset < limit && start < end {
                return Err(XhciError::InvalidMmio);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct PortProtocol {
    major: u8,
    slot_type: u8,
    speeds: [Option<UsbSpeed>; 16],
}

#[derive(Clone, Copy)]
struct Attached {
    port: u8,
    protocol: PortProtocol,
}

/// Descriptor bytes are copied only after complete checked control transfers.
/// The slot has been disabled before this object is returned successfully.
pub struct UsbDescriptors {
    pub device: [u8; 18],
    pub configuration: [u8; usb::MAX_CONFIG_BYTES],
    pub configuration_len: usize,
    pub speed: UsbSpeed,
    pub port: u8,
    pub slot: u8,
    pub address: u8,
    pub max_packet0: u16,
    pub control_transfers: u64,
    pub control_wraps: u64,
    pub short_packets: u64,
}

/// Both ownership and cycle state are private. One command may be outstanding.
#[must_use = "explicitly halt/reset xHCI or preserve its quarantined DMA"]
pub struct Driver {
    controller: Option<XhciController>,
    physical: u64,
    mapped: u64,
    layout: Option<Layout>,
    takeover: bool,
    quarantined: bool,
    live: bool,
    dma: Option<DmaBuffer>,
    scratch: Option<DmaBuffer>,
    producer: usize,
    producer_cycle: bool,
    consumer: usize,
    consumer_cycle: bool,
    command_wraps: u64,
    event_wraps: u64,
    protocols: [Option<PortProtocol>; MAX_PORTS as usize],
    attached: Option<Attached>,
    device_dma: Option<DmaBuffer>,
    slot: u8,
    enumerated: bool,
    control_producer: usize,
    control_cycle: bool,
    control_wraps: u64,
    control_transfers: u64,
    short_packets: u64,
}

impl Driver {
    /// Initialize a firmware-configured controller with at most one USB2 device.
    ///
    /// # Safety
    /// Run on the boot BSP with IF=0 before AP/private-root publication. The
    /// allocator and HHDM must satisfy DmaBuffer's exclusive ownership contract.
    pub unsafe fn open(
        inventory: &Inventory,
        boot: &BootInfo,
        topology: Option<&Topology>,
        hpet: Option<&Hpet>,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<Option<Self>, XhciError> {
        assert!(!arch::interrupts_enabled());
        let Some(controller) = XhciController::discover(inventory).map_err(XhciError::Pci)? else {
            return Ok(None);
        };
        let mut driver = Self {
            physical: controller.base,
            controller: Some(controller),
            mapped: 0,
            layout: None,
            takeover: false,
            quarantined: false,
            live: false,
            dma: None,
            scratch: None,
            producer: 0,
            producer_cycle: true,
            consumer: 0,
            consumer_cycle: true,
            command_wraps: 0,
            event_wraps: 0,
            protocols: [None; MAX_PORTS as usize],
            attached: None,
            device_dma: None,
            slot: 0,
            enumerated: false,
            control_producer: 0,
            control_cycle: true,
            control_wraps: 0,
            control_transfers: 0,
            short_packets: 0,
        };
        // SAFETY: Caller supplies sole allocator and pre-SMP controller ownership.
        if let Err(error) = unsafe { driver.initialize(boot, topology, hpet, tables, frames) } {
            // SAFETY: No driver or owned pointer has escaped this constructor.
            unsafe { driver.shutdown(tables, frames)? };
            return Err(error);
        }
        Ok(Some(driver))
    }

    unsafe fn initialize(
        &mut self,
        boot: &BootInfo,
        topology: Option<&Topology>,
        hpet: Option<&Hpet>,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), XhciError> {
        self.map_range(0..0x20, boot, topology, hpet, tables, frames)?;
        let version_length = self.read(0);
        let layout = Layout::parse(
            version_length,
            self.read(4),
            self.read(8),
            self.read(0x10),
            self.read(0x14),
            self.read(0x18),
        )?;
        // HCCPARAMS2 exists from1.1 onward. VTIO introduces DMA-ID ownership
        // and another register aperture, neither supported by this transport.
        if version_length >> 16 >= 0x0110 && self.read(0x1c) & (1 << 9) != 0 {
            return Err(XhciError::UnsupportedCapability);
        }
        self.layout = Some(layout);
        self.map_range(
            layout.operational..layout.operational + PORTSC + u64::from(layout.ports) * 16,
            boot,
            topology,
            hpet,
            tables,
            frames,
        )?;
        self.map_range(
            layout.runtime..layout.runtime + 0x40,
            boot,
            topology,
            hpet,
            tables,
            frames,
        )?;
        self.map_range(
            layout.doorbell..layout.doorbell_end,
            boot,
            topology,
            hpet,
            tables,
            frames,
        )?;
        validate_initial_state(self.op_read(USBCMD), self.op_read(USBSTS))?;
        if self.op_read(PAGESIZE) & 1 == 0 {
            return Err(XhciError::UnsupportedPageSize);
        }
        let legacy = self.inspect_extended(boot, topology, hpet, tables, frames)?;
        self.attached = self.select_attached()?;
        if (self.attached.is_some() || self.controller.as_ref().unwrap().needs_reset_delay())
            && !clock::is_available()
        {
            return Err(XhciError::ClockRequired);
        }
        if let Some(offset) = legacy {
            // Only the OS-owned byte is changed. BIOS ownership and SMI enable
            // controls were refused, so this cannot request a firmware handoff.
            self.write_byte(offset + 3, ((self.read(offset) >> 24) as u8) | 1);
            if self.read(offset) & ((1 << 16) | (1 << 24)) != 1 << 24 {
                return Err(XhciError::FirmwareOwned);
            }
        }
        // From here failures either prove full quiescence or retain the claim.
        self.takeover = true;
        // SAFETY: Firmware ownership is released and independent DMA rejected.
        unsafe { self.controller.as_mut().unwrap().prepare() }.map_err(XhciError::Pci)?;
        self.halt_reset()?;
        // SAFETY: HCRST completed and CNR cleared; no owned address is installed.
        self.dma = Some(
            unsafe { DmaBuffer::allocate(frames, boot.direct_map_offset, 3) }
                .map_err(XhciError::Dma)?,
        );
        if layout.scratchpads != 0 {
            // SAFETY: Same unique allocator/HHDM, bounded pinned scratchpad pages.
            self.scratch = Some(
                unsafe { DmaBuffer::allocate(frames, boot.direct_map_offset, layout.scratchpads) }
                    .map_err(XhciError::Dma)?,
            );
        }
        if self.attached.is_some() {
            // SAFETY: Four fresh DMA32 pages hold output/input contexts, EP0
            // ring and one bounded data page. No address is published yet.
            self.device_dma = Some(
                unsafe { DmaBuffer::allocate(frames, boot.direct_map_offset, 4) }
                    .map_err(XhciError::Dma)?,
            );
        }
        self.initialize_dma()?;
        fence(Ordering::SeqCst);
        // SAFETY: Full controller reset retired firmware DMA, all new structures
        // are initialized/pinned. No event/command address is published yet.
        unsafe { self.controller.as_mut().unwrap().enable() }.map_err(XhciError::Pci)?;
        let physical = self.dma.as_ref().unwrap().physical_address();
        let enabled_slots = u32::from(self.attached.is_some());
        self.op_write(CONFIG, enabled_slots); // At most one USB device slot.
        self.op_write(DNCTRL, 0);
        self.write64(
            layout.operational + DCBAAP,
            physical + CONTROL_OFFSET as u64,
        );
        self.write64(
            layout.operational + CRCR,
            (physical + COMMAND_OFFSET as u64) | 1,
        );
        self.write(layout.runtime + IMAN, 1); // IP W1C, IE disabled.
        self.write(layout.runtime + IMOD, 0);
        // ERSTBA can immediately cause DMA: table/data are live before this
        // write, and every DMA pointer is below 4GiB (both high halves stay zero).
        self.write(layout.runtime + ERSTSZ, 1);
        self.write64(layout.runtime + ERDP, physical + EVENT_OFFSET as u64);
        self.write64(layout.runtime + ERSTBA, physical + ERST_OFFSET as u64);
        if self.op_read(CONFIG) & 0xff != enabled_slots
            || self.read(layout.runtime + IMAN) & 2 != 0
            || self.read(layout.runtime + ERSTSZ) & 0xffff != 1
            || self.read64(layout.operational + DCBAAP) != physical + CONTROL_OFFSET as u64
        {
            return Err(XhciError::ControllerNotReady);
        }
        // CRCR's pointer/RCS fields read as zero on compliant controllers. A
        // matching command completion, not a readback assumption, verifies it.
        self.op_write(USBCMD, RUN);
        self.wait_register(layout.operational + USBSTS, HALTED, 0, RESET_NS)?;
        if self.op_read(USBSTS) & (HOST_ERROR | CONTROLLER_ERROR | NOT_READY) != 0 {
            return Err(XhciError::ControllerNotReady);
        }
        self.live = true;
        #[cfg(feature = "boot-smoke")]
        if matches!(
            crate::boot::smoke_mode(),
            crate::boot::SmokeMode::XhciInitFailure
        ) && !INIT_FAILURE_INJECTED.swap(true, Ordering::SeqCst)
        {
            return Err(XhciError::InjectedInitialization);
        }
        Ok(())
    }

    fn inspect_extended(
        &mut self,
        boot: &BootInfo,
        topology: Option<&Topology>,
        hpet: Option<&Hpet>,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<Option<u64>, XhciError> {
        let layout = self.layout.unwrap();
        let mut offset = layout.extended;
        let mut legacy = None;
        for _ in 0..MAX_EXT_CAPS {
            if offset == 0 {
                return Ok(legacy);
            }
            layout.extended_range(offset, 4)?;
            self.map_range(offset..offset + 4, boot, topology, hpet, tables, frames)?;
            let header = self.read(offset);
            let mut length = match header as u8 {
                1 => 8,
                2 => 16,
                // Debug can survive HCRST. Unknown capabilities may add DMA
                // domains or ownership controls; none are guessed at here.
                _ => return Err(XhciError::UnsupportedCapability),
            };
            layout.extended_range(offset, length)?;
            self.map_range(
                offset..offset + length,
                boot,
                topology,
                hpet,
                tables,
                frames,
            )?;
            if header as u8 == 2 {
                length += u64::from(self.read(offset + 8) >> 28) * 4;
                layout.extended_range(offset, length)?;
                self.map_range(
                    offset..offset + length,
                    boot,
                    topology,
                    hpet,
                    tables,
                    frames,
                )?;
                self.record_protocol(offset, header)?;
            }
            if header as u8 == 1 {
                if offset < layout.operational {
                    return Err(XhciError::InvalidMmio);
                }
                if legacy.is_some()
                    || header & (1 << 16) != 0
                    || self.read(offset + 4) & 0xe011 != 0
                {
                    return Err(XhciError::FirmwareOwned);
                }
                legacy = Some(offset);
            }
            let next = u64::from((header >> 8) as u8) * 4;
            if next == 0 {
                return Ok(legacy);
            }
            if next < length {
                return Err(XhciError::InvalidCapabilities);
            }
            offset = offset.checked_add(next).ok_or(XhciError::InvalidMmio)?;
        }
        Err(XhciError::InvalidCapabilities)
    }

    fn map_range(
        &mut self,
        range: core::ops::Range<u64>,
        boot: &BootInfo,
        topology: Option<&Topology>,
        hpet: Option<&Hpet>,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), XhciError> {
        let offset = range.start;
        let end = range.end;
        if offset >= end || end > MMIO_LIMIT {
            return Err(XhciError::InvalidMmio);
        }
        for page in offset / PAGE..end.div_ceil(PAGE) {
            let bit = 1_u64 << page;
            if self.mapped & bit != 0 {
                continue;
            }
            let physical = self
                .physical
                .checked_add(page * PAGE)
                .ok_or(XhciError::InvalidMmio)?;
            validate_page(physical, boot.memory_map.entries())?;
            let overlaps = |address: u64| address & !(PAGE - 1) == physical;
            if hpet.is_some_and(|hpet| overlaps(hpet.address))
                || topology.is_some_and(|topology| {
                    overlaps(topology.lapic_address)
                        || topology.io_apics().iter().any(|io| overlaps(io.address))
                })
            {
                return Err(XhciError::ControllerOverlap);
            }
            // AHCI normally releases its two-page window before xHCI. A retained
            // alias can mean quarantined DMA; never remap that controller page.
            for ahci_page in 0..2 {
                if tables
                    .translate(0xffff_d300_0018_0000 + ahci_page * PAGE)
                    .map_err(XhciError::Paging)?
                    == Some(physical)
                {
                    return Err(XhciError::ControllerOverlap);
                }
            }
            let direct = boot
                .direct_map_offset
                .checked_add(physical)
                .ok_or(XhciError::InvalidMmio)?;
            if tables
                .translate(direct)
                .map_err(XhciError::Paging)?
                .is_some()
            {
                return Err(XhciError::CachedAlias);
            }
            if let Some(framebuffer) = &boot.framebuffer {
                let base = tables
                    .translate(framebuffer.addr() as u64)
                    .map_err(XhciError::Paging)?
                    .ok_or(XhciError::InvalidMmio)?;
                let length = framebuffer
                    .pitch()
                    .checked_mul(framebuffer.height())
                    .ok_or(XhciError::InvalidMmio)?;
                let limit = base.checked_add(length).ok_or(XhciError::InvalidMmio)?;
                if physical < limit && base < physical + PAGE {
                    return Err(XhciError::MemoryOverlap);
                }
            }
            // SAFETY: Whole reserved page excludes known aliases/RAM/firmware
            // controllers, and this boot-only claim owns the register range.
            unsafe { tables.map_device_page(WINDOW + page * PAGE, physical, frames) }
                .map_err(XhciError::Paging)?;
            self.mapped |= bit;
        }
        Ok(())
    }

    fn record_protocol(&mut self, offset: u64, header: u32) -> Result<(), XhciError> {
        if self.read(offset + 4) != 0x2042_5355 {
            return Err(XhciError::InvalidCapabilities);
        }
        let major = (header >> 24) as u8;
        if !matches!(major, 2 | 3) {
            return Err(XhciError::UnsupportedCapability);
        }
        let ports = self.read(offset + 8);
        let first = ports as u8;
        let count = (ports >> 8) as u8;
        let protocol_slots = self.read(offset + 12);
        if protocol_slots & !31 != 0 {
            return Err(XhciError::InvalidCapabilities);
        }
        if count == 0 {
            return Ok(());
        }
        if first == 0 || u16::from(first) + u16::from(count) > u16::from(self.max_ports()) + 1 {
            return Err(XhciError::InvalidCapabilities);
        }
        let speed_count = (ports >> 28) as u8;
        let mut speeds = [None; 16];
        if major == 2 && speed_count == 0 {
            speeds[1] = Some(UsbSpeed::Full);
            speeds[2] = Some(UsbSpeed::Low);
            speeds[3] = Some(UsbSpeed::High);
        }
        // USB3 asymmetric RX/TX entries may share a speed ID. We need only
        // that protocol's port ownership here; no USB3 speed is consumed.
        if major == 2 {
            let mut seen = 0_u16;
            for index in 0..speed_count {
                let psi = self.read(offset + 16 + u64::from(index) * 4);
                let id = (psi & 15) as usize;
                if id == 0 || seen & (1 << id) != 0 {
                    return Err(XhciError::InvalidCapabilities);
                }
                seen |= 1 << id;
                speeds[id] = usb2_speed(psi);
            }
        }
        let protocol = PortProtocol {
            major,
            slot_type: protocol_slots as u8,
            speeds,
        };
        for port in first - 1..first - 1 + count {
            if self.protocols[usize::from(port)].is_some() {
                return Err(XhciError::InvalidCapabilities);
            }
            self.protocols[usize::from(port)] = Some(protocol);
        }
        Ok(())
    }

    fn select_attached(&self) -> Result<Option<Attached>, XhciError> {
        let mut attached = None;
        for index in 0..self.max_ports() {
            let status = self.op_read(PORTSC + u64::from(index) * 16);
            if status & 3 == 0 {
                continue;
            }
            let Some(protocol) = self.protocols[usize::from(index)] else {
                return Err(XhciError::ConnectedPorts);
            };
            if status & 1 == 0 || protocol.major != 2 || attached.is_some() {
                return Err(XhciError::ConnectedPorts);
            }
            attached = Some(Attached {
                port: index + 1,
                protocol,
            });
        }
        Ok(attached)
    }

    fn initialize_dma(&self) -> Result<(), XhciError> {
        let dma = self.dma.as_ref().unwrap();
        let base = dma.physical_address();
        validate_dma(base)?;
        let pointer = dma.as_ptr();
        // SAFETY: Fresh zeroed allocations, HC reset, no DMA addresses published.
        unsafe {
            put_trb(pointer, COMMAND_SLOTS * TRB_BYTES, link_trb(base, true));
            pointer
                .add(ERST_OFFSET)
                .cast::<u64>()
                .write_volatile(base + EVENT_OFFSET as u64);
            pointer
                .add(ERST_OFFSET + 8)
                .cast::<u32>()
                .write_volatile(TRBS as u32);
            if let Some(scratch) = &self.scratch {
                pointer
                    .add(CONTROL_OFFSET)
                    .cast::<u64>()
                    .write_volatile(base + SCRATCH_ARRAY_OFFSET as u64);
                for index in 0..self.layout.unwrap().scratchpads {
                    pointer
                        .add(SCRATCH_ARRAY_OFFSET + index * 8)
                        .cast::<u64>()
                        .write_volatile(scratch.physical_address() + index as u64 * PAGE);
                }
            }
        }
        Ok(())
    }

    pub fn max_ports(&self) -> u8 {
        self.layout.unwrap().ports
    }
    pub fn max_slots(&self) -> u8 {
        self.layout.unwrap().slots
    }
    pub fn command_wraps(&self) -> u64 {
        self.command_wraps
    }
    pub fn event_wraps(&self) -> u64 {
        self.event_wraps
    }

    /// Complete one actual xHCI No Op command. No USB transfer/device state is
    /// requested. Any error retires this command stream until explicit shutdown.
    pub fn noop(&mut self) -> Result<(), XhciError> {
        assert!(!arch::interrupts_enabled());
        if !self.live {
            return Err(XhciError::ControllerNotReady);
        }
        let result = self
            .submit_command([0, 0, 0, 23 << 10], Some(0))
            .map(|_| ());
        if result.is_err() {
            self.live = false;
        }
        result
    }

    fn submit_command(
        &mut self,
        command: [u32; 4],
        expected_slot: Option<u8>,
    ) -> Result<u8, XhciError> {
        let layout = self.layout.unwrap();
        if self.op_read(USBSTS) & (HALTED | HOST_ERROR | CONTROLLER_ERROR | NOT_READY) != 0 {
            return Err(XhciError::ControllerNotReady);
        }
        let dma = self.dma.as_ref().unwrap();
        let base = dma.physical_address();
        let offset = self.producer * TRB_BYTES;
        let command_pointer = base + offset as u64;
        // SAFETY: One request at a time. All earlier slot users completed; Link
        // ownership is republished before the command that makes HC reach it.
        unsafe {
            if self.producer == COMMAND_SLOTS - 1 {
                put_trb(
                    dma.as_ptr(),
                    COMMAND_SLOTS * TRB_BYTES,
                    link_trb(base, self.producer_cycle),
                );
            }
            put_trb(
                dma.as_ptr(),
                offset,
                [
                    command[0],
                    command[1],
                    command[2],
                    command[3] | u32::from(self.producer_cycle),
                ],
            );
        }
        fence(Ordering::SeqCst);
        self.write(layout.doorbell, 0);
        let start = clock::snapshot().map(|time| time.monotonic_ns);
        let mut asynchronous = 0;
        for poll in 0..POLL_LIMIT {
            if self.op_read(USBSTS) & (HALTED | HOST_ERROR | CONTROLLER_ERROR | NOT_READY) != 0 {
                return Err(XhciError::ControllerNotReady);
            }
            if let Some(event) = self.read_event() {
                let completed = command_event(event, command_pointer, layout.ports, expected_slot)?;
                self.consume_event();
                if let Some(slot) = completed {
                    self.producer += 1;
                    if self.producer == COMMAND_SLOTS {
                        self.producer = 0;
                        self.producer_cycle = !self.producer_cycle;
                        self.command_wraps += 1;
                    }
                    return Ok(slot);
                }
                asynchronous += 1;
                if asynchronous > 64 {
                    return Err(XhciError::EventStorm);
                }
            }
            if timed_out(start, poll, COMMAND_NS) {
                break;
            }
            core::hint::spin_loop();
        }
        Err(XhciError::Timeout)
    }

    fn read_event(&self) -> Option<[u32; 4]> {
        let pointer = self.dma.as_ref().unwrap().as_ptr();
        let offset = EVENT_OFFSET + self.consumer * TRB_BYTES;
        // SAFETY: Only a matching cycle transfers ownership of this bounded TRB.
        let control = unsafe { pointer.add(offset + 12).cast::<u32>().read_volatile() };
        if control & 1 != u32::from(self.consumer_cycle) {
            return None;
        }
        fence(Ordering::SeqCst);
        // SAFETY: Acquired matching event cycle; all fields lie in the live ring.
        Some(unsafe {
            [
                pointer.add(offset).cast::<u32>().read_volatile(),
                pointer.add(offset + 4).cast::<u32>().read_volatile(),
                pointer.add(offset + 8).cast::<u32>().read_volatile(),
                control,
            ]
        })
    }

    fn consume_event(&mut self) {
        self.consumer += 1;
        if self.consumer == TRBS {
            self.consumer = 0;
            self.consumer_cycle = !self.consumer_cycle;
            self.event_wraps += 1;
        }
        let layout = self.layout.unwrap();
        let physical = self.dma.as_ref().unwrap().physical_address()
            + EVENT_OFFSET as u64
            + (self.consumer * TRB_BYTES) as u64;
        fence(Ordering::SeqCst);
        // Move ERDP to the next unconsumed TRB, acknowledging EHB (W1C).
        self.write64(layout.runtime + ERDP, physical | 8);
        self.write(layout.runtime + IMAN, 1); // IP W1C; never enable interrupts.
    }

    /// Enumerate one directly connected USB2 device without configuring any
    /// interface. A successful return has completed Disable Slot; shutdown
    /// later reclaims its pinned context, control-ring and data pages.
    pub fn enumerate(&mut self) -> Result<Option<UsbDescriptors>, XhciError> {
        assert!(!arch::interrupts_enabled());
        if !self.live || self.enumerated {
            return Err(XhciError::InvalidDeviceState);
        }
        let Some(attached) = self.attached else {
            return Ok(None);
        };
        let result = self.enumerate_device(attached);
        if result.is_err() {
            self.live = false;
        }
        result.map(Some)
    }

    fn enumerate_device(&mut self, attached: Attached) -> Result<UsbDescriptors, XhciError> {
        let (speed, speed_id) = self.reset_usb2_port(attached)?;
        let slot = self.submit_command(
            [
                0,
                0,
                0,
                (9 << 10) | (u32::from(attached.protocol.slot_type) << 16),
            ],
            None,
        )?;
        self.slot = slot;
        let max_packet0 = if speed == UsbSpeed::High { 64 } else { 8 };
        self.prepare_device_context(attached.port, speed_id, max_packet0)?;
        let physical = self.device_dma.as_ref().unwrap().physical_address();
        let dcbaa = self.dma.as_ref().unwrap().as_ptr();
        // SAFETY: Newly Enabled slot has not received an Address command. Its
        // pre-zeroed output context remains pinned until Disable Slot or reset.
        unsafe {
            dcbaa
                .add(CONTROL_OFFSET + usize::from(slot) * 8)
                .cast::<u64>()
                .write_volatile(physical + DEVICE_CONTEXT as u64)
        };
        fence(Ordering::SeqCst);
        self.submit_command(
            [
                (physical + INPUT_CONTEXT as u64) as u32,
                0,
                0,
                (11 << 10) | (u32::from(slot) << 24),
            ],
            Some(slot),
        )?;
        // Address Device with BSR=0 assigns the USB address itself. Software
        // must never put a USB SET_ADDRESS request on this transfer ring.
        let pointer = self.device_dma.as_ref().unwrap().as_ptr();
        // SAFETY: Command completion acquired xHC's completed output context.
        let slot_state = unsafe {
            pointer
                .add(DEVICE_CONTEXT + 12)
                .cast::<u32>()
                .read_volatile()
        };
        let address = slot_state as u8;
        if slot_state >> 27 != 2 || !(1..=127).contains(&address) {
            return Err(XhciError::InvalidDeviceState);
        }
        self.device_delay(ADDRESS_RECOVERY_NS, attached.port)?;
        #[cfg(feature = "boot-smoke")]
        if matches!(
            crate::boot::smoke_mode(),
            crate::boot::SmokeMode::XhciDeviceFailure
        ) && !DEVICE_FAILURE_INJECTED.swap(true, Ordering::SeqCst)
        {
            return Err(XhciError::InjectedDeviceFailure);
        }

        let mut prefix = [0_u8; 8];
        if self.get_descriptor(1, &mut prefix)? != prefix.len() {
            return Err(XhciError::TransferFailed);
        }
        let actual_packet =
            u16::from(usb::device_prefix(&prefix, speed).map_err(XhciError::Descriptor)?);
        if actual_packet != max_packet0 {
            if speed != UsbSpeed::Full {
                return Err(XhciError::InvalidDeviceState);
            }
            self.evaluate_packet_size(actual_packet)?;
        }
        let mut device = [0_u8; 18];
        if self.get_descriptor(1, &mut device)? != device.len() || device[..8] != prefix {
            return Err(XhciError::TransferFailed);
        }
        usb::parse_device(&device, speed).map_err(XhciError::Descriptor)?;
        let mut header = [0_u8; 9];
        if self.get_descriptor(2, &mut header)? != header.len() {
            return Err(XhciError::TransferFailed);
        }
        let total = usize::from(
            usb::configuration_header(&header)
                .map_err(XhciError::Descriptor)?
                .total_length,
        );
        if total > usb::MAX_CONFIG_BYTES {
            return Err(XhciError::TransferFailed);
        }
        let mut configuration = [0_u8; usb::MAX_CONFIG_BYTES];
        if self.get_descriptor(2, &mut configuration[..total])? != total
            || configuration[..9] != header
        {
            return Err(XhciError::TransferFailed);
        }
        #[cfg(feature = "boot-smoke")]
        for _ in 0..172 {
            let mut oversized = [0_u8; 64];
            if self.get_descriptor(1, &mut oversized)? != device.len() || oversized[..18] != device
            {
                return Err(XhciError::TransferFailed);
            }
        }
        self.submit_command([0, 0, 0, (10 << 10) | (u32::from(slot) << 24)], Some(slot))?;
        // SAFETY: Disable Slot completion retired EP0/context references. Keep
        // frames pinned until shutdown but revoke its DCBAA pointer immediately.
        unsafe {
            dcbaa
                .add(CONTROL_OFFSET + usize::from(slot) * 8)
                .cast::<u64>()
                .write_volatile(0)
        };
        self.slot = 0;
        self.enumerated = true;
        Ok(UsbDescriptors {
            device,
            configuration,
            configuration_len: total,
            speed,
            port: attached.port,
            slot,
            address,
            max_packet0: actual_packet,
            control_transfers: self.control_transfers,
            control_wraps: self.control_wraps,
            short_packets: self.short_packets,
        })
    }

    fn prepare_device_context(&self, port: u8, speed_id: u8, packet: u16) -> Result<(), XhciError> {
        let dma = self
            .device_dma
            .as_ref()
            .ok_or(XhciError::InvalidDeviceState)?;
        let base = dma.physical_address();
        let size = self.layout.unwrap().context_size;
        let input_slot = INPUT_CONTEXT + size;
        let input_ep0 = INPUT_CONTEXT + size * 2;
        // SAFETY: Fresh zeroed private allocation, slot Enabled but unaddressed.
        unsafe {
            let pointer = dma.as_ptr();
            pointer
                .add(INPUT_CONTEXT + 4)
                .cast::<u32>()
                .write_volatile(3);
            pointer
                .add(input_slot)
                .cast::<u32>()
                .write_volatile((u32::from(speed_id) << 20) | (1 << 27));
            pointer
                .add(input_slot + 4)
                .cast::<u32>()
                .write_volatile(u32::from(port) << 16);
            pointer
                .add(input_ep0 + 4)
                .cast::<u32>()
                .write_volatile((u32::from(packet) << 16) | (4 << 3) | (3 << 1));
            pointer
                .add(input_ep0 + 8)
                .cast::<u64>()
                .write_volatile((base + EP0_RING as u64) | 1);
            pointer.add(input_ep0 + 16).cast::<u32>().write_volatile(8);
            put_trb(
                pointer,
                EP0_RING + COMMAND_SLOTS * TRB_BYTES,
                link_trb(base + EP0_RING as u64, true),
            );
        }
        Ok(())
    }

    fn evaluate_packet_size(&mut self, packet: u16) -> Result<(), XhciError> {
        let dma = self.device_dma.as_ref().unwrap();
        let input = INPUT_CONTEXT + self.layout.unwrap().context_size * 2;
        // SAFETY: Prior control transfer completed, no command uses input context.
        // Evaluate Context only reads selected EP0 Max Packet Size fields.
        unsafe {
            dma.as_ptr()
                .add(INPUT_CONTEXT + 4)
                .cast::<u32>()
                .write_volatile(2);
            dma.as_ptr()
                .add(input + 4)
                .cast::<u32>()
                .write_volatile((u32::from(packet) << 16) | (4 << 3) | (3 << 1));
        }
        fence(Ordering::SeqCst);
        self.submit_command(
            [
                (dma.physical_address() + INPUT_CONTEXT as u64) as u32,
                0,
                0,
                (13 << 10) | (u32::from(self.slot) << 24),
            ],
            Some(self.slot),
        )?;
        Ok(())
    }

    fn reset_usb2_port(&mut self, attached: Attached) -> Result<(UsbSpeed, u8), XhciError> {
        let register = PORTSC + u64::from(attached.port - 1) * 16;
        self.debounce_usb2_port(attached.port)?;
        let status = self.op_read(register);
        if status & (1 << 9) == 0 {
            return Err(XhciError::InvalidDeviceState);
        }
        self.op_write(register, (status & PORT_PRESERVE) | (status & PORT_CHANGES));
        // PED is RW1C: writing back a whole PORTSC value would disable the port.
        // Preserve only defined RWS fields and issue the PR action explicitly.
        self.op_write(
            register,
            (self.op_read(register) & PORT_PRESERVE) | (1 << 4),
        );
        let start = clock::snapshot()
            .ok_or(XhciError::ClockRequired)?
            .monotonic_ns;
        let mut completed = false;
        for poll in 0..POLL_LIMIT {
            let status = self.op_read(register);
            if status & 1 == 0 || status & (1 << 3) != 0 {
                return Err(XhciError::DeviceDisconnected);
            }
            if status & (1 << 4) == 0 && status & (1 << 21) != 0 {
                if status & 2 == 0 {
                    return Err(XhciError::InvalidDeviceState);
                }
                self.op_write(register, (status & PORT_PRESERVE) | (status & PORT_CHANGES));
                completed = true;
                break;
            }
            if timed_out(Some(start), poll, RESET_NS) {
                break;
            }
            core::hint::spin_loop();
        }
        if !completed {
            return Err(XhciError::Timeout);
        }
        self.device_delay(RESET_RECOVERY_NS, attached.port)?;
        let status = self.op_read(register);
        if status & 3 != 3 || (status >> 5) & 15 != 0 {
            return Err(XhciError::InvalidDeviceState);
        }
        let speed_id = ((status >> 10) & 15) as u8;
        let speed =
            attached.protocol.speeds[usize::from(speed_id)].ok_or(XhciError::InvalidDeviceState)?;
        Ok((speed, speed_id))
    }

    fn debounce_usb2_port(&self, port: u8) -> Result<(), XhciError> {
        let start = clock::snapshot()
            .ok_or(XhciError::ClockRequired)?
            .monotonic_ns;
        let mut stable_since = None;
        for _ in 0..POLL_LIMIT {
            let now = clock::snapshot()
                .ok_or(XhciError::ClockRequired)?
                .monotonic_ns;
            let status = self.op_read(PORTSC + u64::from(port - 1) * 16);
            if status & (1 << 3) != 0 {
                return Err(XhciError::DeviceDisconnected);
            }
            // HCRST can temporarily clear CCS while the powered port detects
            // its existing device again. Start debounce only once PP+CCS hold;
            // a transient disconnect restarts it within the same finite wait.
            if status & ((1 << 9) | 1) == ((1 << 9) | 1) {
                let stable = *stable_since.get_or_insert(now);
                if now.saturating_sub(stable) >= DEBOUNCE_NS {
                    return Ok(());
                }
            } else {
                stable_since = None;
            }
            if now.saturating_sub(start) >= RESET_NS {
                break;
            }
            core::hint::spin_loop();
        }
        Err(XhciError::Timeout)
    }

    fn device_delay(&self, duration: u64, port: u8) -> Result<(), XhciError> {
        let start = clock::snapshot()
            .ok_or(XhciError::ClockRequired)?
            .monotonic_ns;
        for _ in 0..POLL_LIMIT {
            let status = self.op_read(PORTSC + u64::from(port - 1) * 16);
            if status & 1 == 0 || status & (1 << 3) != 0 {
                return Err(XhciError::DeviceDisconnected);
            }
            if clock::snapshot()
                .ok_or(XhciError::ClockRequired)?
                .monotonic_ns
                .saturating_sub(start)
                >= duration
            {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(XhciError::Timeout)
    }

    fn get_descriptor(&mut self, kind: u8, output: &mut [u8]) -> Result<usize, XhciError> {
        if !matches!(kind, 1 | 2)
            || output.is_empty()
            || output.len() > usb::MAX_CONFIG_BYTES
            || self.slot == 0
            || !self.live
        {
            return Err(XhciError::InvalidDeviceState);
        }
        let attached = self.attached.unwrap();
        if self.op_read(PORTSC + u64::from(attached.port - 1) * 16) & 3 != 3 {
            return Err(XhciError::DeviceDisconnected);
        }
        let dma = self.device_dma.as_ref().unwrap();
        let base = dma.physical_address();
        let ring = base + EP0_RING as u64;
        let position = self.control_producer;
        debug_assert!(position.is_multiple_of(3) && position + 2 < COMMAND_SLOTS);
        let setup_pointer = ring + (position * TRB_BYTES) as u64;
        let data_pointer = setup_pointer + TRB_BYTES as u64;
        let status_pointer = data_pointer + TRB_BYTES as u64;
        let requested = output.len();
        let cycle = u32::from(self.control_cycle);
        // SAFETY: One control transfer at a time, all previous stages completed.
        // Publish Data/Status/Link before Setup so HC never sees half a request.
        unsafe {
            let pointer = dma.as_ptr();
            for index in 0..requested {
                pointer.add(DEVICE_DATA + index).write_volatile(0);
            }
            if position + 3 == COMMAND_SLOTS {
                put_trb(
                    pointer,
                    EP0_RING + COMMAND_SLOTS * TRB_BYTES,
                    link_trb(ring, self.control_cycle),
                );
            }
            put_trb(
                pointer,
                EP0_RING + (position + 1) * TRB_BYTES,
                [
                    (base + DEVICE_DATA as u64) as u32,
                    0,
                    requested as u32,
                    (3 << 10) | (1 << 16) | (1 << 2) | cycle,
                ],
            );
            put_trb(
                pointer,
                EP0_RING + (position + 2) * TRB_BYTES,
                [0, 0, 0, (4 << 10) | (1 << 5) | cycle],
            );
            put_trb(
                pointer,
                EP0_RING + position * TRB_BYTES,
                [
                    0x0680 | (u32::from(kind) << 24),
                    (requested as u32) << 16,
                    8,
                    (2 << 10) | (3 << 16) | (1 << 6) | cycle,
                ],
            );
        }
        fence(Ordering::SeqCst);
        self.write(self.layout.unwrap().doorbell + u64::from(self.slot) * 4, 1);
        let start = clock::snapshot()
            .ok_or(XhciError::ClockRequired)?
            .monotonic_ns;
        let mut actual = requested;
        let mut short = false;
        let mut events = 0;
        for poll in 0..POLL_LIMIT {
            if self.op_read(USBSTS) & (HALTED | HOST_ERROR | CONTROLLER_ERROR | NOT_READY) != 0 {
                return Err(XhciError::ControllerNotReady);
            }
            if self.op_read(PORTSC + u64::from(attached.port - 1) * 16) & 3 != 3 {
                return Err(XhciError::DeviceDisconnected);
            }
            if let Some(event) = self.read_event() {
                if (event[3] >> 10) & 0x3f == 34 {
                    port_event(event, self.max_ports())?;
                    self.consume_event();
                    events += 1;
                    if events > 64 {
                        return Err(XhciError::EventStorm);
                    }
                    continue;
                }
                let completion =
                    transfer_event(event, self.slot, data_pointer, status_pointer, requested)?;
                self.consume_event();
                match completion {
                    TransferCompletion::Short(length) => {
                        if short {
                            return Err(XhciError::InvalidEvent);
                        }
                        short = true;
                        actual = length;
                    }
                    TransferCompletion::Status => {
                        fence(Ordering::SeqCst);
                        let pointer = self.device_dma.as_ref().unwrap().as_ptr();
                        for (index, byte) in output[..actual].iter_mut().enumerate() {
                            // SAFETY: Status completion retires the complete USB
                            // transfer, including its data stage; bounded data.
                            *byte = unsafe { pointer.add(DEVICE_DATA + index).read_volatile() };
                        }
                        self.control_transfers += 1;
                        self.short_packets += u64::from(short);
                        self.control_producer += 3;
                        if self.control_producer == COMMAND_SLOTS {
                            self.control_producer = 0;
                            self.control_cycle = !self.control_cycle;
                            self.control_wraps += 1;
                        }
                        return Ok(actual);
                    }
                }
            }
            if timed_out(Some(start), poll, COMMAND_NS) {
                break;
            }
            core::hint::spin_loop();
        }
        Err(XhciError::Timeout)
    }

    fn halt_reset(&mut self) -> Result<(), XhciError> {
        let layout = self.layout.unwrap();
        let result = (|| {
            self.op_write(
                USBCMD,
                self.op_read(USBCMD) & !(RUN | INTERRUPT_ENABLE | SYSTEM_ERROR_ENABLE),
            );
            self.wait_register(layout.operational + USBSTS, HALTED, HALTED, RESET_NS)?;
            self.op_write(USBCMD, RESET);
            if self.controller.as_ref().unwrap().needs_reset_delay() {
                self.intel_reset_delay()?;
            }
            self.wait_register(layout.operational + USBCMD, RESET, 0, RESET_NS)?;
            self.wait_register(layout.operational + USBSTS, NOT_READY, 0, RESET_NS)?;
            if self.op_read(USBSTS) & HALTED == 0 {
                return Err(XhciError::ControllerNotReady);
            }
            Ok(())
        })();
        if result.is_err() {
            self.quarantined = true;
        }
        result
    }

    fn intel_reset_delay(&self) -> Result<(), XhciError> {
        let start = clock::snapshot()
            .ok_or(XhciError::ClockRequired)?
            .monotonic_ns;
        for _ in 0..POLL_LIMIT {
            // Only the separate HPET is accessed during the controller's delay.
            if clock::snapshot()
                .ok_or(XhciError::ClockRequired)?
                .monotonic_ns
                .saturating_sub(start)
                >= INTEL_DELAY_NS
            {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(XhciError::Timeout)
    }

    fn wait_register(
        &self,
        register: u64,
        mask: u32,
        expected: u32,
        timeout: u64,
    ) -> Result<(), XhciError> {
        let start = clock::snapshot().map(|time| time.monotonic_ns);
        for poll in 0..POLL_LIMIT {
            if self.read(register) & mask == expected {
                return Ok(());
            }
            if timed_out(start, poll, timeout) {
                break;
            }
            core::hint::spin_loop();
        }
        Err(XhciError::Timeout)
    }

    /// Acknowledge halt and full reset before releasing any DMA or MMIO. Debug
    /// capability was rejected, so no independent DMA engine survives reset.
    ///
    /// # Safety
    /// Use the original allocator/root before shared-root publication, BSP IF=0.
    pub unsafe fn shutdown(
        mut self,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), XhciError> {
        assert!(!arch::interrupts_enabled());
        self.live = false;
        if self.takeover && (self.quarantined || self.halt_reset().is_err()) {
            // SAFETY: Inhibit future mastering, but retain all possibly live
            // DMA, mappings and the PCI claim for the remainder of this boot.
            let _ = unsafe { self.controller.as_mut().unwrap().disable_bus_master() };
            return Err(XhciError::DmaQuarantined);
        }
        let controller = self.controller.take().unwrap();
        if self.takeover || !controller.is_prepared() {
            // SAFETY: Full reset completed, or no configuration was ever changed.
            unsafe { controller.release() }.map_err(XhciError::Pci)?;
        }
        fence(Ordering::SeqCst);
        if let Some(device_dma) = self.device_dma.take() {
            // SAFETY: Full controller reset retired any addressed slot and EP0
            // transfer, including partial/failing enumeration; BME is disabled.
            unsafe { device_dma.release(frames) }.map_err(XhciError::Dma)?;
        }
        if let Some(scratch) = self.scratch.take() {
            // SAFETY: Reset revoked scratchpad and DCBAA ownership; BME disabled.
            unsafe { scratch.release(frames) }.map_err(XhciError::Dma)?;
        }
        if let Some(dma) = self.dma.take() {
            // SAFETY: HC reset and BME disabled; no CPU pointer escapes this type.
            unsafe { dma.release(frames) }.map_err(XhciError::Dma)?;
        }
        for page in (0..MAX_PAGES).rev() {
            if self.mapped & (1_u64 << page) == 0 {
                continue;
            }
            // SAFETY: Every device and CPU access ended before this private unmap.
            let physical = unsafe { tables.unmap_page(WINDOW + page as u64 * PAGE) }
                .map_err(XhciError::Paging)?;
            assert_eq!(physical, self.physical + page as u64 * PAGE);
        }
        Ok(())
    }

    fn read(&self, register: u64) -> u32 {
        assert!(
            register.is_multiple_of(4)
                && register < MMIO_LIMIT
                && self.mapped & (1 << (register / PAGE)) != 0
        );
        // SAFETY: Checked DWORD alignment and a live private uncached page.
        unsafe { ((WINDOW + register) as *const u32).read_volatile() }
    }
    fn write(&self, register: u64, value: u32) {
        assert!(
            register.is_multiple_of(4)
                && register < MMIO_LIMIT
                && self.mapped & (1 << (register / PAGE)) != 0
        );
        // SAFETY: Checked mapped/aligned register under exclusive device ownership.
        unsafe { ((WINDOW + register) as *mut u32).write_volatile(value) };
    }
    fn write_byte(&self, register: u64, value: u8) {
        assert!(register < MMIO_LIMIT && self.mapped & (1 << (register / PAGE)) != 0);
        // SAFETY: xHCI Legacy Support explicitly permits independent byte writes.
        unsafe { ((WINDOW + register) as *mut u8).write_volatile(value) };
    }
    fn read64(&self, register: u64) -> u64 {
        let low = u64::from(self.read(register));
        if self.layout.unwrap().addresses_64 {
            low | (u64::from(self.read(register + 4)) << 32)
        } else {
            low
        }
    }
    fn write64(&self, register: u64, value: u64) {
        // xHCI 5.1 permits low-DWORD then high-DWORD address writes. DMA32
        // makes the high half zero throughout, including an intermediate write.
        self.write(register, value as u32);
        if self.layout.unwrap().addresses_64 {
            self.write(register + 4, (value >> 32) as u32);
        } else {
            assert_eq!(value >> 32, 0, "xHCI DMA32 pointer overflow");
        }
    }
    fn op_read(&self, register: u64) -> u32 {
        self.read(self.layout.unwrap().operational + register)
    }
    fn op_write(&self, register: u64, value: u32) {
        self.write(self.layout.unwrap().operational + register, value);
    }
}

fn validate_initial_state(command: u32, status: u32) -> Result<(), XhciError> {
    // Refuse full/light reset, VTIO, save/restore in progress, save/restore
    // errors, and any unavailable/failed host before changing firmware state.
    if command & (RESET | (1 << 7) | (1 << 16)) != 0
        || status & (NOT_READY | HOST_ERROR | CONTROLLER_ERROR | (1 << 8) | (1 << 9) | (1 << 10))
            != 0
    {
        return Err(XhciError::ControllerNotReady);
    }
    Ok(())
}

fn timed_out(start: Option<u64>, poll: usize, timeout: u64) -> bool {
    poll.is_multiple_of(256)
        && start.is_some_and(|start| {
            clock::snapshot().is_some_and(|now| now.monotonic_ns.saturating_sub(start) >= timeout)
        })
}

fn validate_page(physical: u64, entries: &[&Entry]) -> Result<(), XhciError> {
    let end = physical.checked_add(PAGE).ok_or(XhciError::InvalidMmio)?;
    // This is an MMIO address, not a DMA allocation. Firmware may place it
    // above 4GiB; map_device_page separately enforces CPUID's physical width.
    if physical == 0 || !physical.is_multiple_of(PAGE) || end > 1 << 52 {
        return Err(XhciError::InvalidMmio);
    }
    for entry in entries {
        let limit = entry
            .base
            .checked_add(entry.length)
            .ok_or(XhciError::InvalidMmio)?;
        if entry.base < end && physical < limit && entry.entry_type != EntryType::RESERVED {
            return Err(XhciError::MemoryOverlap);
        }
    }
    Ok(())
}

fn validate_dma(physical: u64) -> Result<(), XhciError> {
    if physical == 0
        || !physical.is_multiple_of(PAGE)
        || physical
            .checked_add(3 * PAGE)
            .is_none_or(|end| end > 1 << 32)
    {
        return Err(XhciError::InvalidCapabilities);
    }
    Ok(())
}

fn link_trb(base: u64, cycle: bool) -> [u32; 4] {
    [
        base as u32,
        (base >> 32) as u32,
        0,
        (6 << 10) | 2 | u32::from(cycle),
    ]
}

/// Caller owns the complete target TRB; publish its Cycle/control DWORD last.
unsafe fn put_trb(pointer: *mut u8, offset: usize, trb: [u32; 4]) {
    // SAFETY: Caller bounds/alignment and command-ring ownership contract.
    unsafe {
        for (index, value) in trb[..3].iter().enumerate() {
            pointer
                .add(offset + index * 4)
                .cast::<u32>()
                .write_volatile(*value);
        }
        fence(Ordering::SeqCst);
        pointer
            .add(offset + 12)
            .cast::<u32>()
            .write_volatile(trb[3]);
    }
}

/// True for the matching no-op completion, false for a valid asynchronous port
/// event. Unknown/completion-error events never count as successful commands.
#[cfg(feature = "boot-smoke")]
fn classify_event(event: [u32; 4], command: u64, ports: u8) -> Result<bool, XhciError> {
    command_event(event, command, ports, Some(0)).map(|slot| slot.is_some())
}

fn command_event(
    event: [u32; 4],
    command: u64,
    ports: u8,
    expected_slot: Option<u8>,
) -> Result<Option<u8>, XhciError> {
    let kind = (event[3] >> 10) & 0x3f;
    if kind == 34 {
        port_event(event, ports)?;
        return Ok(None);
    }
    if event[2] >> 24 != 1 {
        return Err(XhciError::CommandFailed);
    }
    let slot = (event[3] >> 24) as u8;
    if kind != 33
        || (u64::from(event[0]) | (u64::from(event[1]) << 32)) != command
        || event[2] & 0x00ff_ffff != 0
        || event[3] & !((0xff << 24) | (0x3f << 10) | 1) != 0
        || expected_slot.map_or(slot != 1, |expected| slot != expected)
    {
        return Err(XhciError::InvalidEvent);
    }
    Ok(Some(slot))
}

fn port_event(event: [u32; 4], ports: u8) -> Result<(), XhciError> {
    if (event[3] >> 10) & 0x3f != 34
        || event[1] != 0
        || event[0] & 0x00ff_ffff != 0
        || !(1..=u32::from(ports)).contains(&(event[0] >> 24))
        || event[2] != 1 << 24
        || event[3] & !((0x3f << 10) | 1) != 0
    {
        return Err(XhciError::InvalidEvent);
    }
    Ok(())
}

enum TransferCompletion {
    Short(usize),
    Status,
}

fn transfer_event(
    event: [u32; 4],
    slot: u8,
    data: u64,
    status: u64,
    requested: usize,
) -> Result<TransferCompletion, XhciError> {
    if (event[3] >> 10) & 0x3f != 32
        || (event[3] >> 24) as u8 != slot
        || (event[3] >> 16) & 31 != 1
        || event[3] & !((0xff << 24) | (31 << 16) | (0x3f << 10) | 1) != 0
    {
        return Err(XhciError::InvalidEvent);
    }
    let pointer = u64::from(event[0]) | (u64::from(event[1]) << 32);
    let residual = (event[2] & 0x00ff_ffff) as usize;
    match event[2] >> 24 {
        13 if pointer == data && residual != 0 && residual <= requested => {
            Ok(TransferCompletion::Short(requested - residual))
        }
        1 if pointer == status && residual == 0 => Ok(TransferCompletion::Status),
        1 | 13 => Err(XhciError::InvalidEvent),
        _ => Err(XhciError::TransferFailed),
    }
}

fn usb2_speed(psi: u32) -> Option<UsbSpeed> {
    // USB2 uses symmetric, half-duplex USB link protocol entries.
    if psi & ((3 << 6) | (1 << 8) | (3 << 14)) != 0 {
        return None;
    }
    let scale = match (psi >> 4) & 3 {
        0 => 1,
        1 => 1_000,
        2 => 1_000_000,
        _ => 1_000_000_000,
    };
    match u64::from(psi >> 16) * scale {
        1_500_000 => Some(UsbSpeed::Low),
        12_000_000 => Some(UsbSpeed::Full),
        480_000_000 => Some(UsbSpeed::High),
        _ => None,
    }
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    assert!(validate_initial_state(0, HALTED).is_ok());
    assert!(validate_initial_state(RUN, 0).is_ok());
    for command in [RESET, 1 << 7, 1 << 16] {
        assert!(validate_initial_state(command, HALTED).is_err());
    }
    for status in [
        NOT_READY,
        HOST_ERROR,
        CONTROLLER_ERROR,
        1 << 8,
        1 << 9,
        1 << 10,
    ] {
        assert!(validate_initial_state(0, status).is_err());
    }
    let layout = Layout::parse(
        0x0100_0040,
        (8 << 24) | (16 << 8) | 64,
        0,
        0x0200_0001,
        0x2000,
        0x1000,
    )
    .unwrap();
    assert_eq!((layout.ports, layout.slots, layout.scratchpads), (8, 64, 0));
    assert_eq!(layout.context_size, 32);
    assert_eq!(
        Layout::parse(0x0100_0040, 0x0800_0140, 0, 4, 0x2000, 0x1000)
            .unwrap()
            .context_size,
        64
    );
    assert!(
        Layout::parse(
            0x0100_0040,
            (1 << 24) | (1025 << 8) | 1,
            0,
            0,
            0x20000,
            0x1000
        )
        .is_err()
    );
    assert!(layout.extended_range(0x800, 16).is_ok());
    assert!(layout.extended_range(0x20, 16).is_ok());
    assert!(layout.extended_range(0x40, 4).is_err());
    assert!(layout.extended_range(MMIO_LIMIT - 4, 8).is_err());
    for invalid in [0, 1, 0x0900_0040, 0x0100_0000, 0x0100_0041] {
        assert!(Layout::parse(invalid, 0x0800_0140, 0, 0, 0x2000, 0x1000).is_err());
    }
    assert!(Layout::parse(0x0100_0040, 0x0800_0140, 5 << 27, 0, 0x2000, 0x1000).is_err());
    assert!(Layout::parse(0x0100_0040, 0x0800_0140, 1 << 21, 0, 0x2000, 0x1000).is_err());
    assert_eq!(
        Layout::parse(0x0100_0040, 0x0800_0140, 4 << 27, 0, 0x2000, 0x1000)
            .unwrap()
            .scratchpads,
        4
    );
    for (doorbell, runtime) in [
        (0x2001, 0x1000),
        (0x2000, 0x1004),
        (0x1000, 0x1000),
        (0x40, 0x1000),
    ] {
        assert!(Layout::parse(0x0100_0040, 0x0800_0140, 0, 0, doorbell, runtime).is_err());
    }
    assert_eq!(link_trb(0x2000, true), [0x2000, 0, 0, 0x1803]);
    assert_eq!(link_trb(0x2000, false)[3], 0x1802);
    let complete = [0x20f0, 0, 1 << 24, (33 << 10) | 1];
    assert!(classify_event(complete, 0x20f0, 8).unwrap());
    assert!(classify_event(complete, 0x20e0, 8).is_err());
    assert!(!classify_event([2 << 24, 0, 1 << 24, (34 << 10) | 1], 0x20f0, 8).unwrap());
    for event in [
        [0x20f0, 0, 2 << 24, 33 << 10],
        [0x20f0, 1, 1 << 24, 33 << 10],
        [0, 0, 1 << 24, 34 << 10],
        [9 << 24, 0, 1 << 24, 34 << 10],
        [0x20f0, 0, (1 << 24) | 1, 33 << 10],
        [0x20f0, 0, 1 << 24, (33 << 10) | (1 << 24)],
    ] {
        assert!(classify_event(event, 0x20f0, 8).is_err());
    }
    // CONFIG enables exactly slot 1. An otherwise valid Enable Slot event may
    // never select an out-of-range DCBAA entry or device doorbell.
    let enabled = [0x20f0, 0, 1 << 24, (33 << 10) | (1 << 24)];
    assert_eq!(command_event(enabled, 0x20f0, 8, None).unwrap(), Some(1));
    for slot in [0, 2, 255] {
        let mut invalid = enabled;
        invalid[3] = (33 << 10) | (slot << 24);
        assert!(command_event(invalid, 0x20f0, 8, None).is_err());
    }
    let transfer_control = (1 << 24) | (1 << 16) | (32 << 10) | 1;
    let short = [0x4020, 0, (13 << 24) | 46, transfer_control];
    let status = [0x4030, 0, 1 << 24, transfer_control];
    assert!(matches!(
        transfer_event(short, 1, 0x4020, 0x4030, 64),
        Ok(TransferCompletion::Short(18))
    ));
    assert!(matches!(
        transfer_event(status, 1, 0x4020, 0x4030, 64),
        Ok(TransferCompletion::Status)
    ));
    for invalid in [
        [0x4010, 0, short[2], transfer_control], // unrelated TRB
        [0x4020, 1, short[2], transfer_control], // forged high address
        [0x4020, 0, 13 << 24, transfer_control], // no shortfall
        [0x4020, 0, (13 << 24) | 65, transfer_control], // excessive residue
        [0x4030, 0, (1 << 24) | 1, transfer_control], // status has no data
        [0x4020, 0, 1 << 24, transfer_control],  // Data IOC was not requested
        [0x4030, 0, short[2], transfer_control], // shortfall on wrong stage
        [0x4030, 0, 1 << 24, transfer_control ^ (3 << 24)], // wrong slot
        [0x4030, 0, 1 << 24, transfer_control ^ (3 << 16)], // wrong endpoint
        [0x4030, 0, 1 << 24, transfer_control | 4], // Event Data not enabled
        [0x4030, 0, 1 << 24, transfer_control | (1 << 10)], // wrong TRB type
    ] {
        assert!(transfer_event(invalid, 1, 0x4020, 0x4030, 64).is_err());
    }
    assert!(matches!(
        transfer_event(
            [0x4020, 0, 6 << 24, transfer_control],
            1,
            0x4020,
            0x4030,
            64
        ),
        Err(XhciError::TransferFailed)
    ));
    assert_eq!(usb2_speed((1500 << 16) | (1 << 4) | 2), Some(UsbSpeed::Low));
    assert_eq!(usb2_speed((12 << 16) | (2 << 4) | 1), Some(UsbSpeed::Full));
    let high = (480 << 16) | (2 << 4) | 3;
    assert_eq!(usb2_speed(high), Some(UsbSpeed::High));
    for unsupported in [
        high | (1 << 6),
        high | (1 << 8),
        high | (1 << 14),
        (500 << 16) | (2 << 4) | 3,
    ] {
        assert_eq!(usb2_speed(unsupported), None);
    }
    assert_eq!(PORT_PRESERVE & (PORT_CHANGES | (1 << 1) | (1 << 4)), 0);
    assert!(validate_dma(0xffff_d000).is_ok());
    for invalid in [0, 1, 0xffff_e000, 1 << 32, u64::MAX] {
        assert!(validate_dma(invalid).is_err());
    }
    assert!(validate_page(0xfebc_0000, &[]).is_ok());
    assert!(validate_page(0xc0_0000_4000, &[]).is_ok());
    assert!(validate_page(1 << 52, &[]).is_err());
    let ram = Entry {
        base: 0xfebc_0080,
        length: 1,
        entry_type: EntryType::USABLE,
    };
    assert!(validate_page(0xfebc_0000, &[&ram]).is_err());
    crate::serial::println!("MIRROR_XHCI_TESTS_OK");
}
