//! Vendor-independent, read-only AHCI SATA fallback on firmware-assigned PCI.
//!
//! One controller, first active directly attached ATA disk, 512-byte logical
//! sectors, LBA48, one non-NCQ command in slot zero, and polling on the BSP.
//! BIOS-owned controllers are refused. All implemented ports are stopped before
//! controller-wide bus mastering is enabled; no inherited DMA pointer is reused.
//! Unsupported controllers remain unavailable rather than receiving guesses at
//! vendor registers. No RAID mode, ATAPI, hotplug, spin-up, COMRESET, writes,
//! interrupts, power transitions, port multipliers, or IOMMU support yet.
//!
//! Independently implemented against Intel AHCI 1.3.1, sections 2, 3, 4 and 10:
//! https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/serial-ata-ahci-spec-rev1-3-1.pdf
//! ATA IDENTIFY layout and READ DMA EXT command definitions:
//! https://github.com/qemu/qemu/blob/v10.1.0/hw/ide/core.c

use core::{
    fmt,
    sync::atomic::{Ordering, fence},
};
use limine::memory_map::{Entry, EntryType};
use mirror_acpi::{Hpet, Topology};

use crate::{
    arch,
    boot::BootInfo,
    clock,
    dma::{DmaBuffer, DmaError},
    memory::FrameAllocator,
    paging::{PageTables, PagingError},
    pci::{AhciController, Inventory, LegacyError},
};

pub const SECTOR_SIZE: usize = 512;
const PAGE: u64 = 4096;
const WINDOW: u64 = 0xffff_d300_0018_0000;
const MAX_PAGES: usize = 2;
const PORT_START: u64 = 0x100;
const PORT_STRIDE: u64 = 0x80;
const CAP: u64 = 0;
const GHC: u64 = 4;
const HOST_IS: u64 = 8;
const PI: u64 = 0x0c;
const VERSION: u64 = 0x10;
const CAP2: u64 = 0x24;
const BOHC: u64 = 0x28;
const CLB: u64 = 0;
const CLBU: u64 = 4;
const FB: u64 = 8;
const FBU: u64 = 0x0c;
const IS: u64 = 0x10;
const IE: u64 = 0x14;
const CMD: u64 = 0x18;
const TFD: u64 = 0x20;
const SIG: u64 = 0x24;
const SSTS: u64 = 0x28;
const SERR: u64 = 0x30;
const SACT: u64 = 0x34;
const CI: u64 = 0x38;
const FBS: u64 = 0x40;
const ST: u32 = 1;
const FRE: u32 = 1 << 4;
const FR: u32 = 1 << 14;
const CR: u32 = 1 << 15;
const AHCI_ENABLE: u32 = 1 << 31;
const IRQ_ENABLE: u32 = 1 << 1;
const TASK_BUSY: u32 = 0x80 | 0x08;
const TASK_ERROR: u32 = 0x01 | 0x20;
const PORT_ERRORS: u32 = (1 << 30) | (1 << 29) | (1 << 28) | (1 << 27) | (1 << 26) | (1 << 24);
const ATA_SIGNATURE: u32 = 0x0000_0101;
const FIS_OFFSET: usize = 1024;
const TABLE_OFFSET: usize = 1280;
const DATA_OFFSET: usize = 1536;
const PRDT_OFFSET: usize = TABLE_OFFSET + 128;
const POLL_LIMIT: usize = 50_000_000;
const STOP_NS: u64 = 500_000_000;
const REQUEST_NS: u64 = 5_000_000_000;
const _: () = assert!(DATA_OFFSET + SECTOR_SIZE <= PAGE as usize);
const _: () = assert!(WINDOW >= crate::heap::HEAP_START + crate::heap::HEAP_MAX_SIZE as u64 + PAGE);

