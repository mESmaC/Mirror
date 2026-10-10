#![no_std]
#![forbid(unsafe_code)]
//! Bounded, allocation-free ACPI discovery. Physical memory access belongs to
//! the caller: this crate only reads immutable byte slices and never follows
//! firmware addresses. Tables must remain immutable while being inspected.
//!
//! Layouts follow ACPI 6.6 sections 5.2.5, 5.2.6, 5.2.7, 5.2.8 and 5.2.12:
//! <https://uefi.org/specs/ACPI/6.6/05_ACPI_Software_Programming_Model.html>.
//! HPET layouts follow the IA-PC HPET Specification 1.0a, section 3.2.4:
//! <https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/software-developers-hpet-spec-1-0a.pdf>.
//! These are x86 topology and optional timer descriptors, not an AML interpreter.
//! Capacity and table-size limits are kernel policy and bound each table parse.

pub const RSDP_V1_SIZE: usize = 20;
pub const RSDP_V2_SIZE: usize = 36;
pub const SDT_HEADER_SIZE: usize = 36;
pub const HPET_TABLE_SIZE: usize = 56;
pub const MAX_RSDP_SIZE: usize = 4096;
pub const MAX_TABLE_SIZE: usize = 256 * 1024;
pub const MAX_ROOT_ENTRIES: usize = 256;
pub const MAX_CPUS: usize = 64;
pub const MAX_IO_APICS: usize = 8;
pub const MAX_OVERRIDES: usize = 16;
pub const MAX_LOCAL_NMIS: usize = 64;
pub const MAX_NMI_SOURCES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Signature,
    Revision,
    Length,
    Checksum,
    Reserved,
    Address,
    Capacity,
    Duplicate,
    Conflict,
    RecordLength,
    InvalidIsaIrq,
    UnsupportedInterruptModel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    Rsdt,
    Xsdt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    pub revision: u8,
    pub root_address: u64,
    pub root_kind: RootKind,
}

