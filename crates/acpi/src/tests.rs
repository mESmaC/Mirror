extern crate std;

use super::*;
use std::{vec, vec::Vec};

fn repair_checksum(bytes: &mut [u8], offset: usize) {
    bytes[offset] = 0;
    bytes[offset] = 0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
}

fn sdt(signature: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let length = SDT_HEADER_SIZE + body.len();
    let mut bytes = vec![0; length];
    bytes[..4].copy_from_slice(signature);
    bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    bytes[8] = 1;
    bytes[SDT_HEADER_SIZE..].copy_from_slice(body);
    repair_checksum(&mut bytes, 9);
    bytes
}

fn rsdp(revision: u8, rsdt: u32, xsdt: u64) -> Vec<u8> {
    let length = if revision == 0 { 20 } else { 36 };
    let mut bytes = vec![0; length];
    bytes[..8].copy_from_slice(b"RSD PTR ");
    bytes[15] = revision;
    bytes[16..20].copy_from_slice(&rsdt.to_le_bytes());
    if length >= 36 {
        bytes[20..24].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[24..32].copy_from_slice(&xsdt.to_le_bytes());
    }
    repair_checksum(&mut bytes[..20], 8);
    if length >= 36 {
        repair_checksum(&mut bytes, 32);
    }
    bytes
}

