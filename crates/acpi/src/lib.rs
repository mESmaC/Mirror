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
//! The FADT's reset register follows ACPI 6.6 sections 5.2.3.2 (the Generic
//! Address Structure) and 5.2.9 (the FADT, its Flags and RESET_REG).
//! These are x86 topology, optional timer and reset descriptors, not an AML
//! interpreter. Capacity and table-size limits are kernel policy and bound
//! each table parse.

pub const RSDP_V1_SIZE: usize = 20;
pub const RSDP_V2_SIZE: usize = 36;
pub const SDT_HEADER_SIZE: usize = 36;
pub const HPET_TABLE_SIZE: usize = 56;
/// The shortest FADT that holds RESET_VALUE, at offset 128 (spec 0058a, D2).
pub const FADT_RESET_MIN_LENGTH: usize = 129;
/// The PIIX/ICH reset control register.
pub const RESET_CONTROL_PORT: u16 = 0xcf9;
/// The i8042 command port, and its pulse-reset-line command.
pub const KBC_COMMAND_PORT: u16 = 0x64;
pub const KBC_RESET_PULSE: u8 = 0xfe;
pub const MAX_RSDP_SIZE: usize = 4096;
pub const MAX_TABLE_SIZE: usize = 256 * 1024;
pub const MAX_ROOT_ENTRIES: usize = 256;
/// The most usable CPU records retained. Enabled records take priority over
/// online-capable ones, and the rest are omitted and counted (0013-D4).
pub const MAX_CPUS: usize = 64;
pub const MAX_IO_APICS: usize = 8;
pub const MAX_OVERRIDES: usize = 16;
/// One record per LINT for each retained CPU plus one wildcard per LINT, the
/// most that remains once unmatched and redundant records are dropped.
pub const MAX_LOCAL_NMIS: usize = 2 * MAX_CPUS + 2;
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
    /// The FADT does not set RESET_REG_SUP.
    Unsupported,
    /// The reset register is not in system I/O space.
    AddressSpace,
    /// The reset register is not 8 bits wide.
    Width,
    /// The reset register has a nonzero bit offset.
    Offset,
    /// The reset register's access size is neither undefined nor byte.
    AccessSize,
    /// The reset register is a port other than 0xCF9 or 0x64.
    Port,
    /// The reset value is not a hard reset for its port.
    Value,
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
/// The table supplies only the register address, which the driver must still
/// validate against platform memory ownership. The hardware GCAP_ID register,
/// not this table, is authoritative for the block's identity and capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hpet {
    /// Firmware's copy of GCAP_ID[31:0], stored without validation.
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
        // The block ID is not validated here: the driver checks REV_ID and
        // bit 14 in the hardware register, which wins over this copy.
        let event_timer_block_id = u32_at(bytes, 36);
        // Only the GAS space ID and address are used. Firmware commonly
        // encodes a register width of 64 although ACPI 6.6 section 5.2.3.2
        // gives a register block zero width and offset, so the width, offset
        // and access size bytes are ignored. System I/O is unsupported.
        if bytes[40] != 0 {
            return Err(Error::Address);
        }
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

/// The FADT's reset register, accepted fail-closed in the only two shapes
/// Mirror drives (spec 0058a, D2): an 8-bit system I/O register at 0xCF9 whose
/// value sets bits 1 and 2 (a hard reset) and clears bits 4-7, or the i8042
/// command port with its pulse command. Memory and PCI configuration space
/// registers, and any other port, are rejected: an arbitrary port could belong
/// to another driver. Nothing here touches the port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FadtReset {
    pub revision: u8,
    pub port: u16,
    pub value: u8,
}

impl FadtReset {
    /// Checks, in order: the table header (length bound, then checksum), the
    /// FACP signature, a length that holds RESET_VALUE, revision 2 or later,
    /// RESET_REG_SUP, the GAS (space, width, offset, access size, address),
    /// the port and the value. The first failure is the error.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        const FLAGS: usize = 112;
        const RESET_REG_SUP: u32 = 1 << 10;
        const RESET_REGISTER: usize = 116;
        const RESET_VALUE: usize = 128;
        const SYSTEM_IO: u8 = 1;
        let table = Sdt::parse(bytes)?;
        if table.signature() != *b"FACP" {
            return Err(Error::Signature);
        }
        let bytes = table.bytes;
        if bytes.len() < FADT_RESET_MIN_LENGTH {
            return Err(Error::Length);
        }
        let revision = table.header.revision;
        // Revision 1 (ACPI 1.0) has no reset register; its bytes are reserved.
        if revision < 2 {
            return Err(Error::Revision);
        }
        if u32_at(bytes, FLAGS) & RESET_REG_SUP == 0 {
            return Err(Error::Unsupported);
        }
        let register = &bytes[RESET_REGISTER..RESET_VALUE];
        if register[0] != SYSTEM_IO {
            return Err(Error::AddressSpace);
        }
        if register[1] != 8 {
            return Err(Error::Width);
        }
        if register[2] != 0 {
            return Err(Error::Offset);
        }
        // 0 is the pre-ACPI 3.0 "undefined" encoding, 1 is byte access.
        if register[3] > 1 {
            return Err(Error::AccessSize);
        }
        let port = u16::try_from(u64_at(register, 4))
            .ok()
            .filter(|port| *port != 0)
            .ok_or(Error::Address)?;
        let value = bytes[RESET_VALUE];
        match port {
            // Bit 1 clear would select a CPU-only reset, and bits 4-7 are
            // reserved; bit 3 (full reset) and bit 0 are the firmware's choice.
            RESET_CONTROL_PORT if value & 0xf6 == 0x06 => {}
            KBC_COMMAND_PORT if value == KBC_RESET_PULSE => {}
            RESET_CONTROL_PORT | KBC_COMMAND_PORT => return Err(Error::Value),
            _ => return Err(Error::Port),
        }
        Ok(Self {
            revision,
            port,
            value,
        })
    }
}