impl Rsdp {
    /// Determines a bounded mapping size from a prefix. Revision zero needs
    /// twenty bytes; revisions >=2 need twenty-four. Revision one is undefined.
    /// This does not validate either checksum; use `parse` on the full mapping.
    pub fn required_len(prefix: &[u8]) -> Result<usize, Error> {
        require(prefix, RSDP_V1_SIZE)?;
        if &prefix[..8] != b"RSD PTR " {
            return Err(Error::Signature);
        }
        match prefix[15] {
            0 => Ok(RSDP_V1_SIZE),
            1 => Err(Error::Revision),
            _ => {
                require(prefix, 24)?;
                bounded_length(u32_at(prefix, 20) as usize, RSDP_V2_SIZE, MAX_RSDP_SIZE)
            }
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let length = Self::required_len(bytes)?;
        require(bytes, length)?;
        checksum(&bytes[..RSDP_V1_SIZE])?;
        let revision = bytes[15];
        let (root_address, root_kind) = if revision >= 2 {
            checksum(&bytes[..length])?;
            reserved(&bytes[33..36])?;
            let xsdt = u64_at(bytes, 24);
            if xsdt != 0 {
                (xsdt, RootKind::Xsdt)
            } else {
                (u32_at(bytes, 16) as u64, RootKind::Rsdt)
            }
        } else {
            (u32_at(bytes, 16) as u64, RootKind::Rsdt)
        };
        if root_address == 0 {
            return Err(Error::Address);
        }
        Ok(Self {
            revision,
            root_address,
            root_kind,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub signature: [u8; 4],
    pub length: usize,
    pub revision: u8,
}

impl Header {
    /// Reads the fixed header and bounds the declared length before a caller
    /// maps it. No checksum can be checked until the complete table is mapped.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        require(bytes, SDT_HEADER_SIZE)?;
        Ok(Self {
            signature: [bytes[0], bytes[1], bytes[2], bytes[3]],
            length: bounded_length(u32_at(bytes, 4) as usize, SDT_HEADER_SIZE, MAX_TABLE_SIZE)?,
            revision: bytes[8],
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sdt<'a> {
    header: Header,
    bytes: &'a [u8],
}

impl<'a> Sdt<'a> {
    /// Trailing mapped bytes are allowed but do not participate in validation.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let header = Header::parse(bytes)?;
        require(bytes, header.length)?;
        let bytes = &bytes[..header.length];
        checksum(bytes)?;
        Ok(Self { header, bytes })
    }

    pub fn header(&self) -> Header {
        self.header
    }

    pub fn signature(&self) -> [u8; 4] {
        self.header.signature
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

/// Firmware's description of one HPET block, without any live MMIO access.
/// The driver must validate the address against platform memory ownership and
/// compare the reported block ID with hardware before using the counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hpet {
    pub event_timer_block_id: u32,
    pub address: u64,
    pub number: u8,
    pub minimum_tick: u16,
    /// 0: no page guarantee; 1: 4 KiB; 2: 64 KiB. Never grants user access.
    pub page_protection: u8,
    /// The upper attribute nibble is OEM-defined, not reserved-zero.
    pub oem_attributes: u8,
}

impl Hpet {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let table = Sdt::parse(bytes)?;
        if table.signature() != *b"HPET" {
            return Err(Error::Signature);
        }
        if table.header.revision != 1 {
            return Err(Error::Revision);
        }
        let bytes = table.bytes;
        require(bytes, HPET_TABLE_SIZE)?;
        let event_timer_block_id = u32_at(bytes, 36);
        if event_timer_block_id & (1 << 14) != 0 {
            return Err(Error::Reserved);
        }
        if event_timer_block_id & 0xff == 0 {
            return Err(Error::Revision);
        }
        // This GAS addresses a register block, so width and offset are zero
        // (ACPI 6.6 section 5.2.3.2). HPET 1.0a also reserves GAS byte3; retain
        // its legacy zero access-size encoding. System I/O is unsupported.
        if bytes[40] != 0 {
            return Err(Error::Address);
        }
        reserved(&bytes[41..44])?;
        let address = u64_at(bytes, 44);
        if address == 0 || !address.is_multiple_of(8) || address.checked_add(1024).is_none() {
            return Err(Error::Address);
        }
        let page_protection = bytes[55] & 0xf;
        if page_protection > 2 {
            return Err(Error::Reserved);
        }
        // A zero minimum periodic tick is used by QEMU. Counter width and
        // comparator count remain hardware/driver policy, not table validity.
        Ok(Self {
            event_timer_block_id,
            address,
            number: bytes[52],
            minimum_tick: u16::from_le_bytes([bytes[53], bytes[54]]),
            page_protection,
            oem_attributes: bytes[55] >> 4,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Root<'a> {
    entries: &'a [u8],
    width: usize,
}

impl<'a> Root<'a> {
    pub fn parse(bytes: &'a [u8], kind: RootKind) -> Result<Self, Error> {
        let table = Sdt::parse(bytes)?;
        let (signature, width) = match kind {
            RootKind::Rsdt => (*b"RSDT", 4),
            RootKind::Xsdt => (*b"XSDT", 8),
        };
        if table.signature() != signature {
            return Err(Error::Signature);
        }
        let entries = &table.bytes[SDT_HEADER_SIZE..];
        if !entries.len().is_multiple_of(width) {
            return Err(Error::Length);
        }
        if entries.len() / width > MAX_ROOT_ENTRIES {
            return Err(Error::Capacity);
        }
        let root = Self { entries, width };
        for (index, address) in root.addresses().enumerate() {
            if address == 0 {
                return Err(Error::Address);
            }
            if root.addresses().take(index).any(|prior| prior == address) {
                return Err(Error::Duplicate);
            }
        }
        Ok(root)
    }

    pub fn addresses(&self) -> impl ExactSizeIterator<Item = u64> + '_ {
        self.entries.chunks_exact(self.width).map(|entry| {
            if self.width == 4 {
                u32_at(entry, 0) as u64
            } else {
                u64_at(entry, 0)
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cpu {
    pub uid: u32,
    pub apic_id: u32,
    pub enabled: bool,
    pub online_capable: bool,
    pub x2apic: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoApic {
    pub id: u8,
    pub address: u64,
    pub gsi_base: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Polarity {
    #[default]
    ActiveHigh,
    ActiveLow,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Trigger {
    #[default]
    Edge,
    Level,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqRoute {
    pub gsi: u32,
    pub polarity: Polarity,
    pub trigger: Trigger,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterruptOverride {
    pub source: u8,
    pub gsi: u32,
    pub polarity: Polarity,
    pub trigger: Trigger,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalNmi {
    /// None applies to all processors, including processors of the other
    /// record encoding. Otherwise this is the ACPI UID, not the APIC ID.
    pub processor_uid: Option<u32>,
    pub x2apic: bool,
    pub lint: u8,
    /// None preserves firmware's conform-to-bus value for the driver to resolve.
    pub polarity: Option<Polarity>,
    pub trigger: Option<Trigger>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NmiSource {
    pub gsi: u32,
    pub polarity: Option<Polarity>,
    pub trigger: Option<Trigger>,
}

#[derive(Clone, Debug)]
pub struct Topology {
    pub lapic_address: u64,
    pub legacy_pic: bool,
    cpus: [Cpu; MAX_CPUS],
    cpu_count: usize,
    io_apics: [IoApic; MAX_IO_APICS],
    io_apic_count: usize,
    overrides: [InterruptOverride; MAX_OVERRIDES],
    override_count: usize,
    local_nmis: [LocalNmi; MAX_LOCAL_NMIS],
    local_nmi_count: usize,
    nmi_sources: [NmiSource; MAX_NMI_SOURCES],
    nmi_source_count: usize,
}

impl Topology {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let table = Sdt::parse(bytes)?;
        if table.signature() != *b"APIC" {
            return Err(Error::Signature);
        }
        let bytes = table.bytes;
        require(bytes, 44)?;
        let flags = u32_at(bytes, 40);
        if flags & !1 != 0 {
            return Err(Error::Reserved);
        }
        let mut result = Self {
            lapic_address: u32_at(bytes, 36) as u64,
            legacy_pic: flags & 1 != 0,
            cpus: [Cpu::default(); MAX_CPUS],
            cpu_count: 0,
            io_apics: [IoApic::default(); MAX_IO_APICS],
            io_apic_count: 0,
            overrides: [InterruptOverride::default(); MAX_OVERRIDES],
            override_count: 0,
            local_nmis: [LocalNmi::default(); MAX_LOCAL_NMIS],
            local_nmi_count: 0,
            nmi_sources: [NmiSource::default(); MAX_NMI_SOURCES],
            nmi_source_count: 0,
        };
        let mut remaining = &bytes[44..];
        let mut address_override = false;
        while !remaining.is_empty() {
            require(remaining, 2)?;
            let length = remaining[1] as usize;
            if length < 2 {
                return Err(Error::RecordLength);
            }
            require(remaining, length)?;
            let record = &remaining[..length];
            match record[0] {
                0 => {
                    record_length(record, 8)?;
                    result.add_cpu(record[2] as u32, record[3] as u32, u32_at(record, 4), false)?;
                }
                1 => {
                    record_length(record, 12)?;
                    reserved(&record[3..4])?;
                    let controller = IoApic {
                        id: record[2],
                        address: u32_at(record, 4) as u64,
                        gsi_base: u32_at(record, 8),
                    };
                    // IOREGSEL/IOWIN occupy twenty bytes; page-aligned MMIO
                    // bases are required by the current kernel mapping policy.
                    page_address(controller.address)?;
                    if result.io_apics().iter().any(|prior| {
                        prior.id == controller.id
                            || prior.address == controller.address
                            || prior.gsi_base == controller.gsi_base
                    }) {
                        return Err(Error::Duplicate);
                    }
                    push(&mut result.io_apics, &mut result.io_apic_count, controller)?;
                }
                2 => {
                    record_length(record, 10)?;
                    if record[2] != 0 {
                        return Err(Error::Reserved);
                    }
                    let source = record[3];
                    if source >= 16 {
                        return Err(Error::InvalidIsaIrq);
                    }
                    let (polarity, trigger) = interrupt_flags(u16_at(record, 8))?;
                    let entry = InterruptOverride {
                        source,
                        gsi: u32_at(record, 4),
                        polarity: polarity.unwrap_or(Polarity::ActiveHigh),
                        trigger: trigger.unwrap_or(Trigger::Edge),
                    };
                    if result
                        .overrides()
                        .iter()
                        .any(|prior| prior.source == entry.source || prior.gsi == entry.gsi)
                    {
                        return Err(Error::Duplicate);
                    }
                    push(&mut result.overrides, &mut result.override_count, entry)?;
                }
                3 => {
                    record_length(record, 8)?;
                    let (polarity, trigger) = interrupt_flags(u16_at(record, 2))?;
                    let entry = NmiSource {
                        gsi: u32_at(record, 4),
                        polarity,
                        trigger,
                    };
                    if result
                        .nmi_sources()
                        .iter()
                        .any(|prior| prior.gsi == entry.gsi)
                    {
                        return Err(Error::Duplicate);
                    }
                    push(&mut result.nmi_sources, &mut result.nmi_source_count, entry)?;
                }
                4 => {
                    record_length(record, 6)?;
                    let (polarity, trigger) = interrupt_flags(u16_at(record, 3))?;
                    result.add_local_nmi(LocalNmi {
                        processor_uid: (record[2] != 0xff).then_some(record[2] as u32),
                        x2apic: false,
                        lint: record[5],
                        polarity,
                        trigger,
                    })?;
                }
                5 => {
                    record_length(record, 12)?;
                    reserved(&record[2..4])?;
                    if address_override {
                        return Err(Error::Duplicate);
                    }
                    address_override = true;
                    result.lapic_address = u64_at(record, 4);
                }
                9 => {
                    record_length(record, 16)?;
                    reserved(&record[2..4])?;
                    result.add_cpu(
                        u32_at(record, 12),
                        u32_at(record, 4),
                        u32_at(record, 8),
                        true,
                    )?;
                }
                10 => {
                    record_length(record, 12)?;
                    reserved(&record[9..12])?;
                    let (polarity, trigger) = interrupt_flags(u16_at(record, 2))?;
                    let uid = u32_at(record, 4);
                    result.add_local_nmi(LocalNmi {
                        processor_uid: (uid != u32::MAX).then_some(uid),
                        x2apic: true,
                        lint: record[8],
                        polarity,
                        trigger,
                    })?;
                }
                // A SAPIC supersedes an IOAPIC record. Ignoring it would let
                // this x86 driver program an explicitly overridden controller.
                6..=8 => return Err(Error::UnsupportedInterruptModel),
                _ => {}
            }
            remaining = &remaining[length..];
        }
        page_address(result.lapic_address)?;
        if result
            .io_apics()
            .iter()
            .any(|io| io.address == result.lapic_address)
        {
            return Err(Error::Conflict);
        }
        if result
            .nmi_sources()
            .iter()
            .any(|nmi| result.overrides().iter().any(|entry| entry.gsi == nmi.gsi))
        {
            return Err(Error::Conflict);
        }
        Ok(result)
    }

    pub fn cpus(&self) -> &[Cpu] {
        &self.cpus[..self.cpu_count]
    }

    pub fn io_apics(&self) -> &[IoApic] {
        &self.io_apics[..self.io_apic_count]
    }

    pub fn overrides(&self) -> &[InterruptOverride] {
        &self.overrides[..self.override_count]
    }

    pub fn local_nmis(&self) -> &[LocalNmi] {
        &self.local_nmis[..self.local_nmi_count]
    }

    pub fn nmi_sources(&self) -> &[NmiSource] {
        &self.nmi_sources[..self.nmi_source_count]
    }

    /// Resolve ISA polarity/trigger defaults and reject an identity input
    /// repurposed by another IRQ override or an NMI source. IOAPIC GSI coverage
    /// requires hardware register discovery and is checked by the caller.
    pub fn isa_irq(&self, irq: u8) -> Result<IrqRoute, Error> {
        if irq >= 16 {
            return Err(Error::InvalidIsaIrq);
        }
        let entry = self.overrides().iter().find(|entry| entry.source == irq);
        let route = match entry {
            Some(entry) => IrqRoute {
                gsi: entry.gsi,
                polarity: entry.polarity,
                trigger: entry.trigger,
            },
            None => IrqRoute {
                gsi: irq as u32,
                polarity: Polarity::ActiveHigh,
                trigger: Trigger::Edge,
            },
        };
        if self
            .overrides()
            .iter()
            .any(|entry| entry.source != irq && entry.gsi == route.gsi)
            || self.nmi_sources().iter().any(|nmi| nmi.gsi == route.gsi)
        {
            return Err(Error::Conflict);
        }
        Ok(route)
    }

    fn add_cpu(&mut self, uid: u32, apic_id: u32, flags: u32, x2apic: bool) -> Result<(), Error> {
        // ACPI requires an unusable record (neither availability bit set) to
        // be ignored, including identity fields that firmware may leave stale.
        if flags & 3 == 0 {
            return Ok(());
        }
        if flags & !3 != 0 || flags & 3 == 3 {
            return Err(Error::Reserved);
        }
        if apic_id == u32::MAX || (!x2apic && apic_id == 0xff) {
            return Err(Error::Address);
        }
        if self
            .cpus()
            .iter()
            .any(|cpu| cpu.uid == uid || cpu.apic_id == apic_id)
        {
            return Err(Error::Duplicate);
        }
        push(
            &mut self.cpus,
            &mut self.cpu_count,
            Cpu {
                uid,
                apic_id,
                enabled: flags & 1 != 0,
                online_capable: flags & 2 != 0,
                x2apic,
            },
        )
    }

    fn add_local_nmi(&mut self, entry: LocalNmi) -> Result<(), Error> {
        if entry.lint > 1 {
            return Err(Error::Reserved);
        }
        if self.local_nmis().iter().any(|prior| {
            prior.lint == entry.lint
                && (prior.processor_uid.is_none()
                    || entry.processor_uid.is_none()
                    || prior.processor_uid == entry.processor_uid)
        }) {
            return Err(Error::Duplicate);
        }
        push(&mut self.local_nmis, &mut self.local_nmi_count, entry)
    }
}

fn require(bytes: &[u8], length: usize) -> Result<(), Error> {
    if bytes.len() < length {
        Err(Error::Truncated)
    } else {
        Ok(())
    }
}

fn bounded_length(length: usize, minimum: usize, maximum: usize) -> Result<usize, Error> {
    if (minimum..=maximum).contains(&length) {
        Ok(length)
    } else {
        Err(Error::Length)
    }
}

fn checksum(bytes: &[u8]) -> Result<(), Error> {
    if bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0 {
        Ok(())
    } else {
        Err(Error::Checksum)
    }
}

fn reserved(bytes: &[u8]) -> Result<(), Error> {
    if bytes.iter().any(|value| *value != 0) {
        Err(Error::Reserved)
    } else {
        Ok(())
    }
}

fn record_length(bytes: &[u8], expected: usize) -> Result<(), Error> {
    if bytes.len() == expected {
        Ok(())
    } else {
        Err(Error::RecordLength)
    }
}

fn page_address(address: u64) -> Result<(), Error> {
    if address == 0 || !address.is_multiple_of(4096) {
        Err(Error::Address)
    } else {
        Ok(())
    }
}

fn interrupt_flags(flags: u16) -> Result<(Option<Polarity>, Option<Trigger>), Error> {
    if flags & !0xf != 0 {
        return Err(Error::Reserved);
    }
    let polarity = match flags & 3 {
        0 => None,
        1 => Some(Polarity::ActiveHigh),
        3 => Some(Polarity::ActiveLow),
        _ => return Err(Error::Reserved),
    };
    let trigger = match (flags >> 2) & 3 {
        0 => None,
        1 => Some(Trigger::Edge),
        3 => Some(Trigger::Level),
        _ => return Err(Error::Reserved),
    };
    Ok((polarity, trigger))
}

fn push<T: Copy, const N: usize>(
    items: &mut [T; N],
    count: &mut usize,
    value: T,
) -> Result<(), Error> {
    let slot = items.get_mut(*count).ok_or(Error::Capacity)?;
    *slot = value;
    *count += 1;
    Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[cfg(test)]
mod tests;
