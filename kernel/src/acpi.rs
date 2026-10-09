//! Bounded ACPI discovery, without AML execution or firmware memory reclamation.
//!
//! Limine base revision 3 returns a physical RSDP and does not map ACPI RAM.
//! Copy tables through one temporary RO/NX/WB page, validate owned bytes, and
//! retain only parsed topology. No firmware pointer outlives its mapping.

use alloc::vec::Vec;
use core::fmt;
use limine::memory_map::{Entry, EntryType};
use mirror_acpi::{Header, Root, Rsdp, SDT_HEADER_SIZE, Topology};

use crate::{boot::BootInfo, memory::FrameAllocator, paging::PageTables, serial};

// Keep firmware scratch mappings outside the full reserved heap arena.
const FIRMWARE_WINDOW: u64 = 0xffff_d300_0020_0000;
const _: () = assert!(
    FIRMWARE_WINDOW >= crate::heap::HEAP_START + crate::heap::HEAP_MAX_SIZE as u64 + PAGE_SIZE
);
const PAGE_SIZE: u64 = crate::memory::PAGE_SIZE;

#[derive(Debug)]
enum Error {
    MissingRsdp,
    InvalidRange,
    UnreservedMemory,
    OutOfMemory,
    Paging(crate::paging::PagingError),
    Table(mirror_acpi::Error),
    MissingMadt,
    DuplicateMadt,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paging(error) => write!(f, "mapping:{error:?}"),
            Self::Table(error) => write!(f, "table:{error:?}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Called once before private address spaces are created, on the sole CPU.
pub fn discover(
    boot: &BootInfo,
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
) -> Option<Topology> {
    assert!(!crate::arch::interrupts_enabled());
    let mut reader = Reader {
        entries: boot.memory_map.entries(),
        tables,
        frames,
    };
    match reader.discover(boot.rsdp_physical) {
        Ok(topology) => Some(topology),
        Err(error) => {
            serial::println!("MIRROR_ACPI_SKIPPED reason={}", error);
            None
        }
    }
}

struct Reader<'a, 'b> {
    entries: &'a [&'a Entry],
    tables: &'a mut PageTables,
    frames: &'a mut FrameAllocator<'b>,
}

impl Reader<'_, '_> {
    fn discover(&mut self, physical: Option<u64>) -> Result<Topology, Error> {
        let physical = physical.ok_or(Error::MissingRsdp)?;
        let mut prefix = [0; 24];
        self.read(physical, &mut prefix[..20])?;
        if prefix[15] >= 2 {
            self.read(physical, &mut prefix)?;
        }
        let length = Rsdp::required_len(&prefix).map_err(Error::Table)?;
        let bytes = self.copy(physical, length)?;
        let rsdp = Rsdp::parse(&bytes).map_err(Error::Table)?;
        let root_bytes = self.table(rsdp.root_address)?;
        let root = Root::parse(&root_bytes, rsdp.root_kind).map_err(Error::Table)?;
        let mut topology = None;
        for address in root.addresses() {
            let mut header = [0; SDT_HEADER_SIZE];
            self.read(address, &mut header)?;
            let header = Header::parse(&header).map_err(Error::Table)?;
            if header.signature == *b"APIC" {
                if topology.is_some() {
                    return Err(Error::DuplicateMadt);
                }
                let bytes = self.copy(address, header.length)?;
                topology = Some(Topology::parse(&bytes).map_err(Error::Table)?);
            }
        }
        let topology = topology.ok_or(Error::MissingMadt)?;
        serial::println!(
            "MIRROR_ACPI_READY revision={} root={:?} cpus={} enabled={} ioapics={} overrides={} lapic={:#x}",
            rsdp.revision,
            rsdp.root_kind,
            topology.cpus().len(),
            topology.cpus().iter().filter(|cpu| cpu.enabled).count(),
            topology.io_apics().len(),
            topology.overrides().len(),
            topology.lapic_address
        );
        for cpu in topology.cpus() {
            serial::println!(
                "MIRROR_ACPI_CPU uid={} apic={} enabled={} online_capable={} x2apic={}",
                cpu.uid,
                cpu.apic_id,
                u8::from(cpu.enabled),
                u8::from(cpu.online_capable),
                u8::from(cpu.x2apic)
            );
        }
        Ok(topology)
    }