fn madt(records: &[u8]) -> Vec<u8> {
    let mut body = Vec::from(0xfee0_0000u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(records);
    sdt(b"APIC", &body)
}

fn hpet() -> Vec<u8> {
    // Match QEMU's HPET descriptor, including zero GAS width/access size and
    // the valid zero minimum periodic tick. It advertises three comparators.
    let mut body = vec![0; HPET_TABLE_SIZE - SDT_HEADER_SIZE];
    body[..4].copy_from_slice(&0x8086_a201_u32.to_le_bytes());
    body[8..16].copy_from_slice(&0xfed0_0000_u64.to_le_bytes());
    sdt(b"HPET", &body)
}

#[test]
fn hpet_preserves_descriptor_and_driver_policy_fields() {
    let mut bytes = hpet();
    assert_eq!(
        Hpet::parse(&bytes),
        Ok(Hpet {
            event_timer_block_id: 0x8086_a201,
            address: 0xfed0_0000,
            number: 0,
            minimum_tick: 0,
            page_protection: 0,
            oem_attributes: 0,
        })
    );
    // Keep a valid 32-bit descriptor: the 64-bit clock restriction belongs to
    // the driver. The parser also does not impose a 32-bit address ceiling or
    // the driver's stricter page alignment on the physical GAS address.
    bytes[36..40].copy_from_slice(&0x1234_0102_u32.to_le_bytes());
    bytes[44..52].copy_from_slice(&0x1_0000_0008_u64.to_le_bytes());
    bytes[52] = 7;
    bytes[53..55].copy_from_slice(&0xffff_u16.to_le_bytes());
    bytes[55] = 0xa2;
    repair_checksum(&mut bytes, 9);
    assert_eq!(
        Hpet::parse(&bytes),
        Ok(Hpet {
            event_timer_block_id: 0x1234_0102,
            address: 0x1_0000_0008,
            number: 7,
            minimum_tick: u16::MAX,
            page_protection: 2,
            oem_attributes: 10,
        })
    );
}

#[test]
fn hpet_bounds_declared_length_and_checksum() {
    let bytes = hpet();
    for length in 0..bytes.len() {
        assert_eq!(Hpet::parse(&bytes[..length]), Err(Error::Truncated));
    }
    for length in SDT_HEADER_SIZE..HPET_TABLE_SIZE {
        let mut short = bytes[..length].to_vec();
        short[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        repair_checksum(&mut short, 9);
        assert_eq!(Hpet::parse(&short), Err(Error::Truncated));
    }
    for length in [0, 35, MAX_TABLE_SIZE + 1] {
        let mut invalid = bytes.clone();
        invalid[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        assert_eq!(Hpet::parse(&invalid), Err(Error::Length));
    }
    // The whole declared table is checksummed, including extension bytes.
    let mut extended = bytes.clone();
    extended.extend_from_slice(&[0x23, 0x45]);
    let length = extended.len() as u32;
    extended[4..8].copy_from_slice(&length.to_le_bytes());
    repair_checksum(&mut extended, 9);
    assert_eq!(Hpet::parse(&extended), Hpet::parse(&bytes));
    extended[HPET_TABLE_SIZE] ^= 1;
    assert_eq!(Hpet::parse(&extended), Err(Error::Checksum));
    // Bytes beyond the declared table do not change its checksum or fields.
    let mut mapped = bytes.clone();
    mapped.extend_from_slice(&[0xff; 16]);
    assert_eq!(Hpet::parse(&mapped), Hpet::parse(&bytes));
    let mut invalid = bytes;
    invalid[9] ^= 1;
    assert_eq!(Hpet::parse(&invalid), Err(Error::Checksum));
}

#[test]
fn hpet_rejects_signature_revision_and_reserved_block_id() {
    let mut bytes = hpet();
    bytes[..4].copy_from_slice(b"APIC");
    repair_checksum(&mut bytes, 9);
    assert_eq!(Hpet::parse(&bytes), Err(Error::Signature));
    for revision in [0, 2, 0xff] {
        let mut bytes = hpet();
        bytes[8] = revision;
        repair_checksum(&mut bytes, 9);
        assert_eq!(Hpet::parse(&bytes), Err(Error::Revision));
    }
    let mut bytes = hpet();
    bytes[36] = 0;
    repair_checksum(&mut bytes, 9);
    assert_eq!(Hpet::parse(&bytes), Err(Error::Revision));
    let mut bytes = hpet();
    bytes[37] |= 1 << 6;
    repair_checksum(&mut bytes, 9);
    assert_eq!(Hpet::parse(&bytes), Err(Error::Reserved));
}

#[test]
fn hpet_rejects_unsupported_gas_and_invalid_addresses() {
    for space in [1, 2, 0x7f, 0xff] {
        let mut bytes = hpet();
        bytes[40] = space;
        repair_checksum(&mut bytes, 9);
        assert_eq!(Hpet::parse(&bytes), Err(Error::Address));
    }
    for offset in 41..44 {
        for value in [1, 4, 32, 64, 0xff] {
            let mut bytes = hpet();
            bytes[offset] = value;
            repair_checksum(&mut bytes, 9);
            assert_eq!(Hpet::parse(&bytes), Err(Error::Reserved));
        }
    }
    for address in [
        0,
        1,
        0xfed0_0001,
        0xfed0_0004,
        u64::MAX - 7,
        u64::MAX - 1023,
    ] {
        let mut bytes = hpet();
        bytes[44..52].copy_from_slice(&address.to_le_bytes());
        repair_checksum(&mut bytes, 9);
        assert_eq!(Hpet::parse(&bytes), Err(Error::Address));
    }
}

#[test]
fn hpet_distinguishes_page_protection_from_oem_attributes() {
    for oem in 0..16 {
        for protection in 0..16 {
            let mut bytes = hpet();
            bytes[55] = (oem << 4) | protection;
            repair_checksum(&mut bytes, 9);
            let result = Hpet::parse(&bytes);
            if protection <= 2 {
                let value = result.unwrap();
                assert_eq!(value.page_protection, protection);
                assert_eq!(value.oem_attributes, oem);
            } else {
                assert_eq!(result, Err(Error::Reserved));
            }
        }
    }
}

#[test]
fn arbitrary_checksummed_hpet_payloads_remain_bounded() {
    let mut state = 0x3141_5926_u32;
    for length in SDT_HEADER_SIZE..128 {
        for _ in 0..16 {
            let mut bytes = sdt(b"HPET", &vec![0; length - SDT_HEADER_SIZE]);
            for byte in &mut bytes[SDT_HEADER_SIZE..] {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *byte = state as u8;
            }
            repair_checksum(&mut bytes, 9);
            let _ = Hpet::parse(&bytes);
        }
    }
}

fn cpu(uid: u8, apic: u8, flags: u32) -> Vec<u8> {
    let mut bytes = vec![0, 8, uid, apic];
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes
}

fn x2cpu(uid: u32, apic: u32, flags: u32) -> Vec<u8> {
    let mut bytes = vec![9, 16, 0, 0];
    bytes.extend_from_slice(&apic.to_le_bytes());
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&uid.to_le_bytes());
    bytes
}

fn io_apic(id: u8, address: u32, gsi: u32) -> Vec<u8> {
    let mut bytes = vec![1, 12, id, 0];
    bytes.extend_from_slice(&address.to_le_bytes());
    bytes.extend_from_slice(&gsi.to_le_bytes());
    bytes
}

fn iso(irq: u8, gsi: u32, flags: u16) -> Vec<u8> {
    let mut bytes = vec![2, 10, 0, irq];
    bytes.extend_from_slice(&gsi.to_le_bytes());
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes
}

fn nmi(gsi: u32, flags: u16) -> Vec<u8> {
    let mut bytes = vec![3, 8];
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&gsi.to_le_bytes());
    bytes
}

fn local_nmi(uid: u8, lint: u8, flags: u16) -> Vec<u8> {
    let mut bytes = vec![4, 6, uid];
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.push(lint);
    bytes
}

fn x2nmi(uid: u32, lint: u8, flags: u16) -> Vec<u8> {
    let mut bytes = vec![10, 12];
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&uid.to_le_bytes());
    bytes.extend_from_slice(&[lint, 0, 0, 0]);
    bytes
}

