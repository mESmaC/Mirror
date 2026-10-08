//! Bounded, read-only PCI discovery through x86 configuration mechanism 1.
//!
//! One BSP owns CF8/CFC; each address/data transaction preserves the caller's
//! interrupt state. Only firmware-configured segment 0 is visible. No ECAM,
//! extended configuration, ARI, hotplug synchronization, BAR sizing, resource
//! assignment, capability walking, DMA, bus-master, or IRQ enablement yet.
//!
//! PCI Local Bus Specification 2.1, sections 3.7.4.1 and 6.2 (PCI-SIG document):
//! https://www.bitsavers.org/pdf/pciSIG/PCI_Local_Bus_Specification_2.1_199506.pdf
//! Virtio 1.2, sections 4.1.2, 5.2.1, and 5.7.1 (device identifiers):
//! https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html

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
    let inventory = scan(|address| {
        let was_enabled = arch::save_and_disable_interrupts();
        // SAFETY: Only this BSP/module owns the two DWORD configuration ports.
        // Reader validated the address before disabling IF. The fixed sequence
        // has no fallible operations between saving and restoring IF. CF8 is
        // only the host's address latch; the selected device is read at CFC.
        unsafe {
            arch::out32(CONFIG_ADDRESS, address);
            let value = arch::in32(CONFIG_DATA);
            arch::restore_interrupts(was_enabled);
            value
        }
    });
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
    serial::println!("MIRROR_PCI_TESTS_OK");
}
