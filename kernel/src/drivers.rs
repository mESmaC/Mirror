//! Bounded, read-only native driver matching and hardware diagnostics.
//!
//! A candidate means that an implementation recognizes the device's identity
//! or standard interface. It does not establish a successful claim, usable
//! firmware resources, initialization, or an active driver. Transport drivers
//! remain responsible for validation, exclusive ownership, and recovery.

use crate::{pci, serial};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Candidate {
    LegacyVirtioBlock,
    AhciSata,
    XhciHost,
}

impl Candidate {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::LegacyVirtioBlock => "virtio-blk-legacy",
            Self::AhciSata => "ahci-sata",
            Self::XhciHost => "xhci-host",
        }
    }

    pub(crate) fn match_kind(self) -> &'static str {
        match self {
            Self::LegacyVirtioBlock => "exact",
            Self::AhciSata | Self::XhciHost => "class",
        }
    }
}

pub(crate) fn candidate(device: &pci::Device) -> Option<Candidate> {
    if device.header & 0x7f != 0 {
        return None;
    }
    if device.vendor == 0x1af4 && device.device == 0x1001 {
        Some(Candidate::LegacyVirtioBlock)
    } else if device.is_ahci() {
        Some(Candidate::AhciSata)
    } else if device.is_xhci() {
        Some(Candidate::XhciHost)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub retained: usize,
    pub matched: usize,
    pub unsupported: usize,
    pub omitted: u32,
}

pub(crate) fn devices(inventory: &pci::Inventory) -> impl Iterator<Item = &pci::Device> {
    inventory.devices.iter().take(inventory.retained).flatten()
}

pub(crate) fn summary(inventory: &pci::Inventory) -> Summary {
    let retained = devices(inventory).count();
    let matched = devices(inventory)
        .filter(|device| candidate(device).is_some())
        .count();
    Summary {
        retained,
        matched,
        unsupported: retained - matched,
        omitted: inventory.omitted,
    }
}

pub(crate) fn report(inventory: &pci::Inventory) {
    for device in devices(inventory) {
        let matched = candidate(device);
        serial::println!(
            "MIRROR_DRIVER_DEVICE bdf={:02x}:{:02x}.{} id={:04x}:{:04x} class={:02x}:{:02x}:{:02x} driver={} match={}",
            device.bdf.bus,
            device.bdf.slot,
            device.bdf.function,
            device.vendor,
            device.device,
            device.class,
            device.subclass,
            device.interface,
            matched.map_or("none", Candidate::name),
            matched.map_or("none", Candidate::match_kind)
        );
    }
    let summary = summary(inventory);
    serial::println!(
        "MIRROR_DRIVER_CATALOG retained={} matched={} unsupported={} omitted={}",
        summary.retained,
        summary.matched,
        summary.unsupported,
        summary.omitted
    );
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn self_test() {
    let unknown = pci::Device {
        bdf: pci::Bdf {
            bus: 0,
            slot: 1,
            function: 0,
        },
        vendor: 0x1234,
        device: 0x5678,
        class: 0xff,
        subclass: 0,
        interface: 0,
        revision: 0,
        header: 0,
        bridge: None,
    };
    assert_eq!(candidate(&unknown), None);
    let legacy = pci::Device {
        vendor: 0x1af4,
        device: 0x1001,
        ..unknown
    };
    assert_eq!(candidate(&legacy), Some(Candidate::LegacyVirtioBlock));
    assert_eq!(
        candidate(&pci::Device {
            header: 0x80,
            ..legacy
        }),
        Some(Candidate::LegacyVirtioBlock)
    );
    for device in [
        pci::Device {
            header: 1,
            ..legacy
        },
        pci::Device {
            header: 2,
            ..legacy
        },
        pci::Device {
            vendor: 0x1234,
            ..legacy
        },
        pci::Device {
            device: 0x1042,
            ..legacy
        },
        pci::Device {
            device: 0x1000,
            ..legacy
        },
    ] {
        assert_eq!(candidate(&device), None);
    }
    let ahci = pci::Device {
        class: 1,
        subclass: 6,
        interface: 1,
        ..unknown
    };
    assert_eq!(candidate(&ahci), Some(Candidate::AhciSata));
    assert_eq!(
        candidate(&pci::Device {
            vendor: 0x8086,
            device: 0x2922,
            header: 0x80,
            ..ahci
        }),
        Some(Candidate::AhciSata)
    );
    for device in [
        pci::Device { header: 1, ..ahci },
        pci::Device { class: 2, ..ahci },
        pci::Device {
            subclass: 1,
            ..ahci
        },
        pci::Device {
            interface: 0,
            ..ahci
        },
    ] {
        assert_eq!(candidate(&device), None);
    }
    let xhci = pci::Device {
        class: 0x0c,
        subclass: 3,
        interface: 0x30,
        ..unknown
    };
    assert_eq!(candidate(&xhci), Some(Candidate::XhciHost));
    assert_eq!(Candidate::XhciHost.name(), "xhci-host");
    assert_eq!(Candidate::XhciHost.match_kind(), "class");
    assert_eq!(
        candidate(&pci::Device {
            vendor: 0x8086,
            device: 0x1234,
            header: 0x80,
            ..xhci
        }),
        Some(Candidate::XhciHost)
    );
    for device in [
        pci::Device { header: 1, ..xhci },
        pci::Device { class: 2, ..xhci },
        pci::Device {
            subclass: 0,
            ..xhci
        },
        pci::Device {
            interface: 0,
            ..xhci
        },
        pci::Device {
            interface: 0x10,
            ..xhci
        },
        pci::Device {
            interface: 0x20,
            ..xhci
        },
    ] {
        assert_eq!(candidate(&device), None);
    }
    let mut inventory = test_inventory();
    assert_eq!(
        summary(&inventory),
        Summary {
            retained: 5,
            matched: 2,
            unsupported: 3,
            omitted: 3,
        }
    );
    inventory.devices[5] = Some(legacy);
    assert_eq!(summary(&inventory).matched, 2, "ignore unretained entries");
    inventory.devices[4] = Some(xhci);
    assert_eq!(summary(&inventory).matched, 3);
    assert_eq!(summary(&inventory).unsupported, 2);
    inventory.devices.fill(Some(unknown));
    inventory.retained = usize::MAX;
    assert_eq!(summary(&inventory).retained, inventory.devices.len());
    assert_eq!(summary(&inventory).unsupported, inventory.devices.len());
    inventory.devices.fill(None);
    assert_eq!(summary(&inventory).retained, 0);
    serial::println!("MIRROR_DRIVER_TESTS_OK");
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn test_inventory() -> pci::Inventory {
    let mut inventory = pci::Inventory {
        functions: 8,
        bridges: 0,
        virtio_block_devices: 2,
        display_devices: 0,
        virtio_gpu_devices: 0,
        config_reads: 0,
        devices: [None; pci::MAX_RETAINED],
        retained: 5,
        omitted: 3,
    };
    for (slot, (vendor, device, class, subclass, interface)) in [
        (0x8086, 0x1237, 6, 0, 0),
        (0x1af4, 0x1001, 1, 0, 0),
        (0x4321, 0x8888, 1, 6, 1),
        (0x1af4, 0x1042, 1, 0, 0),
        (0xabcd, 0x9876, 2, 0, 0),
    ]
    .into_iter()
    .enumerate()
    {
        inventory.devices[slot] = Some(pci::Device {
            bdf: pci::Bdf {
                bus: 0,
                slot: slot as u8,
                function: 0,
            },
            vendor,
            device,
            class,
            subclass,
            interface,
            revision: 0,
            header: 0,
            bridge: None,
        });
    }
    inventory
}