fn lapic_override(address: u64) -> Vec<u8> {
    let mut bytes = vec![5, 12, 0, 0];
    bytes.extend_from_slice(&address.to_le_bytes());
    bytes
}

fn topology_error(records: &[u8], expected: Error) {
    assert_eq!(Topology::parse(&madt(records)).unwrap_err(), expected);
}

#[test]
fn rsdp_selects_xsdt_and_preserves_legacy_fallback() {
    assert_eq!(
        Rsdp::parse(&rsdp(0, 0x1234, 0)).unwrap(),
        Rsdp {
            revision: 0,
            root_address: 0x1234,
            root_kind: RootKind::Rsdt
        }
    );
    let modern = rsdp(2, 0x1234, 0x1_2345_6789);
    assert_eq!(Rsdp::required_len(&modern[..24]), Ok(36));
    assert_eq!(Rsdp::parse(&modern).unwrap().root_address, 0x1_2345_6789);
    assert_eq!(Rsdp::parse(&modern).unwrap().root_kind, RootKind::Xsdt);
    assert_eq!(
        Rsdp::parse(&rsdp(2, 0x1234, 0)).unwrap().root_kind,
        RootKind::Rsdt
    );
    assert_eq!(Rsdp::parse(&rsdp(0, 0, 0)), Err(Error::Address));
    assert_eq!(Rsdp::parse(&rsdp(2, 0, 0)), Err(Error::Address));
}

#[test]
fn rsdp_validates_both_checksums_independently() {
    let mut bytes = rsdp(2, 0x1234, 0x12345678);
    bytes[9] ^= 1;
    repair_checksum(&mut bytes, 32);
    assert_eq!(Rsdp::parse(&bytes), Err(Error::Checksum));
    let mut bytes = rsdp(2, 0x1234, 0x12345678);
    bytes[24] ^= 1;
    assert_eq!(Rsdp::parse(&bytes), Err(Error::Checksum));
    let mut bytes = rsdp(2, 0x1234, 0x12345678);
    bytes[35] = 1;
    repair_checksum(&mut bytes, 32);
    assert_eq!(Rsdp::parse(&bytes), Err(Error::Reserved));
}

#[test]
fn rsdp_bounds_prefix_and_extended_length() {
    let modern = rsdp(2, 0x1234, 0x12345678);
    for length in 0..modern.len() {
        assert_eq!(Rsdp::parse(&modern[..length]), Err(Error::Truncated));
    }
    assert_eq!(Rsdp::parse(&rsdp(1, 1, 1)), Err(Error::Revision));
    for length in [0, 20, 35, MAX_RSDP_SIZE as u32 + 1, u32::MAX] {
        let mut bytes = modern.clone();
        bytes[20..24].copy_from_slice(&length.to_le_bytes());
        assert_eq!(Rsdp::required_len(&bytes), Err(Error::Length));
    }
    let mut bytes = modern.clone();
    bytes.resize(MAX_RSDP_SIZE, 0x3c);
    bytes[15] = 3; // Future revisions may append fields.
    bytes[20..24].copy_from_slice(&(MAX_RSDP_SIZE as u32).to_le_bytes());
    repair_checksum(&mut bytes[..20], 8);
    repair_checksum(&mut bytes, 32);
    assert_eq!(Rsdp::parse(&bytes).unwrap().revision, 3);
    bytes.push(99); // The following memory is outside the declared checksum.
    assert!(Rsdp::parse(&bytes).is_ok());
    bytes[0] = b'X';
    assert_eq!(Rsdp::parse(&bytes), Err(Error::Signature));
}

