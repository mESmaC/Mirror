//! Native xHCI command-transport foundation, before USB device enumeration.
//!
//! One firmware-decoded controller, no attached/enabled ports at takeover,
//! zero device slots enabled, one command ring and polled primary event ring.
//! A no-op command exercises real controller DMA without USB transfers. BIOS
//! ownership, debug/unknown extended capabilities and oversized configurations
//! are refused before mutation. No vendor driver code is copied here.
//!
//! Intel xHCI 1.2 sections 4.2, 4.6, 4.9, 4.20, 5, 6.4 and 7:
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
    #[cfg(feature = "boot-smoke")]
    InjectedInitialization,
}

impl fmt::Display for XhciError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pci(error) => write!(output, "PCI claim: {error:?}"),
            Self::Dma(error) => write!(output, "DMA allocation: {error:?}"),
            Self::Paging(error) => write!(output, "MMIO mapping: {error:?}"),
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
}

impl Driver {
    /// Discover and initialize an empty firmware-configured xHCI controller.
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
        if self.controller.as_ref().unwrap().needs_reset_delay() && !clock::is_available() {
            return Err(XhciError::ClockRequired);
        }
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
            layout.doorbell..layout.doorbell + 4,
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
        for port in 0..layout.ports {
            if self.op_read(PORTSC + u64::from(port) * 16) & 3 != 0 {
                return Err(XhciError::ConnectedPorts);
            }
        }
        let legacy = self.inspect_extended(boot, topology, hpet, tables, frames)?;
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
        self.initialize_dma()?;
        fence(Ordering::SeqCst);
        // SAFETY: Full controller reset retired firmware DMA, all new structures
        // are initialized/pinned. No event/command address is published yet.
        unsafe { self.controller.as_mut().unwrap().enable() }.map_err(XhciError::Pci)?;
        let physical = self.dma.as_ref().unwrap().physical_address();
        self.op_write(CONFIG, 0); // No USB device slots can become active.
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
        if self.op_read(CONFIG) & 0xff != 0
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
        let result = self.submit_noop();
        if result.is_err() {
            self.live = false;
        }
        result
    }

    fn submit_noop(&mut self) -> Result<(), XhciError> {
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
                [0, 0, 0, (23 << 10) | u32::from(self.producer_cycle)],
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
            let pointer = self.dma.as_ref().unwrap().as_ptr();
            let event_offset = EVENT_OFFSET + self.consumer * TRB_BYTES;
            // SAFETY: Bounded event TRB control read; matching cycle transfers
            // ownership of its remaining fields from the HC to this consumer.
            let control = unsafe { pointer.add(event_offset + 12).cast::<u32>().read_volatile() };
            if control & 1 == u32::from(self.consumer_cycle) {
                fence(Ordering::SeqCst);
                // SAFETY: Matching event cycle and acquired device writes.
                let event = unsafe {
                    [
                        pointer.add(event_offset).cast::<u32>().read_volatile(),
                        pointer.add(event_offset + 4).cast::<u32>().read_volatile(),
                        pointer.add(event_offset + 8).cast::<u32>().read_volatile(),
                        control,
                    ]
                };
                let completed = classify_event(event, command_pointer, layout.ports)?;
                self.consume_event();
                if completed {
                    self.producer += 1;
                    if self.producer == COMMAND_SLOTS {
                        self.producer = 0;
                        self.producer_cycle = !self.producer_cycle;
                        self.command_wraps += 1;
                    }
                    return Ok(());
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
fn classify_event(event: [u32; 4], command: u64, ports: u8) -> Result<bool, XhciError> {
    let kind = (event[3] >> 10) & 0x3f;
    if event[2] >> 24 != 1 {
        return Err(XhciError::CommandFailed);
    }
    match kind {
        33 if u64::from(event[0]) | (u64::from(event[1]) << 32) == command
            && event[2] & 0x00ff_ffff == 0
            && event[3] & !((0x3f << 10) | 1) == 0 =>
        {
            Ok(true)
        }
        34 if event[1] == 0
            && event[0] & 0x00ff_ffff == 0
            && (1..=u32::from(ports)).contains(&(event[0] >> 24))
            && event[2] & 0x00ff_ffff == 0
            && event[3] & !((0x3f << 10) | 1) == 0 =>
        {
            Ok(false)
        }
        _ => Err(XhciError::InvalidEvent),
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
