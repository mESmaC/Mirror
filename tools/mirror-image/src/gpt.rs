//! The protective MBR and GPT of decision 0015-D5 (UEFI 2.10, sections 5.2
//! and 5.3): the primary header at LBA 1 with 128 entries at LBA 2-33, and the
//! backup entries and header in the last 33 sectors.

use crate::crc32::crc32;
use crate::{
    DISK_SECTORS, ESP_FIRST_LBA, ESP_LAST_LBA, Error, ImageReader, ROOT_FIRST_LBA, ROOT_LAST_LBA,
    Result, SECTOR_SIZE,
};

pub const ENTRY_COUNT: usize = 128;
pub const ENTRY_SIZE: usize = 128;
const ENTRIES_BYTES: usize = ENTRY_COUNT * ENTRY_SIZE;
const ENTRY_SECTORS: u64 = ENTRIES_BYTES as u64 / SECTOR_SIZE;
pub const PRIMARY_HEADER_LBA: u64 = 1;
pub const PRIMARY_ENTRIES_LBA: u64 = 2;
pub const LAST_PRIMARY_LBA: u64 = PRIMARY_ENTRIES_LBA + ENTRY_SECTORS - 1;
pub const FIRST_USABLE_LBA: u64 = LAST_PRIMARY_LBA + 1;
pub const BACKUP_HEADER_LBA: u64 = DISK_SECTORS - 1;
pub const BACKUP_ENTRIES_LBA: u64 = BACKUP_HEADER_LBA - ENTRY_SECTORS;
pub const LAST_USABLE_LBA: u64 = BACKUP_ENTRIES_LBA - 1;

const _: () = assert!(LAST_PRIMARY_LBA == 33 && BACKUP_ENTRIES_LBA == 2_097_119);

const SIGNATURE: &[u8; 8] = b"EFI PART";
const REVISION: u32 = 0x0001_0000;
const HEADER_SIZE: usize = 92;

pub const EFI_SYSTEM_TYPE: [u8; 16] = guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
pub const LINUX_DATA_TYPE: [u8; 16] = guid("0FC63DAF-8483-4772-8E79-3D69D8477DE4");
/// Fixed identifiers, so every build writes the same bytes. The values are
/// arbitrary; their first six bytes spell "MIRROR" in ASCII.
pub const DISK_GUID: [u8; 16] = guid("4D495252-4F52-4000-8000-000000000000");
pub const ESP_GUID: [u8; 16] = guid("4D495252-4F52-4000-8000-000000000001");
pub const ROOT_GUID: [u8; 16] = guid("4D495252-4F52-4000-8000-000000000002");

pub struct Partition {
    pub name: &'static str,
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
}

pub const PARTITIONS: [Partition; 2] = [
    Partition {
        name: "MIRROR-ESP",
        type_guid: EFI_SYSTEM_TYPE,
        unique_guid: ESP_GUID,
        first_lba: ESP_FIRST_LBA,
        last_lba: ESP_LAST_LBA,
    },
    Partition {
        name: "mirror-root",
        type_guid: LINUX_DATA_TYPE,
        unique_guid: ROOT_GUID,
        first_lba: ROOT_FIRST_LBA,
        last_lba: ROOT_LAST_LBA,
    },
];

pub struct Tables {
    pub mbr: [u8; 512],
    pub primary_header: [u8; 512],
    pub entries: Vec<u8>,
    pub backup_header: [u8; 512],
}

pub fn tables() -> Tables {
    let entries = entries();
    let entries_crc = crc32(&entries);
    Tables {
        mbr: protective_mbr(),
        primary_header: header(
            PRIMARY_HEADER_LBA,
            BACKUP_HEADER_LBA,
            PRIMARY_ENTRIES_LBA,
            entries_crc,
        ),
        entries,
        backup_header: header(
            BACKUP_HEADER_LBA,
            PRIMARY_HEADER_LBA,
            BACKUP_ENTRIES_LBA,
            entries_crc,
        ),
    }
}