#[test]
fn sdt_header_mapping_and_checksum_are_separate() {
    let mut bytes = sdt(b"TEST", &[1, 2, 3]);
    assert_eq!(Header::parse(&bytes[..36]).unwrap().length, 39);
    assert_eq!(Sdt::parse(&bytes[..36]).unwrap_err(), Error::Truncated);
    assert_eq!(Sdt::parse(&bytes).unwrap().bytes().len(), 39);
    bytes.push(7);
    assert_eq!(Sdt::parse(&bytes).unwrap().bytes().len(), 39);
    bytes[38] ^= 1;
    assert_eq!(Sdt::parse(&bytes).unwrap_err(), Error::Checksum);
    for length in [0, 35, MAX_TABLE_SIZE as u32 + 1, u32::MAX] {
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        assert_eq!(Header::parse(&bytes), Err(Error::Length));
    }
    for length in 0..36 {
        assert_eq!(Header::parse(&bytes[..length]), Err(Error::Truncated));
    }
}

#[test]
fn roots_validate_width_duplicates_null_and_capacity() {
    let rsdt = sdt(b"RSDT", &[0x34, 0x12, 0, 0, 0x78, 0x56, 0, 0]);
    assert_eq!(
        Root::parse(&rsdt, RootKind::Rsdt)
            .unwrap()
            .addresses()
            .collect::<Vec<_>>(),
        [0x1234, 0x5678]
    );
    let xsdt = sdt(b"XSDT", &0x1_2345_6789u64.to_le_bytes());
    assert_eq!(
        Root::parse(&xsdt, RootKind::Xsdt)
            .unwrap()
            .addresses()
            .next(),
        Some(0x1_2345_6789)
    );
    assert_eq!(
        Root::parse(&xsdt, RootKind::Rsdt).unwrap_err(),
        Error::Signature
    );
    for (body, expected) in [
        (vec![1; 7], Error::Length),
        (vec![0; 8], Error::Address),
        (vec![1; 16], Error::Duplicate),
        (vec![1; (MAX_ROOT_ENTRIES + 1) * 8], Error::Capacity),
    ] {
        assert_eq!(
            Root::parse(&sdt(b"XSDT", &body), RootKind::Xsdt).unwrap_err(),
            expected
        );
    }
    let mut body = Vec::new();
    for address in 1..=MAX_ROOT_ENTRIES as u64 {
        body.extend_from_slice(&address.to_le_bytes());
    }
    assert_eq!(
        Root::parse(&sdt(b"XSDT", &body), RootKind::Xsdt)
            .unwrap()
            .addresses()
            .len(),
        MAX_ROOT_ENTRIES
    );
}

#[test]
fn topology_discovers_enabled_and_online_capable_cpus() {
    let mut records = cpu(0, 0, 1);
    records.extend(cpu(1, 2, 2));
    records.extend(cpu(0, 0, 0)); // Firmware placeholders are ignored.
    records.extend(cpu(0xff, 0xff, 0x8000_0000));
    records.extend(x2cpu(700, 0x1234, 1));
    records.extend(io_apic(2, 0xfec0_0000, 0));
    records.extend(local_nmi(0xff, 1, 0));
    let topology = Topology::parse(&madt(&records)).unwrap();
    assert_eq!(topology.lapic_address, 0xfee0_0000);
    assert!(topology.legacy_pic);
    assert_eq!(topology.cpus().len(), 3);
    assert!(topology.cpus()[0].enabled);
    assert!(!topology.cpus()[1].enabled);
    assert!(topology.cpus()[1].online_capable);
    assert_eq!(
        topology.cpus()[2],
        Cpu {
            uid: 700,
            apic_id: 0x1234,
            enabled: true,
            online_capable: false,
            x2apic: true
        }
    );
    assert_eq!(
        topology.io_apics()[0],
        IoApic {
            id: 2,
            address: 0xfec0_0000,
            gsi_base: 0
        }
    );
    assert_eq!(topology.local_nmis()[0].processor_uid, None);
    assert_eq!(topology.local_nmis()[0].polarity, None);
}