    fn table(&mut self, physical: u64) -> Result<Vec<u8>, Error> {
        let mut header = [0; SDT_HEADER_SIZE];
        self.read(physical, &mut header)?;
        let header = Header::parse(&header).map_err(Error::Table)?;
        self.copy(physical, header.length)
    }

    fn copy(&mut self, physical: u64, length: usize) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| Error::OutOfMemory)?;
        bytes.resize(length, 0);
        self.read(physical, &mut bytes)?;
        Ok(bytes)
    }

    fn read(&mut self, physical: u64, bytes: &mut [u8]) -> Result<(), Error> {
        let end = physical
            .checked_add(bytes.len() as u64)
            .ok_or(Error::InvalidRange)?;
        if physical == 0 || end <= physical {
            return Err(Error::InvalidRange);
        }
        if !firmware_range(self.entries, physical, end) {
            return Err(Error::UnreservedMemory);
        }
        let mut copied = 0;
        while copied < bytes.len() {
            let current = physical + copied as u64;
            let page = current & !(PAGE_SIZE - 1);
            let page_end = page.checked_add(PAGE_SIZE).ok_or(Error::InvalidRange)?;
            // Even speculative/cache-line reads must not reach a device page
            // through this WB alias. Require the whole mapped page to be RAM.
            if !firmware_range(self.entries, page, page_end) {
                return Err(Error::UnreservedMemory);
            }
            let offset = current - page;
            let count = (bytes.len() - copied).min((PAGE_SIZE - offset) as usize);
            // SAFETY: The checked byte interval lies only in reserved firmware
            // RAM. WB agrees with any bootloader RAM alias, and no reference to
            // this scratch page survives the copy. IF=0, unique tables/allocator.
            unsafe {
                self.tables
                    .map_firmware_page(FIRMWARE_WINDOW, page, self.frames)
            }
            .map_err(Error::Paging)?;
            // SAFETY: The source page is mapped read-only and these exact bytes
            // are firmware RAM. Destination is disjoint owned kernel memory.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (FIRMWARE_WINDOW + offset) as *const u8,
                    bytes.as_mut_ptr().add(copied),
                    count,
                );
            }
            // SAFETY: There are no live source references or subsequent reads.
            let removed =
                unsafe { self.tables.unmap_page(FIRMWARE_WINDOW) }.map_err(Error::Paging)?;
            assert_eq!(removed, page);
            copied += count;
        }
        Ok(())
    }
}

fn firmware_range(entries: &[&Entry], start: u64, end: u64) -> bool {
    let mut cursor = start;
    while cursor < end {
        let Some(entry) = entries.iter().find(|entry| {
            entry.base <= cursor
                && entry
                    .base
                    .checked_add(entry.length)
                    .is_some_and(|limit| cursor < limit)
        }) else {
            return false;
        };
        if !matches!(
            entry.entry_type,
            EntryType::ACPI_RECLAIMABLE | EntryType::ACPI_NVS | EntryType::BOOTLOADER_RECLAIMABLE
        ) {
            return false;
        }
        cursor = end.min(entry.base + entry.length);
    }
    start < end
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    let region = |base, length, entry_type| Entry {
        base,
        length,
        entry_type,
    };
    let first = region(0x1000, 4096, EntryType::ACPI_RECLAIMABLE);
    let second = region(0x2000, 4096, EntryType::ACPI_NVS);
    let reserved = region(0x3000, 4096, EntryType::RESERVED);
    let usable = region(0x4000, 4096, EntryType::USABLE);
    let wrapped = region(u64::MAX - 10, 20, EntryType::ACPI_RECLAIMABLE);
    let entries = [&first, &second, &reserved, &usable, &wrapped];
    assert!(firmware_range(&entries, 0x1fff, 0x2001));
    assert!(firmware_range(&entries, 0x1000, 0x3000));
    let partial = region(0x6001, 4095, EntryType::ACPI_RECLAIMABLE);
    assert!(firmware_range(&[&partial], 0x6001, 0x6010));
    assert!(!firmware_range(&[&partial], 0x6000, 0x7000));
    for (start, end) in [
        (0, 20),
        (0x1000, 0x1000),
        (0x2000, 0x3001),
        (0x4000, 0x4004),
        (0x5000, 0x5004),
        (u64::MAX - 9, u64::MAX),
    ] {
        assert!(!firmware_range(&entries, start, end));
    }
    serial::println!("MIRROR_ACPI_MAPPING_TESTS_OK");
}