#[cfg(feature = "boot-smoke")]
static INIT_FAILURE_INJECTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[derive(Debug)]
pub enum AhciError {
    Pci(LegacyError),
    Dma(DmaError),
    Paging(PagingError),
    InvalidMmio,
    MemoryOverlap,
    CachedAlias,
    ControllerOverlap,
    UnsupportedVersion,
    InvalidPorts,
    BiosOwned,
    ControllerNotReady,
    EngineTimeout,
    DeviceNotReady,
    InvalidIdentify,
    UnsupportedSectorSize,
    SectorOutOfRange,
    IoError,
    InvalidTransfer,
    Timeout,
    DmaQuarantined,
    #[cfg(feature = "boot-smoke")]
    InjectedInitialization,
}

impl fmt::Display for AhciError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pci(error) => write!(output, "PCI claim: {error:?}"),
            Self::Dma(error) => write!(output, "DMA allocation: {error:?}"),
            Self::Paging(error) => write!(output, "MMIO mapping: {error:?}"),
            other => write!(output, "{other:?}"),
        }
    }
}

/// No Drop path frees DMA or releases a possibly active controller claim.
#[must_use = "explicitly shut down AHCI, or retain its quarantined DMA"]
pub struct Driver {
    controller: Option<AhciController>,
    physical: u64,
    offset: u64,
    mapped: usize,
    implemented: u32,
    takeover: bool,
    dma: Option<DmaBuffer>,
    selected: u8,
    capacity: u64,
    live: bool,
    quarantined: bool,
}