#[test]
fn processor_identity_and_flags_reject_conflicting_firmware() {
    for (first, second) in [
        (cpu(0, 0, 1), cpu(0, 1, 1)),
        (cpu(0, 0, 1), cpu(1, 0, 2)),
        (cpu(0, 0, 1), x2cpu(0, 300, 1)),
        (cpu(0, 0, 1), x2cpu(300, 0, 1)),
    ] {
        topology_error(&[first, second].concat(), Error::Duplicate);
    }
    for flags in [3, 5, 6, u32::MAX] {
        topology_error(&cpu(0, 0, flags), Error::Reserved);
        topology_error(&x2cpu(0, 0, flags), Error::Reserved);
    }
    topology_error(&cpu(0, 0xff, 1), Error::Address);
    topology_error(&x2cpu(0, u32::MAX, 1), Error::Address);
    let mut record = x2cpu(0, 0, 1);
    record[2] = 1;
    topology_error(&record, Error::Reserved);
}

#[test]
fn isa_overrides_resolve_defaults_and_repurposed_inputs() {
    let mut records = iso(0, 2, 0);
    records.extend(iso(9, 9, 0xf));
    records.extend(iso(4, 20, 5));
    let topology = Topology::parse(&madt(&records)).unwrap();
    assert_eq!(
        topology.isa_irq(0),
        Ok(IrqRoute {
            gsi: 2,
            polarity: Polarity::ActiveHigh,
            trigger: Trigger::Edge
        })
    );
    assert_eq!(
        topology.isa_irq(1),
        Ok(IrqRoute {
            gsi: 1,
            polarity: Polarity::ActiveHigh,
            trigger: Trigger::Edge
        })
    );
    assert_eq!(
        topology.isa_irq(9),
        Ok(IrqRoute {
            gsi: 9,
            polarity: Polarity::ActiveLow,
            trigger: Trigger::Level
        })
    );
    assert_eq!(topology.isa_irq(4).unwrap().gsi, 20);
    assert_eq!(topology.isa_irq(2), Err(Error::Conflict));
    assert_eq!(topology.isa_irq(16), Err(Error::InvalidIsaIrq));
    let records = [iso(0, 2, 0), iso(2, 0, 0)].concat();
    let topology = Topology::parse(&madt(&records)).unwrap();
    assert_eq!(topology.isa_irq(0).unwrap().gsi, 2);
    assert_eq!(topology.isa_irq(2).unwrap().gsi, 0);
}

#[test]
fn interrupt_flags_preserve_conformance_and_reject_reserved_encodings() {
    for flags in 0..=0xffu16 {
        let result = Topology::parse(&madt(&iso(5, 5, flags)));
        let valid = flags & !0xf == 0 && flags & 3 != 2 && (flags >> 2) & 3 != 2;
        assert_eq!(result.is_ok(), valid, "flags={flags:x}");
    }
    for flags in [0x100, 0x1000, 0x8000] {
        topology_error(&iso(5, 5, flags), Error::Reserved);
    }
    let records = [local_nmi(4, 1, 0xd), x2nmi(700, 0, 3), nmi(40, 0)].concat();
    let topology = Topology::parse(&madt(&records)).unwrap();
    assert_eq!(
        topology.local_nmis()[0].polarity,
        Some(Polarity::ActiveHigh)
    );
    assert_eq!(topology.local_nmis()[0].trigger, Some(Trigger::Level));
    assert_eq!(topology.local_nmis()[1].processor_uid, Some(700));
    assert_eq!(topology.local_nmis()[1].polarity, Some(Polarity::ActiveLow));
    assert_eq!(topology.local_nmis()[1].trigger, None);
    assert_eq!(topology.nmi_sources()[0].polarity, None);
}