/// A validated RSDT or XSDT. Its entries are firmware addresses that the
/// caller reads one at a time, so one bad entry costs only that table.
#[derive(Clone, Copy, Debug)]
pub struct Root<'a> {
    entries: &'a [u8],
    width: usize,
}

impl<'a> Root<'a> {
    /// Validates the table, its entry width and the entry cap. Null and
    /// repeated entries are not errors: `addresses` skips them.
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
        // The cap counts every entry, null and repeated ones included, and
        // bounds the quadratic repeat check in `addresses`.
        if entries.len() / width > MAX_ROOT_ENTRIES {
            return Err(Error::Capacity);
        }
        Ok(Self { entries, width })
    }

    /// Table addresses in root order. A null entry is skipped, and an address
    /// that appears more than once is yielded only at its first position.
    pub fn addresses(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.entries.len() / self.width).filter_map(move |index| {
            let address = self.entry(index);
            let repeated = (0..index).any(|prior| self.entry(prior) == address);
            (address != 0 && !repeated).then_some(address)
        })
    }

    fn entry(&self, index: usize) -> u64 {
        let offset = index * self.width;
        if self.width == 4 {
            u32_at(self.entries, offset) as u64
        } else {
            u64_at(self.entries, offset)
        }
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

/// MADT records that were valid but dropped rather than rejecting the table
/// (0013-D3, D4). Each count is bounded by the records in one table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MadtDiagnostics {
    /// Usable CPU records beyond MAX_CPUS.
    pub omitted_cpus: u32,
    /// Local NMI records that repeat an earlier record's LINT, target and
    /// resolved mode.
    pub merged_nmis: u32,
    /// Local NMI records that repeat an earlier record's LINT and target with
    /// a different resolved mode. The earlier record is kept.
    pub conflicting_nmis: u32,
    /// UID-specific local NMI records whose UID matches no retained CPU.
    pub unmatched_nmis: u32,
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
    diagnostics: MadtDiagnostics,
}