impl Driver {
    /// Take ownership before AP startup or private-root publication, with IF=0.
    ///
    /// # Safety
    /// The sole real allocator and boot HHDM must satisfy DmaBuffer::allocate.
    /// Only firmware-assigned MMIO resources and already active SATA links are
    /// supported. None means there is no candidate controller or ATA disk.
    pub unsafe fn open(
        inventory: &Inventory,
        boot: &BootInfo,
        topology: Option<&Topology>,
        hpet: Option<&Hpet>,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<Option<Self>, AhciError> {
        assert!(!arch::interrupts_enabled());
        let Some(controller) = AhciController::discover(inventory).map_err(AhciError::Pci)? else {
            return Ok(None);
        };
        let mut driver = Self {
            physical: controller.abar & !(PAGE - 1),
            offset: controller.abar & (PAGE - 1),
            controller: Some(controller),
            mapped: 0,
            implemented: 0,
            takeover: false,
            dma: None,
            selected: 0,
            capacity: 0,
            live: false,
            quarantined: false,
        };
        // SAFETY: The caller supplies exclusive boot ownership and live RAM.
        let result = unsafe { driver.initialize(boot, topology, hpet, tables, frames) };
        match result {
            Ok(true) => Ok(Some(driver)),
            Ok(false) => {
                // SAFETY: No caller has received the driver or its DMA pointers.
                unsafe { driver.shutdown(tables, frames)? };
                Ok(None)
            }
            Err(error) => {
                // Cleanup failure takes precedence: never imply safe reclamation
                // when an engine or PCI command could not be put in a safe state.
                // SAFETY: The unpublished driver still exclusively owns resources.
                unsafe { driver.shutdown(tables, frames)? };
                Err(error)
            }
        }
    }

    unsafe fn initialize(
        &mut self,
        boot: &BootInfo,
        topology: Option<&Topology>,
        hpet: Option<&Hpet>,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<bool, AhciError> {
        self.map_next(boot, topology, hpet, tables, frames)?;
        let version = self.read(VERSION);
        if !(0x0001_0000..=0x0001_0301).contains(&version) {
            return Err(AhciError::UnsupportedVersion);
        }
        // PCI memory decode is already enabled by firmware. Refuse inherited
        // legacy mode or an in-progress reset rather than switching interfaces.
        if self.read(GHC) & (AHCI_ENABLE | 1) != AHCI_ENABLE {
            return Err(AhciError::ControllerNotReady);
        }
        let capability = self.read(CAP);
        self.implemented = self.read(PI);
        let length = register_span(capability, self.implemented)?;
        let needed = (self.offset + length).div_ceil(PAGE) as usize;
        if needed > MAX_PAGES {
            return Err(AhciError::InvalidMmio);
        }
        while self.mapped < needed {
            self.map_next(boot, topology, hpet, tables, frames)?;
        }
        if version >= 0x0001_0200 && self.read(CAP2) & 1 != 0 {
            let handoff = self.read(BOHC);
            if handoff & (1 | (1 << 4)) != 0 {
                return Err(AhciError::BiosOwned);
            }
            // OOS only; do not acknowledge W1C OOC or enable firmware SMIs.
            self.write(BOHC, 1 << 1);
            if self.read(BOHC) & (1 | (1 << 1) | (1 << 4)) != 1 << 1 {
                return Err(AhciError::BiosOwned);
            }
        }
        // Only now may PCI DMA/interrupt permissions change: firmware has
        // released ownership, AHCI mode is active, and every port is mapped.
        // SAFETY: Claim owns the whole controller; inherited memory stays reserved.
        unsafe { self.controller.as_mut().unwrap().prepare() }.map_err(AhciError::Pci)?;
        self.takeover = true;
        self.write(GHC, (self.read(GHC) | AHCI_ENABLE) & !IRQ_ENABLE);
        if self.read(GHC) & (AHCI_ENABLE | IRQ_ENABLE | 1) != AHCI_ENABLE {
            return Err(AhciError::ControllerNotReady);
        }
        self.stop_all()?;
        self.clear_all_addresses()?;
        let selected = (0..32_u8).find(|&port| {
            self.implemented & (1_u32 << port) != 0
                && self.port_read(port, SSTS) & 0x0f0f == 0x0103
                && self.port_read(port, SIG) == ATA_SIGNATURE
        });
        let Some(selected) = selected else {
            return Ok(false);
        };
        self.selected = selected;
        if self.port_read(selected, CMD) & (1 << 17) != 0
            || (capability & (1 << 16) != 0 && self.port_read(selected, FBS) & 1 != 0)
        {
            return Err(AhciError::DeviceNotReady);
        }
        self.wait_clear(
            selected,
            TFD,
            TASK_BUSY,
            REQUEST_NS,
            AhciError::DeviceNotReady,
        )?;
        // SAFETY: All ports are stopped and no owned DMA has been published.
        self.dma = Some(
            unsafe { DmaBuffer::allocate(frames, boot.direct_map_offset, 1) }
                .map_err(AhciError::Dma)?,
        );
        let physical = self.dma.as_ref().unwrap().physical_address();
        self.port_write(selected, CLB, physical as u32);
        self.port_write(selected, CLBU, 0);
        self.port_write(selected, FB, (physical + FIS_OFFSET as u64) as u32);
        self.port_write(selected, FBU, 0);
        if self.port_read(selected, CLB) != physical as u32
            || self.port_read(selected, CLBU) != 0
            || self.port_read(selected, FB) != (physical + FIS_OFFSET as u64) as u32
            || self.port_read(selected, FBU) != 0
        {
            return Err(AhciError::ControllerNotReady);
        }
        // SAFETY: Every implemented port is stopped; the only installed pointers
        // name this driver's pinned DMA32 buffer, not any firmware allocation.
        unsafe { self.controller.as_mut().unwrap().enable() }.map_err(AhciError::Pci)?;
        self.start_selected()?;
        self.live = true;
        #[cfg(feature = "boot-smoke")]
        if matches!(
            crate::boot::smoke_mode(),
            crate::boot::SmokeMode::AhciInitFailure
        ) && !INIT_FAILURE_INJECTED.swap(true, Ordering::SeqCst)
        {
            // Both engines can already DMA. Returning through open's normal
            // cleanup proves that initialization failure revokes live resources.
            return Err(AhciError::InjectedInitialization);
        }
        self.command(0xec, 0)?;
        let mut identify = [0_u8; SECTOR_SIZE];
        self.copy_data(&mut identify);
        self.capacity = identify_capacity(&identify)?;
        Ok(true)
    }

    fn map_next(
        &mut self,
        boot: &BootInfo,
        topology: Option<&Topology>,
        hpet: Option<&Hpet>,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), AhciError> {
        if self.mapped >= MAX_PAGES {
            return Err(AhciError::InvalidMmio);
        }
        let physical = self
            .physical
            .checked_add(self.mapped as u64 * PAGE)
            .ok_or(AhciError::InvalidMmio)?;
        validate_page(physical, boot.memory_map.entries())?;
        let overlaps = |address: u64| address & !(PAGE - 1) == physical;
        if hpet.is_some_and(|hpet| overlaps(hpet.address))
            || topology.is_some_and(|topology| {
                overlaps(topology.lapic_address)
                    || topology.io_apics().iter().any(|io| overlaps(io.address))
            })
        {
            return Err(AhciError::ControllerOverlap);
        }
        let direct = boot
            .direct_map_offset
            .checked_add(physical)
            .ok_or(AhciError::InvalidMmio)?;
        if tables
            .translate(direct)
            .map_err(AhciError::Paging)?
            .is_some()
        {
            return Err(AhciError::CachedAlias);
        }
        if let Some(framebuffer) = &boot.framebuffer {
            let base = tables
                .translate(framebuffer.addr() as u64)
                .map_err(AhciError::Paging)?
                .ok_or(AhciError::InvalidMmio)?;
            let length = framebuffer
                .pitch()
                .checked_mul(framebuffer.height())
                .ok_or(AhciError::InvalidMmio)?;
            let end = base.checked_add(length).ok_or(AhciError::InvalidMmio)?;
            if physical < end && base < physical + PAGE {
                return Err(AhciError::MemoryOverlap);
            }
        }
        // SAFETY: Whole page excludes RAM, framebuffer, ACPI controllers and
        // cached HHDM aliases; no private roots or APs exist, caller owns PCI.
        unsafe { tables.map_device_page(WINDOW + self.mapped as u64 * PAGE, physical, frames) }
            .map_err(AhciError::Paging)?;
        self.mapped += 1;
        Ok(())
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.capacity
    }
    pub fn port(&self) -> u8 {
        self.selected
    }

    /// Output is unchanged unless a complete 512-byte command succeeded.
    pub fn read_sector(
        &mut self,
        sector: u64,
        output: &mut [u8; SECTOR_SIZE],
    ) -> Result<(), AhciError> {
        assert!(!arch::interrupts_enabled());
        if sector >= self.capacity || sector >= 1 << 48 {
            return Err(AhciError::SectorOutOfRange);
        }
        self.command(0x25, sector)?;
        self.copy_data(output);
        Ok(())
    }

    fn command(&mut self, command: u8, sector: u64) -> Result<(), AhciError> {
        if !self.live || self.quarantined {
            return Err(AhciError::DeviceNotReady);
        }
        let result = self.issue(command, sector);
        if result.is_err() {
            self.live = false;
            // A task-file error may leave CI asserted. Stop both engines before
            // touching descriptors again; ST=0 makes AHCI clear CI and SACT.
            if self.stop_port(self.selected).is_err() {
                self.quarantined = true;
                // SAFETY: Inhibiting new DMA is not treated as proof of quiescence.
                let _ = unsafe { self.controller.as_mut().unwrap().disable_bus_master() };
                return Err(AhciError::DmaQuarantined);
            }
            if matches!(result, Err(AhciError::IoError)) && self.start_selected().is_ok() {
                self.live = true;
            }
        }
        result
    }

    fn issue(&mut self, command: u8, sector: u64) -> Result<(), AhciError> {
        let port = self.selected;
        if self.port_read(port, CI) != 0
            || self.port_read(port, SACT) != 0
            || self.port_read(port, SSTS) & 0x0f0f != 0x0103
            || self.port_read(port, CMD) & (ST | FRE) != ST | FRE
        {
            return Err(AhciError::DeviceNotReady);
        }
        self.wait_clear(port, TFD, TASK_BUSY, REQUEST_NS, AhciError::DeviceNotReady)?;
        let dma = self.dma.as_ref().unwrap();
        let header = command_header(dma.physical_address())?;
        let fis = command_fis(command, sector)?;
        // SAFETY: Slot zero has completed and no other slot was issued. The
        // received-FIS area stays untouched while its independent engine runs.
        unsafe {
            let pointer = dma.as_ptr();
            for (index, value) in header.iter().enumerate() {
                pointer.add(index * 4).cast::<u32>().write_volatile(*value);
            }
            for offset in TABLE_OFFSET..DATA_OFFSET + SECTOR_SIZE {
                pointer.add(offset).write_volatile(0);
            }
            for (index, value) in fis.iter().enumerate() {
                pointer.add(TABLE_OFFSET + index).write_volatile(*value);
            }
            pointer
                .add(PRDT_OFFSET)
                .cast::<u32>()
                .write_volatile((dma.physical_address() + DATA_OFFSET as u64) as u32);
            pointer
                .add(PRDT_OFFSET + 12)
                .cast::<u32>()
                .write_volatile(SECTOR_SIZE as u32 - 1);
        }
        self.clear_status(port);
        fence(Ordering::SeqCst);
        self.port_write(port, CI, 1);
        let start = clock::snapshot().map(|time| time.monotonic_ns);
        for poll in 0..POLL_LIMIT {
            let status = self.port_read(port, IS);
            if status & (PORT_ERRORS & !(1 << 30)) != 0 {
                return Err(AhciError::InvalidTransfer);
            }
            if status & (1 << 30) != 0 {
                return Err(AhciError::IoError);
            }
            let pending = self.port_read(port, CI);
            if pending & !1 != 0 {
                return Err(AhciError::InvalidTransfer);
            }
            if pending & 1 == 0 {
                fence(Ordering::SeqCst);
                let status = self.port_read(port, IS);
                if status & (PORT_ERRORS & !(1 << 30)) != 0 {
                    return Err(AhciError::InvalidTransfer);
                }
                if status & (1 << 30) != 0 || self.port_read(port, TFD) & TASK_ERROR != 0 {
                    return Err(AhciError::IoError);
                }
                if self.port_read(port, TFD) & TASK_BUSY != 0 {
                    return Err(AhciError::InvalidTransfer);
                }
                // SAFETY: CI cleared after this command, and coherent DMA has
                // been acquired before reading its bounded command-header count.
                let transferred = unsafe { dma.as_ptr().add(4).cast::<u32>().read_volatile() };
                if transferred != SECTOR_SIZE as u32 {
                    return Err(AhciError::InvalidTransfer);
                }
                return Ok(());
            }
            if deadline(start, poll, REQUEST_NS) {
                break;
            }
            core::hint::spin_loop();
        }
        Err(AhciError::Timeout)
    }

    fn copy_data(&self, output: &mut [u8; SECTOR_SIZE]) {
        let dma = self.dma.as_ref().unwrap();
        for (index, byte) in output.iter_mut().enumerate() {
            // SAFETY: A successful completed command owns this data again; the
            // independent received-FIS engine has a disjoint 256-byte range.
            *byte = unsafe { dma.as_ptr().add(DATA_OFFSET + index).read_volatile() };
        }
    }

    fn start_selected(&self) -> Result<(), AhciError> {
        let port = self.selected;
        if self.port_read(port, CMD) & (ST | CR | FRE | FR) != 0
            || self.port_read(port, CI) != 0
            || self.port_read(port, SACT) != 0
            || self.port_read(port, TFD) & TASK_BUSY != 0
        {
            return Err(AhciError::DeviceNotReady);
        }
        self.clear_status(port);
        // Clear ATAPI, CLO, ICC and autonomous link-power transitions. Retain
        // firmware spin-up/power-on state; only already-active links are used.
        let command = self.port_read(port, CMD)
            & !((1 << 24) | (1 << 3) | (15 << 28) | (1 << 26) | (1 << 27));
        self.port_write(port, CMD, command | FRE);
        if self.port_read(port, CMD) & (FRE | (1 << 26) | (1 << 27)) != FRE {
            return Err(AhciError::ControllerNotReady);
        }
        self.port_write(port, CMD, command | FRE | ST);
        if self.port_read(port, CMD) & (ST | FRE) != ST | FRE {
            return Err(AhciError::ControllerNotReady);
        }
        Ok(())
    }

    fn stop_port(&self, port: u8) -> Result<(), AhciError> {
        self.port_write(port, IE, 0);
        if self.port_read(port, IE) != 0 {
            return Err(AhciError::ControllerNotReady);
        }
        let command = self.port_read(port, CMD) & !(ST | (1 << 3) | (15 << 28));
        self.port_write(port, CMD, command);
        self.wait_clear(port, CMD, ST | CR, STOP_NS, AhciError::EngineTimeout)?;
        self.port_write(
            port,
            CMD,
            self.port_read(port, CMD) & !(FRE | (1 << 3) | (15 << 28)),
        );
        self.wait_clear(
            port,
            CMD,
            ST | CR | FRE | FR,
            STOP_NS,
            AhciError::EngineTimeout,
        )
    }

    fn stop_all(&self) -> Result<(), AhciError> {
        for port in 0..32 {
            if self.implemented & (1_u32 << port) != 0 {
                self.stop_port(port)?;
            }
        }
        Ok(())
    }

    fn clear_all_addresses(&self) -> Result<(), AhciError> {
        for port in 0..32 {
            if self.implemented & (1_u32 << port) == 0 {
                continue;
            }
            for register in [CLB, CLBU, FB, FBU] {
                self.port_write(port, register, 0);
                if self.port_read(port, register) != 0 {
                    return Err(AhciError::ControllerNotReady);
                }
            }
            self.clear_status(port);
        }
        Ok(())
    }

    fn clear_status(&self, port: u8) {
        // Acknowledge observed W1C bits only, never invent reserved bits.
        self.port_write(port, SERR, self.port_read(port, SERR));
        self.port_write(port, IS, self.port_read(port, IS));
        self.write(HOST_IS, 1_u32 << port);
    }

    fn wait_clear(
        &self,
        port: u8,
        register: u64,
        mask: u32,
        timeout: u64,
        error: AhciError,
    ) -> Result<(), AhciError> {
        let start = clock::snapshot().map(|time| time.monotonic_ns);
        for poll in 0..POLL_LIMIT {
            if self.port_read(port, register) & mask == 0 {
                return Ok(());
            }
            if deadline(start, poll, timeout) {
                break;
            }
            core::hint::spin_loop();
        }
        Err(error)
    }

    /// Stop every engine and revoke DMA pointers before reclaiming memory. PCI
    /// remains bus-master disabled, with IRQs suppressed. If quiescence cannot
    /// be proven, retain the claim, MMIO mappings and every DMA frame forever.
    ///
    /// # Safety
    /// Caller still owns the original allocator/root on the boot BSP with IF=0.
    pub unsafe fn shutdown(
        mut self,
        tables: &mut PageTables,
        frames: &mut FrameAllocator<'_>,
    ) -> Result<(), AhciError> {
        assert!(!arch::interrupts_enabled());
        self.live = false;
        if self.takeover {
            self.write(GHC, (self.read(GHC) | AHCI_ENABLE) & !IRQ_ENABLE);
            if self.quarantined || self.stop_all().is_err() || self.clear_all_addresses().is_err() {
                // SAFETY: Bus-master inhibition does not release live DMA.
                let _ = unsafe { self.controller.as_mut().unwrap().disable_bus_master() };
                return Err(AhciError::DmaQuarantined);
            }
        }
        let controller = self.controller.take().unwrap();
        if self.takeover || !controller.is_prepared() {
            // SAFETY: All engines stopped and DMA pointers cleared, or no
            // configuration was ever written. Otherwise the claim stays held.
            unsafe { controller.release() }.map_err(AhciError::Pci)?;
        }
        if let Some(dma) = self.dma.take() {
            fence(Ordering::SeqCst);
            // SAFETY: CR and FR stopped on all ports, pointers cleared, BM off.
            unsafe { dma.release(frames) }.map_err(AhciError::Dma)?;
        }
        while self.mapped != 0 {
            self.mapped -= 1;
            // SAFETY: No device/Rust access remains and no shared roots exist.
            let physical = unsafe { tables.unmap_page(WINDOW + self.mapped as u64 * PAGE) }
                .map_err(AhciError::Paging)?;
            assert_eq!(physical, self.physical + self.mapped as u64 * PAGE);
        }
        Ok(())
    }

    fn read(&self, register: u64) -> u32 {
        assert!(
            register.is_multiple_of(4) && self.offset + register + 4 <= self.mapped as u64 * PAGE
        );
        // SAFETY: Private live UC mapping, checked alignment and full DWORD bounds.
        unsafe { ((WINDOW + self.offset + register) as *const u32).read_volatile() }
    }
    fn write(&self, register: u64, value: u32) {
        assert!(
            register.is_multiple_of(4) && self.offset + register + 4 <= self.mapped as u64 * PAGE
        );
        // SAFETY: Private live UC mapping and exclusive whole-controller ownership.
        unsafe { ((WINDOW + self.offset + register) as *mut u32).write_volatile(value) };
    }
    fn port_read(&self, port: u8, register: u64) -> u32 {
        assert!(port < 32 && self.implemented & (1_u32 << port) != 0 && register < PORT_STRIDE);
        self.read(PORT_START + u64::from(port) * PORT_STRIDE + register)
    }
    fn port_write(&self, port: u8, register: u64, value: u32) {
        assert!(port < 32 && self.implemented & (1_u32 << port) != 0 && register < PORT_STRIDE);
        self.write(PORT_START + u64::from(port) * PORT_STRIDE + register, value);
    }
}

fn deadline(start: Option<u64>, poll: usize, timeout: u64) -> bool {
    poll.is_multiple_of(256)
        && start.is_some_and(|start| {
            clock::snapshot().is_some_and(|now| now.monotonic_ns.saturating_sub(start) >= timeout)
        })
}

fn register_span(capability: u32, implemented: u32) -> Result<u64, AhciError> {
    if implemented.count_ones() > (capability & 31) + 1 {
        return Err(AhciError::InvalidPorts);
    }
    Ok(PORT_START + u64::from(32 - implemented.leading_zeros()) * PORT_STRIDE)
}

fn validate_page(physical: u64, entries: &[&Entry]) -> Result<(), AhciError> {
    let end = physical.checked_add(PAGE).ok_or(AhciError::InvalidMmio)?;
    if physical == 0 || !physical.is_multiple_of(PAGE) || end > 1 << 32 {
        return Err(AhciError::InvalidMmio);
    }
    for entry in entries {
        let limit = entry
            .base
            .checked_add(entry.length)
            .ok_or(AhciError::InvalidMmio)?;
        if entry.base < end && physical < limit && entry.entry_type != EntryType::RESERVED {
            return Err(AhciError::MemoryOverlap);
        }
    }
    Ok(())
}

fn command_header(physical: u64) -> Result<[u32; 8], AhciError> {
    if physical == 0
        || !physical.is_multiple_of(PAGE)
        || physical.checked_add(PAGE).is_none_or(|end| end > 1 << 32)
    {
        return Err(AhciError::InvalidTransfer);
    }
    // CFL=5 DWORDs, device-to-host, one PRD. CTBA requires 128-byte alignment.
    Ok([
        5 | (1 << 16),
        0,
        (physical + TABLE_OFFSET as u64) as u32,
        0,
        0,
        0,
        0,
        0,
    ])
}

fn command_fis(command: u8, sector: u64) -> Result<[u8; 20], AhciError> {
    if !matches!(command, 0xec | 0x25) || sector >= 1 << 48 || (command == 0xec && sector != 0) {
        return Err(AhciError::InvalidTransfer);
    }
    let mut fis = [0; 20];
    fis[0] = 0x27;
    fis[1] = 0x80;
    fis[2] = command;
    if command == 0x25 {
        fis[7] = 1 << 6;
        let bytes = sector.to_le_bytes();
        fis[4..7].copy_from_slice(&bytes[..3]);
        fis[8..11].copy_from_slice(&bytes[3..6]);
        fis[12] = 1;
    }
    Ok(fis)
}

fn identify_capacity(bytes: &[u8; SECTOR_SIZE]) -> Result<u64, AhciError> {
    let word = |index: usize| u16::from_le_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
    if word(0) & (1 << 15) != 0 || word(49) & (3 << 8) != 3 << 8 || word(83) & 0xc400 != 0x4400 {
        return Err(AhciError::InvalidIdentify);
    }
    if bytes[510] == 0xa5 && bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        return Err(AhciError::InvalidIdentify);
    }
    let sector = word(106);
    if sector & 0xc000 == 0x4000 && sector & (1 << 12) != 0 {
        let words = u32::from(word(117)) | (u32::from(word(118)) << 16);
        if words != (SECTOR_SIZE / 2) as u32 {
            return Err(AhciError::UnsupportedSectorSize);
        }
    }
    let capacity = u64::from(word(100))
        | (u64::from(word(101)) << 16)
        | (u64::from(word(102)) << 32)
        | (u64::from(word(103)) << 48);
    if capacity == 0 || capacity > 1 << 48 {
        return Err(AhciError::InvalidIdentify);
    }
    Ok(capacity)
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    assert_eq!(register_span(0, 0).unwrap(), 0x100);
    assert_eq!(register_span(0, 1 << 31).unwrap(), 0x1100);
    assert_eq!(register_span(31, u32::MAX).unwrap(), 0x1100);
    assert!(matches!(register_span(0, 3), Err(AhciError::InvalidPorts)));
    assert_eq!(
        command_header(0x1000).unwrap(),
        [0x1_0005, 0, 0x1500, 0, 0, 0, 0, 0]
    );
    assert_eq!(command_header(0xffff_f000).unwrap()[2], 0xffff_f500);
    for invalid in [0, 1, 0x1001, 1 << 32, u64::MAX] {
        assert!(command_header(invalid).is_err());
    }
    assert_eq!(
        command_fis(0x25, 0x1234_5678_9abc).unwrap(),
        [
            0x27, 0x80, 0x25, 0, 0xbc, 0x9a, 0x78, 0x40, 0x56, 0x34, 0x12, 0, 1, 0, 0, 0, 0, 0, 0,
            0
        ]
    );
    assert_eq!(command_fis(0xec, 0).unwrap()[2], 0xec);
    for (command, sector) in [(0x35, 0), (0xec, 1), (0x25, 1 << 48), (0x25, u64::MAX)] {
        assert!(command_fis(command, sector).is_err());
    }
    let mut bytes = [0_u8; SECTOR_SIZE];
    bytes[98..100].copy_from_slice(&0x0300_u16.to_le_bytes());
    bytes[166..168].copy_from_slice(&0x4400_u16.to_le_bytes());
    bytes[200..208].copy_from_slice(&2048_u64.to_le_bytes());
    assert_eq!(identify_capacity(&bytes).unwrap(), 2048);
    bytes[212..214].copy_from_slice(&0x5000_u16.to_le_bytes());
    bytes[234..238].copy_from_slice(&2048_u32.to_le_bytes());
    assert!(matches!(
        identify_capacity(&bytes),
        Err(AhciError::UnsupportedSectorSize)
    ));
    bytes[234..238].copy_from_slice(&256_u32.to_le_bytes());
    assert_eq!(identify_capacity(&bytes).unwrap(), 2048);
    bytes[510] = 0xa5;
    assert!(identify_capacity(&bytes).is_err());
    bytes[511] = 0_u8.wrapping_sub(bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)));
    assert_eq!(identify_capacity(&bytes).unwrap(), 2048);
    bytes[200..208].copy_from_slice(&0_u64.to_le_bytes());
    assert!(identify_capacity(&bytes).is_err());
    assert!(validate_page(0xfee0_0000, &[]).is_ok());
    for address in [0, 1, 1 << 32, u64::MAX] {
        assert!(validate_page(address, &[]).is_err());
    }
    let ram = Entry {
        base: 0xfee0_0080,
        length: 1,
        entry_type: EntryType::USABLE,
    };
    assert!(matches!(
        validate_page(0xfee0_0000, &[&ram]),
        Err(AhciError::MemoryOverlap)
    ));
    crate::serial::println!("MIRROR_AHCI_TESTS_OK");
}