#[test]
fn duplicate_irq_overrides_and_nmi_conflicts_are_rejected() {
    topology_error(&[iso(0, 2, 0), iso(0, 4, 0)].concat(), Error::Duplicate);
    topology_error(&[iso(0, 2, 0), iso(1, 2, 0)].concat(), Error::Duplicate);
    topology_error(&iso(16, 16, 0), Error::InvalidIsaIrq);
    let mut invalid_bus = iso(0, 2, 0);
    invalid_bus[2] = 1;
    topology_error(&invalid_bus, Error::Reserved);
    for records in [[iso(0, 2, 0), nmi(2, 0)], [nmi(2, 0), iso(0, 2, 0)]] {
        topology_error(&records.concat(), Error::Conflict);
    }
    topology_error(&[nmi(2, 0), nmi(2, 5)].concat(), Error::Duplicate);
    let topology = Topology::parse(&madt(&nmi(3, 0))).unwrap();
    assert_eq!(topology.isa_irq(3), Err(Error::Conflict));
    assert!(topology.isa_irq(2).is_ok());
}

#[test]
fn local_nmi_wildcards_and_duplicate_connections_are_checked() {
    for records in [
        [local_nmi(4, 1, 0), local_nmi(4, 1, 5)],
        [local_nmi(0xff, 1, 0), local_nmi(4, 1, 0)],
        [local_nmi(4, 1, 0), x2nmi(u32::MAX, 1, 0)],
        [x2nmi(4, 1, 0), local_nmi(4, 1, 0)],
    ] {
        topology_error(&records.concat(), Error::Duplicate);
    }
    let topology = Topology::parse(&madt(&x2nmi(u32::MAX, 1, 0))).unwrap();
    assert_eq!(topology.local_nmis()[0].processor_uid, None);
    assert!(topology.local_nmis()[0].x2apic);
    assert!(
        Topology::parse(&madt(
            &[local_nmi(0xff, 0, 0), x2nmi(u32::MAX, 1, 0)].concat()
        ))
        .is_ok()
    );
    topology_error(&local_nmi(0, 2, 0), Error::Reserved);
    topology_error(&x2nmi(0, 2, 0), Error::Reserved);
    let mut record = x2nmi(0, 1, 0);
    record[11] = 1;
    topology_error(&record, Error::Reserved);
}

#[test]
fn controller_addresses_and_duplicate_resources_are_checked() {
    let base = io_apic(1, 0xfec0_0000, 0);
    for duplicate in [
        io_apic(1, 0xfec0_1000, 24),
        io_apic(2, 0xfec0_0000, 24),
        io_apic(2, 0xfec0_1000, 0),
    ] {
        topology_error(&[base.clone(), duplicate].concat(), Error::Duplicate);
    }
    topology_error(&io_apic(1, 0, 0), Error::Address);
    topology_error(&io_apic(1, 0xfec0_0001, 0), Error::Address);
    topology_error(&io_apic(1, 0xfee0_0000, 0), Error::Conflict);
    let mut record = base;
    record[3] = 1;
    topology_error(&record, Error::Reserved);
    let topology = Topology::parse(&madt(&lapic_override(0x1_fee0_0000))).unwrap();
    assert_eq!(topology.lapic_address, 0x1_fee0_0000);
    topology_error(&lapic_override(0), Error::Address);
    topology_error(&lapic_override(0xfee0_0010), Error::Address);
    topology_error(
        &[lapic_override(0xfee0_0000), lapic_override(0xfee0_0000)].concat(),
        Error::Duplicate,
    );
    let mut record = lapic_override(0xfee0_0000);
    record[3] = 1;
    topology_error(&record, Error::Reserved);
}

#[test]
fn madt_checks_header_signature_checksum_and_flags() {
    let good = madt(&cpu(0, 0, 1));
    for length in 0..good.len() {
        assert_eq!(
            Topology::parse(&good[..length]).unwrap_err(),
            Error::Truncated
        );
    }
    let mut bytes = good.clone();
    bytes[10] ^= 1;
    assert_eq!(Topology::parse(&bytes).unwrap_err(), Error::Checksum);
    let mut bytes = good.clone();
    bytes[0] = b'X';
    repair_checksum(&mut bytes, 9);
    assert_eq!(Topology::parse(&bytes).unwrap_err(), Error::Signature);
    let mut bytes = good.clone();
    bytes[40] = 2;
    repair_checksum(&mut bytes, 9);
    assert_eq!(Topology::parse(&bytes).unwrap_err(), Error::Reserved);
    let mut bytes = good;
    bytes[40] = 0;
    repair_checksum(&mut bytes, 9);
    assert!(!Topology::parse(&bytes).unwrap().legacy_pic);
    assert_eq!(
        Topology::parse(&sdt(b"APIC", &[0; 7])).unwrap_err(),
        Error::Truncated
    );
}

