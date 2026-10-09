//! Bounded PCI discovery, legacy Virtio and AHCI claims through mechanism 1.
//!
//! One BSP owns CF8/CFC; each address/data transaction preserves the caller's
//! interrupt state. Only firmware-configured segment 0 is visible. No ECAM,
//! extended configuration, ARI, hotplug synchronization, BAR sizing, resource
//! assignment, or IRQ routing. Discovery never writes device configuration.
//! A claimed storage function permits only 16-bit command changes; its
//! transport driver owns device reset and DMA lifetime. No MSI/MSI-X enablement.
//!
//! PCI Local Bus Specification 2.1, sections 3.7.4.1 and 6.2 (PCI-SIG document):
//! https://www.bitsavers.org/pdf/pciSIG/PCI_Local_Bus_Specification_2.1_199506.pdf
//! Virtio 1.2, sections 4.1.2, 4.1.4.10, 4.1.5.1.1.1, 5.2.1, and 5.7.1:
//! https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html
//! AHCI 1.3.1 section 2 (class matching, ABAR and PCI power state):
//! https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/serial-ata-ahci-spec-rev1-3-1.pdf
//! PCI capability IDs, command/status masks, and MSI/MSI-X control masks:
//! https://github.com/torvalds/linux/blob/master/include/uapi/linux/pci_regs.h

use core::sync::atomic::{AtomicBool, Ordering};