impl Topology {
    /// Parses the MADT in three bounded walks over its records. The first
    /// frames every record and validates and counts the usable CPU records,
    /// the second retains CPUs by 0013-D4's priority, and the third counts
    /// the omitted CPUs and handles every other record. Local NMI records are
    /// therefore matched against the retained CPUs wherever they appear in
    /// the table. Surplus CPU records and redundant or unmatched local NMI
    /// records are dropped and counted in `diagnostics`; every other
    /// duplicate or surplus record still rejects the table.
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
            diagnostics: MadtDiagnostics::default(),
        };
        let records = &bytes[44..];
        let mut enabled = 0;
        for record in Records(records) {
            if let Some(cpu) = cpu_record(record?)? {
                enabled += usize::from(cpu.enabled);
            }
        }
        let mut selection = CpuSelection::new(enabled);
        for record in Records(records) {
            if let Some(cpu) = cpu_record(record?)?
                && selection.retains(&cpu)
            {
                result.add_cpu(cpu)?;
            }
        }
        // Replays the second walk's selection to find the omitted records.
        let mut selection = CpuSelection::new(enabled);
        let mut address_override = false;
        for record in Records(records) {
            let record = record?;
            match record[0] {
                0 | 9 => {
                    if let Some(cpu) = cpu_record(record)?
                        && !selection.retains(&cpu)
                    {
                        result.omit_cpu(&cpu)?;
                    }
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

    pub fn diagnostics(&self) -> MadtDiagnostics {
        self.diagnostics
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

    /// Retains a usable CPU. `CpuSelection` admits at most MAX_CPUS, so the
    /// push cannot fail on capacity.
    fn add_cpu(&mut self, cpu: Cpu) -> Result<(), Error> {
        self.check_cpu_identity(&cpu)?;
        push(&mut self.cpus, &mut self.cpu_count, cpu)
    }

    /// Counts a usable CPU that did not fit. Its identity is still checked
    /// against every retained CPU, so a record cannot claim the UID or APIC
    /// ID of a CPU that Mirror starts. Two omitted records are not compared
    /// with each other, which would need a quadratic walk of the table, and
    /// neither is used.
    fn omit_cpu(&mut self, cpu: &Cpu) -> Result<(), Error> {
        self.check_cpu_identity(cpu)?;
        self.diagnostics.omitted_cpus += 1;
        Ok(())
    }

    fn check_cpu_identity(&self, cpu: &Cpu) -> Result<(), Error> {
        if self
            .cpus()
            .iter()
            .any(|prior| prior.uid == cpu.uid || prior.apic_id == cpu.apic_id)
        {
            Err(Error::Duplicate)
        } else {
            Ok(())
        }
    }

    /// Records one local NMI connection (0013-D3, D4). A UID-specific record
    /// whose UID matches no retained CPU is dropped. A record that repeats an
    /// earlier record's LINT and target, meaning the same UID in either
    /// record encoding or the wildcard in either, is dropped and the earlier
    /// record wins. A UID-specific record and a wildcard are distinct
    /// targets, so both are kept, and the APIC driver applies the specific
    /// one to its CPU. Stored records keep firmware's raw flags.
    fn add_local_nmi(&mut self, entry: LocalNmi) -> Result<(), Error> {
        if entry.lint > 1 {
            return Err(Error::Reserved);
        }
        if let Some(uid) = entry.processor_uid
            && !self.cpus().iter().any(|cpu| cpu.uid == uid)
        {
            self.diagnostics.unmatched_nmis += 1;
            return Ok(());
        }
        let same_mode = self
            .local_nmis()
            .iter()
            .find(|prior| prior.lint == entry.lint && prior.processor_uid == entry.processor_uid)
            .map(|prior| prior.resolved_mode() == entry.resolved_mode());
        match same_mode {
            Some(true) => self.diagnostics.merged_nmis += 1,
            Some(false) => self.diagnostics.conflicting_nmis += 1,
            // The retained CPUs have distinct UIDs, so at most one record per
            // LINT for each of them plus one wildcard per LINT reaches here,
            // and MAX_LOCAL_NMIS holds them all.
            None => push(&mut self.local_nmis, &mut self.local_nmi_count, entry)?,
        }
        Ok(())
    }
}

impl LocalNmi {
    /// The electrical mode with "conforms to bus" resolved to active-high and
    /// edge-triggered, as the APIC driver programs it.
    fn resolved_mode(&self) -> (Polarity, Trigger) {
        (
            self.polarity.unwrap_or(Polarity::ActiveHigh),
            self.trigger.unwrap_or(Trigger::Edge),
        )
    }
}

/// Walks the MADT's interrupt controller structures. Each item is one record,
/// bounded by its declared length; a framing error ends the walk.
struct Records<'a>(&'a [u8]);

impl<'a> Iterator for Records<'a> {
    type Item = Result<&'a [u8], Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let length = match self.0.get(1) {
            None => Err(Error::Truncated),
            Some(&length) if length < 2 => Err(Error::RecordLength),
            Some(&length) if self.0.len() < length as usize => Err(Error::Truncated),
            Some(&length) => Ok(length as usize),
        };
        Some(match length {
            Ok(length) => {
                let (record, remaining) = self.0.split_at(length);
                self.0 = remaining;
                Ok(record)
            }
            Err(error) => {
                self.0 = &[];
                Err(error)
            }
        })
    }
}

/// Decodes a processor local APIC (type 0) or x2APIC (type 9) record. Other
/// types give `None`, and so does a record with neither availability bit set:
/// ACPI requires such a record to be ignored, including identity fields that
/// firmware may leave stale.
fn cpu_record(record: &[u8]) -> Result<Option<Cpu>, Error> {
    let (uid, apic_id, flags, x2apic) = match record[0] {
        0 => {
            record_length(record, 8)?;
            (record[2] as u32, record[3] as u32, u32_at(record, 4), false)
        }
        9 => {
            record_length(record, 16)?;
            reserved(&record[2..4])?;
            (
                u32_at(record, 12),
                u32_at(record, 4),
                u32_at(record, 8),
                true,
            )
        }
        _ => return Ok(None),
    };
    if flags & 3 == 0 {
        return Ok(None);
    }
    if flags & !3 != 0 || flags & 3 == 3 {
        return Err(Error::Reserved);
    }
    if apic_id == u32::MAX || (!x2apic && apic_id == 0xff) {
        return Err(Error::Address);
    }
    Ok(Some(Cpu {
        uid,
        apic_id,
        enabled: flags & 1 != 0,
        online_capable: flags & 2 != 0,
        x2apic,
    }))
}

/// 0013-D4's CPU retention. Enabled records take priority over
/// online-capable ones, and within each kind the earliest records in the
/// table are kept, so the retained CPUs stay in table order.
struct CpuSelection {
    enabled: usize,
    online_capable: usize,
}

impl CpuSelection {
    fn new(enabled: usize) -> Self {
        let enabled = enabled.min(MAX_CPUS);
        Self {
            enabled,
            online_capable: MAX_CPUS - enabled,
        }
    }

    /// Takes the usable CPU records in table order, one call each, and says
    /// whether each one is retained.
    fn retains(&mut self, cpu: &Cpu) -> bool {
        let remaining = if cpu.enabled {
            &mut self.enabled
        } else {
            &mut self.online_capable
        };
        let retained = *remaining > 0;
        *remaining = remaining.saturating_sub(1);
        retained
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