#[test]
fn madt_record_lengths_and_unknown_records_are_bounded() {
    topology_error(&[0], Error::Truncated);
    topology_error(&[0, 0], Error::RecordLength);
    topology_error(&[0, 1], Error::RecordLength);
    topology_error(&[0, 8, 0], Error::Truncated);
    for record in [
        cpu(0, 0, 1),
        io_apic(0, 0xfec0_0000, 0),
        iso(0, 2, 0),
        nmi(20, 0),
        local_nmi(0, 1, 0),
        lapic_override(0xfee0_0000),
        x2cpu(0, 0, 1),
        x2nmi(0, 1, 0),
    ] {
        let mut shortened = record.clone();
        shortened.pop();
        shortened[1] -= 1;
        topology_error(&shortened, Error::RecordLength);
        let mut extended = record;
        extended.push(0);
        extended[1] += 1;
        topology_error(&extended, Error::RecordLength);
    }
    let records = [vec![0x80, 2], cpu(0, 0, 1), vec![0x81, 5, 1, 2, 3]].concat();
    assert_eq!(Topology::parse(&madt(&records)).unwrap().cpus().len(), 1);
    topology_error(&[0x80, 255, 0, 0], Error::Truncated);
    for kind in 6..=8 {
        topology_error(&[kind, 2], Error::UnsupportedInterruptModel);
    }
}

#[test]
fn all_topology_capacity_limits_fail_without_partial_results() {
    let mut cpus = Vec::new();
    for index in 0..MAX_CPUS {
        cpus.extend(cpu(index as u8, index as u8, 1));
    }
    assert_eq!(
        Topology::parse(&madt(&cpus)).unwrap().cpus().len(),
        MAX_CPUS
    );
    cpus.extend(cpu(MAX_CPUS as u8, MAX_CPUS as u8, 1));
    topology_error(&cpus, Error::Capacity);

    let mut ios = Vec::new();
    for index in 0..MAX_IO_APICS {
        ios.extend(io_apic(
            index as u8,
            0xfec0_0000 + 4096 * index as u32,
            24 * index as u32,
        ));
    }
    assert_eq!(
        Topology::parse(&madt(&ios)).unwrap().io_apics().len(),
        MAX_IO_APICS
    );
    ios.extend(io_apic(100, 0xfe00_0000, 1000));
    topology_error(&ios, Error::Capacity);

    let mut locals = Vec::new();
    for index in 0..MAX_LOCAL_NMIS {
        locals.extend(x2nmi(index as u32, 1, 0));
    }
    assert_eq!(
        Topology::parse(&madt(&locals)).unwrap().local_nmis().len(),
        MAX_LOCAL_NMIS
    );
    locals.extend(x2nmi(MAX_LOCAL_NMIS as u32, 1, 0));
    topology_error(&locals, Error::Capacity);

    let mut nmis = Vec::new();
    for index in 0..MAX_NMI_SOURCES {
        nmis.extend(nmi(100 + index as u32, 0));
    }
    assert_eq!(
        Topology::parse(&madt(&nmis)).unwrap().nmi_sources().len(),
        MAX_NMI_SOURCES
    );
    nmis.extend(nmi(100 + MAX_NMI_SOURCES as u32, 0));
    topology_error(&nmis, Error::Capacity);

    // All sixteen ISA sources can be overridden. A seventeenth is necessarily
    // a duplicate or an invalid ISA source, rejected before capacity handling.
    let mut overrides = Vec::new();
    for irq in 0..16 {
        overrides.extend(iso(irq, 100 + irq as u32, 0));
    }
    assert_eq!(
        Topology::parse(&madt(&overrides))
            .unwrap()
            .overrides()
            .len(),
        MAX_OVERRIDES
    );
}

#[test]
fn checksummed_arbitrary_madt_payloads_do_not_panic() {
    let mut seed = 0x14bc_9862u32;
    for length in 0..512 {
        let mut records = vec![0; length];
        for byte in &mut records {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *byte = seed as u8;
        }
        let _ = Topology::parse(&madt(&records));
    }
}