use crate::{arch, serial};

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;
const BUS_COUNT: u16 = 256;
const SLOT_COUNT: u8 = 32;
const FUNCTION_COUNT: u8 = 8;
const MAX_FUNCTIONS: u32 = BUS_COUNT as u32 * SLOT_COUNT as u32 * FUNCTION_COUNT as u32;
// ID, class/revision, header, and (only for type-1 bridges) bus-number DWORD.
const MAX_CONFIG_READS: u32 = MAX_FUNCTIONS * 4;
const MAX_RETAINED: usize = 32;
const VIRTIO_VENDOR: u16 = 0x1af4;
const COMMAND_IO: u16 = 1;
const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_MASTER: u16 = 1 << 2;
const COMMAND_INTX_DISABLE: u16 = 1 << 10;
const STATUS_CAPABILITIES: u32 = 1 << 20;
const MAX_CAPABILITIES: usize = (0x100 - 0x40) / 4;
// General legacy header (20 bytes without MSI-X) plus block capacity (8).
// Optional device-specific fields outside this window are not supported.
const LEGACY_PORT_BYTES: u32 = 28;
static LEGACY_CLAIMED: AtomicBool = AtomicBool::new(false);
static AHCI_CLAIMED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bdf {
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bridge {
    pub primary: u8,
    pub secondary: u8,
    pub subordinate: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Device {
    pub bdf: Bdf,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub interface: u8,
    pub revision: u8,
    /// Raw header-type byte; bit 7 is the multifunction indicator.
    pub header: u8,
    /// Raw firmware-assigned bus numbers, only for header layout 1.
    pub bridge: Option<Bridge>,
}

impl Device {
    pub(crate) fn is_ahci(self) -> bool {
        (self.class, self.subclass, self.interface) == (1, 6, 1) && self.header & 0x7f == 0
    }

    fn multifunction(self) -> bool {
        self.header & 0x80 != 0
    }

    fn is_virtio_block(self) -> bool {
        // Accept any revision/class/subsystem IDs, as required by Virtio.
        self.vendor == VIRTIO_VENDOR && matches!(self.device, 0x1001 | 0x1042)
    }

    fn is_virtio_gpu(self) -> bool {
        self.vendor == VIRTIO_VENDOR && self.device == 0x1050
    }
}

pub(crate) struct Inventory {
    /// Counts include all discovered functions, even beyond retained storage.
    pub functions: u32,
    pub bridges: u32,
    pub virtio_block_devices: u32,
    pub display_devices: u32,
    pub virtio_gpu_devices: u32,
    pub config_reads: u32,
    /// First 32 functions in BDF order; diagnostics have the same bound.
    pub devices: [Option<Device>; MAX_RETAINED],
    pub retained: usize,
    pub omitted: u32,
}

impl Inventory {
    const fn new() -> Self {
        Self {
            functions: 0,
            bridges: 0,
            virtio_block_devices: 0,
            display_devices: 0,
            virtio_gpu_devices: 0,
            config_reads: 0,
            devices: [None; MAX_RETAINED],
            retained: 0,
            omitted: 0,
        }
    }

    fn record(&mut self, device: Device) {
        assert!(
            self.functions < MAX_FUNCTIONS,
            "PCI function limit exceeded"
        );
        self.functions += 1;
        self.bridges += u32::from(device.bridge.is_some());
        self.virtio_block_devices += u32::from(device.is_virtio_block());
        self.display_devices += u32::from(device.class == 0x03);
        self.virtio_gpu_devices += u32::from(device.is_virtio_gpu());
        if self.retained < self.devices.len() {
            self.devices[self.retained] = Some(device);
            self.retained += 1;
        } else {
            self.omitted += 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressError {
    Bus,
    Slot,
    Function,
    Register,
}

fn configuration_address(
    bus: u16,
    slot: u8,
    function: u8,
    register: u16,
) -> Result<u32, AddressError> {
    if bus >= BUS_COUNT {
        return Err(AddressError::Bus);
    }
    if slot >= SLOT_COUNT {
        return Err(AddressError::Slot);
    }
    if function >= FUNCTION_COUNT {
        return Err(AddressError::Function);
    }
    // Mechanism 1 exposes only 256 bytes. Reject, never silently round/mask,
    // misaligned or extended-register requests that could select another field.
    if register > 0xfc || !register.is_multiple_of(4) {
        return Err(AddressError::Register);
    }
    Ok(0x8000_0000
        | (u32::from(bus) << 16)
        | (u32::from(slot) << 11)
        | (u32::from(function) << 8)
        | u32::from(register))
}

/// One aligned DWORD from conventional segment-0 configuration space.
/// Invalid slot, function, offset, or alignment fails before any port access.
pub(crate) fn read_config_u32(bdf: Bdf, register: u16) -> Result<u32, LegacyError> {
    let address = configuration_address(u16::from(bdf.bus), bdf.slot, bdf.function, register)
        .map_err(|_| LegacyError::ConfigurationAddress)?;
    Ok(read_configuration_address(address))
}

fn read_configuration_address(address: u32) -> u32 {
    let was_enabled = arch::save_and_disable_interrupts();
    // SAFETY: This module owns CF8/CFC on the sole BSP. Every caller has
    // validated the address, and no fallible code runs while IF is disabled.
    unsafe {
        arch::out32(CONFIG_ADDRESS, address);
        let value = arch::in32(CONFIG_DATA);
        arch::restore_interrupts(was_enabled);
        value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LegacyError {
    ConfigurationAddress,
    DeviceChanged,
    IoBar,
    CapabilityList,
    MsiEnabled,
    MsixEnabled,
    AlreadyClaimed,
    IoNotPrepared,
    CommandRejected,
    MemoryBar,
    MemoryNotEnabled,
    PowerState,
}

/// Exclusive vendor-independent class 01/06/01 claim. As with LegacyBlock,
/// dropping a claim never makes an uncertain DMA controller available again.
pub(crate) struct AhciController {
    pub abar: u64,
    original_command: u16,
    command_address: u32,
    prepared: bool,
}

impl AhciController {
    pub(crate) fn discover(inventory: &Inventory) -> Result<Option<Self>, LegacyError> {
        let Some(device) = inventory
            .devices
            .iter()
            .flatten()
            .find(|device| device.is_ahci())
        else {
            return Ok(None);
        };
        let identity = u32::from(device.vendor) | (u32::from(device.device) << 16);
        let (abar, original_command) =
            ahci_configuration(identity, |register| read_config_u32(device.bdf, register))?;
        let command_address = configuration_address(
            u16::from(device.bdf.bus),
            device.bdf.slot,
            device.bdf.function,
            4,
        )
        .map_err(|_| LegacyError::ConfigurationAddress)?;
        AHCI_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| LegacyError::AlreadyClaimed)?;
        Ok(Some(Self {
            abar,
            original_command,
            command_address,
            prepared: false,
        }))
    }

    /// Enable register decoding with DMA and INTx inhibited. The caller owns
    /// the whole controller and keeps inherited DMA memory reserved.
    pub(crate) unsafe fn prepare(&mut self) -> Result<(), LegacyError> {
        // Even rejected readback may have changed the function. Keep the claim
        // held until transport cleanup proves that it can safely be released.
        self.prepared = true;
        // SAFETY: The exclusive claim and caller own this function.
        unsafe { self.write_command(memory_command(self.original_command, false))? };
        Ok(())
    }

    /// All implemented ports must be stopped, with inherited DMA addresses
    /// retired, before enabling this controller-wide bus-master permission.
    pub(crate) unsafe fn enable(&mut self) -> Result<(), LegacyError> {
        if !self.prepared {
            return Err(LegacyError::IoNotPrepared);
        }
        // SAFETY: Caller has installed only owned DMA in quiesced ports.
        unsafe { self.write_command(memory_command(self.original_command, true)) }
    }

    /// This inhibits new DMA but alone never permits reclaiming existing DMA.
    pub(crate) unsafe fn disable_bus_master(&mut self) -> Result<(), LegacyError> {
        // SAFETY: Sole claimed function; preserve all unrelated command bits.
        unsafe { self.write_command(memory_command(self.original_command, false)) }
    }

    /// Release after all implemented engines stopped and all owned DMA pointers
    /// were cleared, or before prepare has ever touched configuration. Never
    /// restore inherited bus mastering or IRQs after taking hardware ownership.
    pub(crate) unsafe fn release(self) -> Result<(), LegacyError> {
        if self.prepared {
            let command = (self.original_command | COMMAND_INTX_DISABLE) & !COMMAND_MASTER;
            // SAFETY: Caller has retired every engine and device reference.
            unsafe { self.write_command(command)? };
        }
        AHCI_CLAIMED.store(false, Ordering::Release);
        Ok(())
    }

    pub(crate) fn is_prepared(&self) -> bool {
        self.prepared
    }

    unsafe fn write_command(&self, command: u16) -> Result<(), LegacyError> {
        let enabled = arch::save_and_disable_interrupts();
        // SAFETY: WORD access at Command leaves adjacent W1C Status untouched.
        let observed = unsafe {
            arch::out32(CONFIG_ADDRESS, self.command_address);
            arch::out16(CONFIG_DATA, command);
            let observed = arch::in16(CONFIG_DATA);
            arch::restore_interrupts(enabled);
            observed
        };
        if observed == command {
            Ok(())
        } else {
            Err(LegacyError::CommandRejected)
        }
    }
}

fn memory_command(original: u16, master: bool) -> u16 {
    let command = (original | COMMAND_MEMORY | COMMAND_INTX_DISABLE) & !COMMAND_MASTER;
    command | if master { COMMAND_MASTER } else { 0 }
}

fn ahci_configuration(
    identity: u32,
    mut read: impl FnMut(u16) -> Result<u32, LegacyError>,
) -> Result<(u64, u16), LegacyError> {
    if read(0)? != identity || read(8)? >> 8 != 0x01_06_01 || (read(0x0c)? >> 16) & 0x7f != 0 {
        return Err(LegacyError::DeviceChanged);
    }
    let command_status = read(4)?;
    // Read-only firmware-ownership checks require decoding before any command
    // mutation. First fallback deliberately refuses unassigned/inactive HBAs.
    if command_status & u32::from(COMMAND_MEMORY) == 0 {
        return Err(LegacyError::MemoryNotEnabled);
    }
    let previous = read(0x20)?;
    let bar = read(0x24)?;
    // ABAR is a non-prefetchable 32-bit memory BAR. BAR4 must not consume BAR5
    // as its upper half. Firmware has assigned resources; no sizing writes.
    if previous & 7 == 4 || bar & 0xf != 0 || bar & !0x3ff == 0 || bar & 0x3ff != 0 {
        return Err(LegacyError::MemoryBar);
    }
    if command_status & STATUS_CAPABILITIES != 0 {
        let mut next = read(0x34)? as u8;
        let mut visited = 0_u64;
        for _ in 0..MAX_CAPABILITIES {
            if next == 0 {
                return Ok((u64::from(bar), command_status as u16));
            }
            if next < 0x40 || !next.is_multiple_of(4) {
                return Err(LegacyError::CapabilityList);
            }
            let bit = 1_u64 << (next / 4);
            if visited & bit != 0 {
                return Err(LegacyError::CapabilityList);
            }
            visited |= bit;
            let cap = read(u16::from(next))?;
            let control = (cap >> 16) as u16;
            match cap as u8 {
                0x05 if control & 1 != 0 => return Err(LegacyError::MsiEnabled),
                0x11 if control & 0x8000 != 0 => return Err(LegacyError::MsixEnabled),
                0x01 if next > 0xf8 || read(u16::from(next) + 4)? & 3 != 0 => {
                    return Err(LegacyError::PowerState);
                }
                _ => {}
            }
            next = (cap >> 8) as u8;
        }
        if next != 0 {
            return Err(LegacyError::CapabilityList);
        }
    }
    Ok((u64::from(bar), command_status as u16))
}

/// Exclusive claim to the first retained legacy block function. It is neither
/// Clone nor Copy. Dropping it does not release the claim: an unreset device
/// must never silently become available to another DMA owner.
pub(crate) struct LegacyBlock {
    pub io_base: u16,
    pub bdf: Bdf,
    original_command: u16,
    // Keep the validated target private: public diagnostic BDF changes cannot
    // redirect command writes to another function.
    command_address: u32,
    io_prepared: bool,
}

impl LegacyBlock {
    /// Claim a firmware-assigned 1af4:1001, type-0 function. Only the retained
    /// inventory (at most 32 devices) is considered. An absent fixture is None;
    /// the first candidate with invalid configuration returns an error instead
    /// of quietly selecting a different disk. No configuration is written.
    pub(crate) fn discover(inventory: &Inventory) -> Result<Option<Self>, LegacyError> {
        let Some(device) = inventory.devices.iter().flatten().find(|device| {
            device.vendor == VIRTIO_VENDOR && device.device == 0x1001 && device.header & 0x7f == 0
        }) else {
            return Ok(None);
        };
        let bdf = device.bdf;
        // Identity/header/command/BAR/pointer plus at most 48 capability reads.
        let (io_base, original_command) =
            legacy_configuration(|register| read_config_u32(bdf, register))?;
        let command_address = configuration_address(u16::from(bdf.bus), bdf.slot, bdf.function, 4)
            .map_err(|_| LegacyError::ConfigurationAddress)?;
        LEGACY_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| LegacyError::AlreadyClaimed)?;
        Ok(Some(Self {
            io_base,
            bdf,
            original_command,
            command_address,
            io_prepared: false,
        }))
    }

    /// Enable the PIO registers, suppress INTx, and inhibit bus mastering so
    /// the transport can reset before providing its own DMA queue addresses.
    ///
    /// # Safety
    /// The caller must run on the BSP and own this function's transport. Any
    /// previous DMA buffers must remain allocated until reset is acknowledged.
    pub(crate) unsafe fn enable_io_only(&mut self) -> Result<(), LegacyError> {
        let command = io_only_command(self.original_command);
        self.io_prepared = false;
        // SAFETY: The claim and caller establish sole device/config ownership.
        unsafe { self.write_command(command)? };
        self.io_prepared = true;
        Ok(())
    }

    /// Enable bus mastering without enabling any interrupt delivery mode.
    ///
    /// # Safety
    /// The caller must have acknowledged a transport reset and established a
    /// valid DMA ownership protocol. All published buffers must remain pinned
    /// until completion or a subsequently acknowledged device reset.
    pub(crate) unsafe fn enable(&mut self) -> Result<(), LegacyError> {
        if !self.io_prepared {
            return Err(LegacyError::IoNotPrepared);
        }
        // SAFETY: The caller owns the transport and its DMA lifetime.
        unsafe { self.write_command(master_command(self.original_command)) }
    }

    /// Inhibit future bus mastering after a failed reset. This does not prove
    /// that previous DMA completed, so it does not authorize freeing buffers.
    ///
    /// # Safety
    /// The caller must retain all possibly device-owned buffers and this claim
    /// until a later reset is acknowledged, or quarantine them permanently.
    pub(crate) unsafe fn disable_bus_master(&mut self) -> Result<(), LegacyError> {
        let command = read_configuration_address(self.command_address) as u16;
        self.io_prepared = false;
        // SAFETY: Command is from the privately retained claimed function.
        unsafe { self.write_command(command & !COMMAND_MASTER) }
    }

    /// Restore exactly the original command and release the claim on verified
    /// success. A failure leaves the global claim held, even though self is
    /// consumed; a device of uncertain state cannot be claimed again.
    ///
    /// # Safety
    /// The caller must have acknowledged device status zero after all queue
    /// activity, and stopped all transport accesses. No pending DMA may remain.
    pub(crate) unsafe fn restore(self) -> Result<(), LegacyError> {
        // SAFETY: The caller has reset the device before restoring its command.
        unsafe { self.write_command(self.original_command)? };
        LEGACY_CLAIMED.store(false, Ordering::Release);
        Ok(())
    }

    unsafe fn write_command(&self, command: u16) -> Result<(), LegacyError> {
        let was_enabled = arch::save_and_disable_interrupts();
        // SAFETY: The private address is the claimed function's aligned
        // command register at offset 4. A WORD write touches no Status bits
        // (offset 6 contains write-one-to-clear fields). Readback is also WORD.
        let observed = unsafe {
            arch::out32(CONFIG_ADDRESS, self.command_address);
            arch::out16(CONFIG_DATA, command);
            let observed = arch::in16(CONFIG_DATA);
            arch::restore_interrupts(was_enabled);
            observed
        };
        if observed == command {
            Ok(())
        } else {
            Err(LegacyError::CommandRejected)
        }
    }
}

fn io_only_command(original: u16) -> u16 {
    (original | COMMAND_IO | COMMAND_INTX_DISABLE) & !COMMAND_MASTER
}

fn master_command(original: u16) -> u16 {
    original | COMMAND_IO | COMMAND_MASTER | COMMAND_INTX_DISABLE
}

fn legacy_io_base(bar: u32) -> Result<u16, LegacyError> {
    // I/O BAR bit 0 must be set, reserved bit 1 clear. No sizing writes are
    // performed; firmware and the Virtio device contract supply the resource.
    let base = bar & !3;
    if bar & 3 != 1 || base == 0 || base > 0x1_0000 - LEGACY_PORT_BYTES {
        return Err(LegacyError::IoBar);
    }
    Ok(base as u16)
}

fn legacy_configuration(
    mut read: impl FnMut(u16) -> Result<u32, LegacyError>,
) -> Result<(u16, u16), LegacyError> {
    if read(0)? != 0x1001_1af4 || (read(0x0c)? >> 16) & 0x7f != 0 {
        return Err(LegacyError::DeviceChanged);
    }
    let command_status = read(4)?;
    let base = legacy_io_base(read(0x10)?)?;
    if command_status & STATUS_CAPABILITIES != 0 {
        let mut next = read(0x34)? as u8;
        let mut visited = 0_u64;
        // Conventional capabilities occupy DWORD-aligned addresses 40..FC.
        // A bitmap catches cycles; the loop also has an explicit read bound.
        for _ in 0..MAX_CAPABILITIES {
            if next == 0 {
                return Ok((base, command_status as u16));
            }
            if next < 0x40 || !next.is_multiple_of(4) {
                return Err(LegacyError::CapabilityList);
            }
            let bit = 1_u64 << (next / 4);
            if visited & bit != 0 {
                return Err(LegacyError::CapabilityList);
            }
            visited |= bit;
            let capability = read(u16::from(next))?;
            let id = capability as u8;
            let control = (capability >> 16) as u16;
            // INTx Disable does not suppress message-signaled interrupts.
            if id == 0x05 && control & 1 != 0 {
                return Err(LegacyError::MsiEnabled);
            }
            // With MSI-X enabled, legacy device config starts at 24 rather
            // than 20. Refuse it instead of changing routing or port layouts.
            if id == 0x11 && control & 0x8000 != 0 {
                return Err(LegacyError::MsixEnabled);
            }
            next = (capability >> 8) as u8;
        }
        if next != 0 {
            return Err(LegacyError::CapabilityList);
        }
    }
    Ok((base, command_status as u16))
}

struct Reader<F> {
    read: F,
    count: u32,
}

impl<F: FnMut(u32) -> u32> Reader<F> {
    fn read(&mut self, bdf: Bdf, register: u16) -> u32 {
        let address = configuration_address(u16::from(bdf.bus), bdf.slot, bdf.function, register)
            .expect("invalid internal PCI address");
        assert!(self.count < MAX_CONFIG_READS, "PCI read limit exceeded");
        self.count += 1;
        (self.read)(address)
    }

    fn details(&mut self, bdf: Bdf, id: u32) -> Device {
        let class = self.read(bdf, 0x08);
        let header = (self.read(bdf, 0x0c) >> 16) as u8;
        let bridge = if header & 0x7f == 1 {
            Some(decode_bridge(self.read(bdf, 0x18)))
        } else {
            None
        };
        Device {
            bdf,
            vendor: id as u16,
            device: (id >> 16) as u16,
            class: (class >> 24) as u8,
            subclass: (class >> 16) as u8,
            interface: (class >> 8) as u8,
            revision: class as u8,
            header,
            bridge,
        }
    }
}

fn decode_bridge(word: u32) -> Bridge {
    Bridge {
        primary: word as u8,
        secondary: (word >> 8) as u8,
        subordinate: (word >> 16) as u8,
    }
}

fn scan(read: impl FnMut(u32) -> u32) -> Inventory {
    let mut reader = Reader { read, count: 0 };
    let mut inventory = Inventory::new();
    // Scan every routed bus without trusting/recursing through bridge-provided
    // bus numbers. Firmware must already have configured any downstream buses.
    for bus in 0..BUS_COUNT {
        for slot in 0..SLOT_COUNT {
            let first = Bdf {
                bus: bus as u8,
                slot,
                function: 0,
            };
            let id = reader.read(first, 0);
            if id as u16 == 0xffff {
                continue;
            }
            let device = reader.details(first, id);
            let multifunction = device.multifunction();
            inventory.record(device);
            if multifunction {
                for function in 1..FUNCTION_COUNT {
                    let bdf = Bdf { function, ..first };
                    let id = reader.read(bdf, 0);
                    if id as u16 != 0xffff {
                        inventory.record(reader.details(bdf, id));
                    }
                }
            }
        }
    }
    inventory.config_reads = reader.count;
    inventory
}

/// Discover firmware-visible segment-0 functions on the BSP. This module is
/// the sole CF8/CFC owner; SMP will require a shared configuration lock. There
/// are at most 65,536 function addresses, 262,144 configuration reads, and 33
/// diagnostic lines. No device configuration register is written.
pub(crate) fn probe() -> Inventory {
    let initial_if = arch::interrupts_enabled();
    let inventory = scan(read_configuration_address);
    assert_eq!(arch::interrupts_enabled(), initial_if, "PCI changed IF");
    for device in inventory.devices.iter().flatten() {
        if let Some(bridge) = device.bridge {
            serial::println!(
                "MIRROR_PCI_DEVICE bdf={:02x}:{:02x}.{} id={:04x}:{:04x} class={:02x}:{:02x}:{:02x} revision={:02x} header={:02x} primary={} secondary={} subordinate={}",
                device.bdf.bus,
                device.bdf.slot,
                device.bdf.function,
                device.vendor,
                device.device,
                device.class,
                device.subclass,
                device.interface,
                device.revision,
                device.header,
                bridge.primary,
                bridge.secondary,
                bridge.subordinate
            );
        } else {
            serial::println!(
                "MIRROR_PCI_DEVICE bdf={:02x}:{:02x}.{} id={:04x}:{:04x} class={:02x}:{:02x}:{:02x} revision={:02x} header={:02x}",
                device.bdf.bus,
                device.bdf.slot,
                device.bdf.function,
                device.vendor,
                device.device,
                device.class,
                device.subclass,
                device.interface,
                device.revision,
                device.header
            );
        }
    }
    serial::println!(
        "MIRROR_PCI_READY functions={} bridges={} virtio_block={} display={} virtio_gpu={} retained={} omitted={} reads={}",
        inventory.functions,
        inventory.bridges,
        inventory.virtio_block_devices,
        inventory.display_devices,
        inventory.virtio_gpu_devices,
        inventory.retained,
        inventory.omitted,
        inventory.config_reads
    );
    inventory
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn self_test() {
    assert_eq!(configuration_address(0, 0, 0, 0), Ok(0x8000_0000));
    assert_eq!(configuration_address(255, 31, 7, 0xfc), Ok(0x80ff_fffc));
    assert_eq!(configuration_address(256, 0, 0, 0), Err(AddressError::Bus));
    assert_eq!(configuration_address(0, 32, 0, 0), Err(AddressError::Slot));
    assert_eq!(
        configuration_address(0, 0, 8, 0),
        Err(AddressError::Function)
    );
    assert_eq!(
        configuration_address(0, 0, 0, 0x100),
        Err(AddressError::Register)
    );
    assert_eq!(
        configuration_address(0, 0, 0, 3),
        Err(AddressError::Register)
    );
    assert_eq!(
        decode_bridge(0x9988_7766),
        Bridge {
            primary: 0x66,
            secondary: 0x77,
            subordinate: 0x88
        }
    );

    let inventory = scan(|address| {
        assert_eq!(address & 0x7f00_0003, 0, "reserved configuration bits");
        let bus = ((address >> 16) & 0xff) as u8;
        let slot = ((address >> 11) & 0x1f) as u8;
        let function = ((address >> 8) & 7) as u8;
        let register = (address & 0xfc) as u8;
        assert!(matches!(register, 0 | 8 | 0x0c | 0x18));
        assert!(
            !(bus == 0 && slot == 1 && function != 0),
            "scanned single-function siblings"
        );
        match (bus, slot, function, register) {
            (0, 1, 0, 0) => 0x1001_1af4, // Transitional Virtio block.
            (0, 1, 0, 8) => 0x0102_0304,
            (0, 1, 0, 0x0c) => 0xaa00_1234, // BIST must not become header bits.
            (2, 3, 0, 0) => 0x244e_8086,
            (2, 3, 0, 8) => 0x0604_0001,
            (2, 3, 0, 0x0c) => 0x0081_0000, // Multifunction type-1 bridge.
            (2, 3, 0, 0x18) => 0x0004_0302,
            (2, 3, 3, 0) => 0x1042_1af4, // Modern Virtio block after holes.
            (2, 3, 3, 8) => 0x0100_0099, // Match any revision.
            (2, 3, 3, 0x0c) => 0,
            (2, 3, 7, 0) => 0x1050_1af4, // Modern Virtio GPU, final function.
            (2, 3, 7, 8) => 0x0300_0001,
            (2, 3, 7, 0x0c) => 0,
            (255, 31, 0, 0) => 0x1111_1234,
            (255, 31, 0, 8) => 0x0300_0002,
            (255, 31, 0, 0x0c) => 0x0080_0000,
            (255, 31, 7, 0) => 0x1001_1234, // Same device ID, wrong vendor.
            (255, 31, 7, 8 | 0x0c) => 0,
            (_, _, _, 0) => 0xffff_ffff,
            _ => panic!("unexpected configuration register read"),
        }
    });
    assert_eq!(inventory.functions, 6);
    assert_eq!(inventory.bridges, 1);
    assert_eq!(inventory.virtio_block_devices, 2);
    assert_eq!(inventory.display_devices, 2);
    assert_eq!(inventory.virtio_gpu_devices, 1);
    assert_eq!(inventory.retained, 6);
    assert_eq!(inventory.omitted, 0);
    assert_eq!(inventory.config_reads, 8192 + 14 + 12 + 1);
    let first = inventory.devices[0].unwrap();
    assert_eq!(
        (first.class, first.subclass, first.interface, first.revision),
        (1, 2, 3, 4)
    );
    assert_eq!(first.header, 0);
    assert_eq!(
        inventory.devices[1].unwrap().bridge,
        Some(Bridge {
            primary: 2,
            secondary: 3,
            subordinate: 4
        })
    );
    assert_eq!(
        inventory.devices[5].unwrap().bdf,
        Bdf {
            bus: 255,
            slot: 31,
            function: 7
        }
    );

    let absent = scan(|_| 0xffff_ffff);
    assert_eq!(absent.functions, 0);
    assert_eq!(absent.config_reads, 8192);
    let crowded = scan(|address| match address & 0xfc {
        0 => 0x1001_1af4,
        8 | 0x0c => 0,
        _ => panic!("unexpected crowded-device register"),
    });
    assert_eq!(crowded.functions, 8192);
    assert_eq!(crowded.virtio_block_devices, 8192);
    assert_eq!(crowded.retained, MAX_RETAINED);
    assert_eq!(crowded.omitted, 8192 - MAX_RETAINED as u32);
    assert_eq!(crowded.config_reads, 8192 * 3);
    legacy_self_test();
    ahci_self_test();
    serial::println!("MIRROR_PCI_TESTS_OK");
}

#[cfg(feature = "boot-smoke")]
fn ahci_self_test() {
    let inspect = |snapshot: &[u32; 64]| {
        ahci_configuration(0x4321_1234, |register| {
            assert!(register <= 0xfc && register.is_multiple_of(4));
            Ok(snapshot[usize::from(register / 4)])
        })
    };
    let mut snapshot = [0_u32; 64];
    snapshot[0] = 0x4321_1234; // No vendor/device allowlist.
    snapshot[1] = 0xa000_0007;
    snapshot[2] = 0x0106_0199;
    snapshot[9] = 0xfedc_0400; // Legal 1KiB-aligned BAR within a page.
    assert_eq!(inspect(&snapshot), Ok((0xfedc_0400, 7)));
    assert_eq!(memory_command(7, false), 0x403);
    assert_eq!(memory_command(7, true), 0x407);
    for bar in [0, 0x4001, 0x4004, 0x4008, 0x4020, 0xffff_ffff] {
        snapshot[9] = bar;
        assert_eq!(inspect(&snapshot), Err(LegacyError::MemoryBar));
    }
    snapshot[9] = 0xffff_fc00; // Mapping later checks the entire port span.
    assert_eq!(inspect(&snapshot), Ok((0xffff_fc00, 7)));
    snapshot[8] = 4; // ABAR must not be the upper half of a 64-bit BAR4.
    assert_eq!(inspect(&snapshot), Err(LegacyError::MemoryBar));
    snapshot[8] = 0;
    snapshot[2] = 0x0104_0000; // RAID mode is not AHCI.
    assert_eq!(inspect(&snapshot), Err(LegacyError::DeviceChanged));
    snapshot[2] = 0x0106_0100;
    snapshot[1] |= STATUS_CAPABILITIES;
    snapshot[0x34 / 4] = 0x40;
    snapshot[0x40 / 4] = 0x4401; // D0 PM capability followed by disabled MSI.
    snapshot[0x44 / 4] = 0x4805;
    // A PMCSR overlaps the next header here; its low two bits are 1. Reject it.
    assert_eq!(inspect(&snapshot), Err(LegacyError::PowerState));
    snapshot[0x40 / 4] = 0x4801;
    snapshot[0x44 / 4] = 0;
    snapshot[0x48 / 4] = 0x05;
    assert_eq!(inspect(&snapshot), Ok((0xffff_fc00, 7)));
    snapshot[0x44 / 4] = 3;
    assert_eq!(inspect(&snapshot), Err(LegacyError::PowerState));
    snapshot[0x44 / 4] = 0;
    snapshot[0x48 / 4] = 0x1_0005;
    assert_eq!(inspect(&snapshot), Err(LegacyError::MsiEnabled));
    snapshot[0x48 / 4] = 0x8000_0011;
    assert_eq!(inspect(&snapshot), Err(LegacyError::MsixEnabled));
    snapshot[0x48 / 4] = 0x4000;
    assert_eq!(inspect(&snapshot), Err(LegacyError::CapabilityList));
    snapshot[0x34 / 4] = 0xff;
    assert_eq!(inspect(&snapshot), Err(LegacyError::CapabilityList));
}

#[cfg(feature = "boot-smoke")]
fn legacy_self_test() {
    // Configuration snapshots exercise only parsing. No claim is created, no
    // global state is changed, and no hardware port is read or written.
    fn inspect(snapshot: &[u32; 64]) -> (Result<(u16, u16), LegacyError>, usize) {
        let mut reads = 0;
        let result = legacy_configuration(|register| {
            assert!(register <= 0xfc && register.is_multiple_of(4));
            reads += 1;
            assert!(reads <= 5 + MAX_CAPABILITIES);
            Ok(snapshot[usize::from(register / 4)])
        });
        (result, reads)
    }

    let mut snapshot = [0_u32; 64];
    snapshot[0] = 0x1001_1af4;
    snapshot[1] = 0xa000_0006; // Keep PCI Status distinct from Command.
    snapshot[4] = 0xc001;
    snapshot[0x34 / 4] = 0xff; // Ignored without Status.CapabilitiesList.
    assert_eq!(inspect(&snapshot), (Ok((0xc000, 6)), 4));
    assert_eq!(io_only_command(6), 0x0403);
    assert_eq!(master_command(6), 0x0407);
    assert_eq!(io_only_command(0xffff), 0xfffb);
    assert_eq!(master_command(0xffff), 0xffff);
    assert_eq!(legacy_io_base(0xffe5), Ok(0xffe4)); // Last valid 28-byte range.
    for bar in [0, 1, 2, 0xc000, 0xc003, 0xffe9, 0x1_0001, 0xffff_ffff] {
        assert_eq!(legacy_io_base(bar), Err(LegacyError::IoBar));
    }

    snapshot[0] = 0x1042_1af4; // A modern-only block is never a legacy target.
    assert_eq!(inspect(&snapshot).0, Err(LegacyError::DeviceChanged));
    snapshot[0] = 0x1001_1234;
    assert_eq!(inspect(&snapshot).0, Err(LegacyError::DeviceChanged));
    snapshot[0] = 0x1001_1af4;
    snapshot[3] = 0x0001_0000; // A type-1 bridge has different BAR/pointer rules.
    assert_eq!(inspect(&snapshot).0, Err(LegacyError::DeviceChanged));
    snapshot[3] = 0x0080_0000; // Multifunction bit is legal on a type-0 header.
    assert_eq!(inspect(&snapshot).0, Ok((0xc000, 6)));

    snapshot[1] |= STATUS_CAPABILITIES;
    snapshot[0x34 / 4] = 0;
    assert_eq!(inspect(&snapshot), (Ok((0xc000, 6)), 5));
    for pointer in [0x3c, 0x42, 0xfe, 0xff] {
        snapshot[0x34 / 4] = pointer;
        assert_eq!(inspect(&snapshot).0, Err(LegacyError::CapabilityList));
    }
    snapshot[0x34 / 4] = 0x40;
    snapshot[0x40 / 4] = 0x4401; // Unknown/irrelevant cap -> disabled MSI.
    snapshot[0x44 / 4] = 0x4805;
    snapshot[0x48 / 4] = 0x4000_0011; // MSI-X masked, but disabled.
    assert_eq!(inspect(&snapshot), (Ok((0xc000, 6)), 8));
    snapshot[0x48 / 4] |= 0x8000_0000;
    assert_eq!(inspect(&snapshot).0, Err(LegacyError::MsixEnabled));
    snapshot[0x48 / 4] &= !0x8000_0000;
    snapshot[0x44 / 4] |= 0x0001_0000;
    assert_eq!(inspect(&snapshot).0, Err(LegacyError::MsiEnabled));
    snapshot[0x44 / 4] = 0x4005; // Two-entry cycle.
    assert_eq!(inspect(&snapshot).0, Err(LegacyError::CapabilityList));
    snapshot[0x40 / 4] = 0x4001; // Self-cycle.
    assert_eq!(inspect(&snapshot).0, Err(LegacyError::CapabilityList));
    snapshot[0x40 / 4] = 0x4301; // Malformed next pointer.
    assert_eq!(inspect(&snapshot).0, Err(LegacyError::CapabilityList));

    for register in (0x40..=0xfc).step_by(4) {
        let next = if register == 0xfc { 0 } else { register + 4 };
        snapshot[register / 4] = (next as u32) << 8 | 1;
    }
    assert_eq!(inspect(&snapshot), (Ok((0xc000, 6)), 5 + MAX_CAPABILITIES));
    snapshot[0xfc / 4] = 0x4001; // Full-length cycle still stops at 48 reads.
    assert_eq!(
        inspect(&snapshot),
        (Err(LegacyError::CapabilityList), 5 + MAX_CAPABILITIES)
    );
    assert_eq!(
        legacy_configuration(|_| Err(LegacyError::ConfigurationAddress)),
        Err(LegacyError::ConfigurationAddress)
    );
}