fn protective_mbr() -> [u8; 512] {
    let mut sector = [0; 512];
    let entry = &mut sector[446..462];
    // Not bootable; starting CHS 0/0/2; type 0xEE. The ending CHS is
    // 0xFFFFFF, "not representable": under the descriptor's 16-head,
    // 63-sector geometry the last sector lies past cylinder 1023.
    entry[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    entry[4] = 0xee;
    entry[5..8].copy_from_slice(&[0xff, 0xff, 0xff]);
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    entry[12..16].copy_from_slice(&((DISK_SECTORS - 1) as u32).to_le_bytes());
    sector[510] = 0x55;
    sector[511] = 0xaa;
    sector
}

fn entries() -> Vec<u8> {
    let mut entries = vec![0; ENTRIES_BYTES];
    for (partition, entry) in PARTITIONS
        .iter()
        .zip(entries.as_chunks_mut::<ENTRY_SIZE>().0)
    {
        entry[0..16].copy_from_slice(&partition.type_guid);
        entry[16..32].copy_from_slice(&partition.unique_guid);
        entry[32..40].copy_from_slice(&partition.first_lba.to_le_bytes());
        entry[40..48].copy_from_slice(&partition.last_lba.to_le_bytes());
        // The attributes at 48..56 stay zero.
        for (unit, bytes) in partition
            .name
            .encode_utf16()
            .zip(entry[56..].as_chunks_mut::<2>().0)
        {
            bytes.copy_from_slice(&unit.to_le_bytes());
        }
    }
    entries
}

fn header(my_lba: u64, alternate_lba: u64, entries_lba: u64, entries_crc: u32) -> [u8; 512] {
    let mut sector = [0; 512];
    sector[0..8].copy_from_slice(SIGNATURE);
    sector[8..12].copy_from_slice(&REVISION.to_le_bytes());
    sector[12..16].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
    sector[24..32].copy_from_slice(&my_lba.to_le_bytes());
    sector[32..40].copy_from_slice(&alternate_lba.to_le_bytes());
    sector[40..48].copy_from_slice(&FIRST_USABLE_LBA.to_le_bytes());
    sector[48..56].copy_from_slice(&LAST_USABLE_LBA.to_le_bytes());
    sector[56..72].copy_from_slice(&DISK_GUID);
    sector[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    sector[80..84].copy_from_slice(&(ENTRY_COUNT as u32).to_le_bytes());
    sector[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
    sector[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let crc = crc32(&sector[..HEADER_SIZE]);
    sector[16..20].copy_from_slice(&crc.to_le_bytes());
    sector
}

/// Checks the protective MBR, both headers and their CRCs, and the D5
/// partition entries in both copies.
pub fn check(reader: &mut ImageReader) -> Result<()> {
    check_mbr(&reader.read_sectors(0, 1)?)?;
    let mut copies = Vec::new();
    for (label, my_lba, alternate_lba, entries_lba) in [
        (
            "primary",
            PRIMARY_HEADER_LBA,
            BACKUP_HEADER_LBA,
            PRIMARY_ENTRIES_LBA,
        ),
        (
            "backup",
            BACKUP_HEADER_LBA,
            PRIMARY_HEADER_LBA,
            BACKUP_ENTRIES_LBA,
        ),
    ] {
        let sector = reader.read_sectors(my_lba, 1)?;
        let entries_crc = check_header(&sector, label, my_lba, alternate_lba, entries_lba)?;
        let entries = reader.read_sectors(entries_lba, ENTRY_SECTORS)?;
        if crc32(&entries) != entries_crc {
            return Err(Error::new(format!(
                "the {label} GPT partition entries fail their CRC32"
            )));
        }
        check_entries(&entries, label)?;
        copies.push(entries);
    }
    if copies[0] != copies[1] {
        return Err(Error::new(
            "the primary and backup GPT partition entries differ",
        ));
    }
    Ok(())
}

fn check_mbr(sector: &[u8]) -> Result<()> {
    let entry = &sector[446..462];
    if sector[510..512] != [0x55, 0xaa]
        || entry[4] != 0xee
        || u32_at(entry, 8) != 1
        || u64::from(u32_at(entry, 12)) != DISK_SECTORS - 1
        || sector[462..510].iter().any(|byte| *byte != 0)
    {
        return Err(Error::new(
            "the protective MBR is not one 0xEE entry covering the disk",
        ));
    }
    Ok(())
}

/// Checks one header and returns the CRC32 it records for its entries.
pub fn check_header(
    sector: &[u8],
    label: &str,
    my_lba: u64,
    alternate_lba: u64,
    entries_lba: u64,
) -> Result<u32> {
    let fail = |what: &str| Err(Error::new(format!("the {label} GPT header {what}")));
    if &sector[0..8] != SIGNATURE {
        return fail("lacks its signature");
    }
    let mut copy = [0; HEADER_SIZE];
    copy.copy_from_slice(&sector[..HEADER_SIZE]);
    copy[16..20].fill(0);
    if crc32(&copy) != u32_at(sector, 16) {
        return fail("fails its CRC32");
    }
    if u32_at(sector, 8) != REVISION
        || u32_at(sector, 12) != HEADER_SIZE as u32
        || u32_at(sector, 20) != 0
        || sector[HEADER_SIZE..].iter().any(|byte| *byte != 0)
    {
        return fail("has a wrong revision, size or reserved field");
    }
    if u64_at(sector, 24) != my_lba || u64_at(sector, 32) != alternate_lba {
        return fail("has wrong header locations");
    }
    if u64_at(sector, 40) != FIRST_USABLE_LBA || u64_at(sector, 48) != LAST_USABLE_LBA {
        return fail("has wrong usable LBAs");
    }
    if sector[56..72] != DISK_GUID {
        return fail("has the wrong disk GUID");
    }
    if u64_at(sector, 72) != entries_lba
        || u32_at(sector, 80) != ENTRY_COUNT as u32
        || u32_at(sector, 84) != ENTRY_SIZE as u32
    {
        return fail("has a wrong entry array location, count or size");
    }
    Ok(u32_at(sector, 88))
}

fn check_entries(entries: &[u8], label: &str) -> Result<()> {
    for (index, entry) in entries.as_chunks::<ENTRY_SIZE>().0.iter().enumerate() {
        let Some(partition) = PARTITIONS.get(index) else {
            if entry.iter().any(|byte| *byte != 0) {
                return Err(Error::new(format!(
                    "{label} GPT entry {} is not empty",
                    index + 1
                )));
            }
            continue;
        };
        let name: Vec<u16> = entry[56..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes(*bytes))
            .collect();
        let expected_name: Vec<u16> = partition.name.encode_utf16().collect();
        if entry[0..16] != partition.type_guid
            || entry[16..32] != partition.unique_guid
            || u64_at(entry, 32) != partition.first_lba
            || u64_at(entry, 40) != partition.last_lba
            || u64_at(entry, 48) != 0
            || name[..expected_name.len()] != expected_name
            || name[expected_name.len()..].iter().any(|unit| *unit != 0)
        {
            return Err(Error::new(format!(
                "{label} GPT entry {} is not partition {} of D5",
                index + 1,
                partition.name
            )));
        }
    }
    Ok(())
}

/// Encodes a GUID written as text in its on-disk order: the first three
/// groups little-endian, the last two as written.
pub const fn guid(text: &str) -> [u8; 16] {
    let text = text.as_bytes();
    assert!(text.len() == 36);
    let mut raw = [0u8; 16];
    let mut position = 0;
    let mut index = 0;
    while position < 36 {
        if position == 8 || position == 13 || position == 18 || position == 23 {
            assert!(text[position] == b'-');
            position += 1;
            continue;
        }
        raw[index] = (hex_digit(text[position]) << 4) | hex_digit(text[position + 1]);
        index += 1;
        position += 2;
    }
    [
        raw[3], raw[2], raw[1], raw[0], raw[5], raw[4], raw[7], raw[6], raw[8], raw[9], raw[10],
        raw[11], raw[12], raw[13], raw[14], raw[15],
    ]
}

const fn hex_digit(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        b'A'..=b'F' => digit - b'A' + 10,
        _ => panic!("not a hexadecimal digit"),
    }
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
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(value)
}
