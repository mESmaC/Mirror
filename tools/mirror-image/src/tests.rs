//! Spec 0015a, acceptance test 1.

use super::*;
use mirror_ext2_layout as layout;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A scratch directory under the system temporary directory, removed when
/// dropped.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "mirror-image-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Deterministic pseudo-random bytes (xorshift64*).
fn pattern(seed: u64, length: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut bytes = Vec::with_capacity(length + 8);
    while bytes.len() < length {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        bytes.extend_from_slice(&state.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    bytes.truncate(length);
    bytes
}

fn write_file(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// The staged files: build.ps1's tree with Limine's license files, plus an
/// empty file, a file of exactly three clusters and long names.
fn staged_files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("EFI/BOOT/BOOTX64.EFI", pattern(1, 376_832)),
        (
            "limine.conf",
            b"timeout: 0\n\n/Mirror\n    protocol: limine\n    kernel_path: boot():/kernel.elf\n"
                .to_vec(),
        ),
        ("kernel.elf", pattern(2, 2_141_112)),
        ("init.elf", pattern(3, 332_872)),
        ("desktop.elf", pattern(4, 664_296)),
        ("agent.elf", pattern(5, 257_368)),
        ("app.elf", pattern(6, 152_200)),
        (
            "LICENSES/limine/LICENSE",
            b"Copyright (C) the Limine authors\n".to_vec(),
        ),
        (
            "LICENSES/limine/3RDPARTY.md",
            b"# Third-party notices\n".to_vec(),
        ),
        ("empty.bin", Vec::new()),
        (
            "three-clusters.bin",
            pattern(7, 3 * esp::CLUSTER_SIZE as usize),
        ),
        (
            "A directory with a long name/a file with a rather long name.txt",
            pattern(8, 5_000),
        ),
    ]
}

fn staged_esp(scratch: &Scratch) -> PathBuf {
    let root = scratch.join("esp");
    for (relative, bytes) in staged_files() {
        write_file(&root, relative, &bytes);
    }
    fs::create_dir_all(root.join("empty-directory")).unwrap();
    root
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// The byte offset of an ESP's cluster 2 within the ESP, from its boot sector.
fn esp_data_offset(boot: &[u8]) -> u64 {
    (u64::from(u16_at(boot, 14)) + u64::from(boot[16]) * u64::from(u32_at(boot, 36))) * 512
}

fn entry_cluster(entry: &[u8]) -> u32 {
    (u32::from(u16_at(entry, 20)) << 16) | u32::from(u16_at(entry, 26))
}

/// The first clusters of the subdirectories in a directory's raw entries,
/// without `.` and `..`.
fn raw_subdirectories(entries: &[u8]) -> Vec<u32> {
    entries
        .chunks(32)
        .take_while(|entry| entry[0] != 0)
        .filter(|entry| entry[0] != 0xe5 && entry[0] != b'.' && entry[11] == 0x10)
        .map(entry_cluster)
        .collect()
}

/// The first cluster of the subdirectory with this short name.
fn raw_subdirectory(entries: &[u8], short_name: &[u8; 11]) -> u32 {
    let entry = entries
        .chunks(32)
        .find(|entry| entry[..11] == short_name[..] && entry[11] == 0x10)
        .unwrap();
    entry_cluster(entry)
}

fn files_in(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn crc32_matches_the_standard_check_value() {
    assert_eq!(crc32::crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32::crc32(b""), 0);
}

#[test]
fn guids_use_the_mixed_endian_disk_order() {
    assert_eq!(
        gpt::EFI_SYSTEM_TYPE,
        [
            0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
            0xc9, 0x3b
        ]
    );
    assert_eq!(
        gpt::LINUX_DATA_TYPE,
        [
            0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47,
            0x7d, 0xe4
        ]
    );
}

#[test]
fn gpt_headers_carry_valid_crcs_and_the_d5_boundaries() {
    let tables = gpt::tables();
    // The protective MBR: one 0xEE entry from LBA 1 to the end of the disk.
    assert_eq!(tables.mbr[510..], [0x55, 0xaa]);
    assert_eq!(tables.mbr[450], 0xee);
    assert_eq!(u32_at(&tables.mbr, 454), 1);
    assert_eq!(u32_at(&tables.mbr, 458), 2_097_151);
    assert!(tables.mbr[462..510].iter().all(|byte| *byte == 0));

    assert_eq!(tables.entries.len(), 128 * 128);
    let entries_crc = crc32::crc32(&tables.entries);
    for (header, my_lba, alternate_lba, entries_lba) in [
        (&tables.primary_header, 1, 2_097_151, 2),
        (&tables.backup_header, 2_097_151, 1, 2_097_119),
    ] {
        assert_eq!(&header[0..8], b"EFI PART");
        assert_eq!(u32_at(header, 8), 0x0001_0000);
        assert_eq!(u32_at(header, 12), 92);
        let mut zeroed = header[..92].to_vec();
        zeroed[16..20].fill(0);
        assert_eq!(u32_at(header, 16), crc32::crc32(&zeroed));
        assert_eq!(u64_at(header, 24), my_lba);
        assert_eq!(u64_at(header, 32), alternate_lba);
        assert_eq!(u64_at(header, 40), 34);
        assert_eq!(u64_at(header, 48), 2_097_118);
        assert_eq!(header[56..72], gpt::DISK_GUID);
        assert_eq!(u64_at(header, 72), entries_lba);
        assert_eq!(u32_at(header, 80), 128);
        assert_eq!(u32_at(header, 84), 128);
        assert_eq!(u32_at(header, 88), entries_crc);
        assert!(header[92..].iter().all(|byte| *byte == 0));
        assert!(gpt::check_header(header, "test", my_lba, alternate_lba, entries_lba).is_ok());
    }

    // The D5 partitions, then 126 empty entries.
    let entry = |index: usize| &tables.entries[index * 128..(index + 1) * 128];
    let name = |entry: &[u8]| {
        let units: Vec<u16> = entry[56..]
            .chunks(2)
            .map(|pair| u16_at(pair, 0))
            .take_while(|unit| *unit != 0)
            .collect();
        String::from_utf16(&units).unwrap()
    };
    let esp = entry(0);
    assert_eq!(esp[0..16], gpt::EFI_SYSTEM_TYPE);
    assert_eq!((u64_at(esp, 32), u64_at(esp, 40)), (2048, 526_335));
    assert_eq!(name(esp), "MIRROR-ESP");
    let root = entry(1);
    assert_eq!(root[0..16], gpt::LINUX_DATA_TYPE);
    assert_eq!((u64_at(root, 32), u64_at(root, 40)), (526_336, 2_095_103));
    assert_eq!(name(root), "mirror-root");
    assert_ne!(esp[16..32], root[16..32]);
    assert_eq!(u64_at(esp, 48) | u64_at(root, 48), 0);
    assert!(tables.entries[256..].iter().all(|byte| *byte == 0));
    // 256 MiB at 1 MiB, then 766 MiB, inside the usable LBAs.
    assert_eq!((526_335 - 2048 + 1) * 512, 256 << 20);
    assert_eq!((2_095_103 - 526_336 + 1) * 512, 766 << 20);

    // A changed byte fails the header CRC.
    let mut corrupt = tables.primary_header;
    corrupt[48] ^= 1;
    let error = gpt::check_header(&corrupt, "primary", 1, 2_097_151, 2).unwrap_err();
    assert!(error.to_string().contains("CRC32"), "{error}");
}

#[test]
fn esp_is_fat32_by_cluster_count_and_reads_back_every_file() {
    let scratch = Scratch::new("esp");
    let root = staged_esp(&scratch);
    let tree = esp::collect(&root).unwrap();
    let mut volume = esp::build(&tree).unwrap();
    assert_eq!(volume.len() as u64, ESP_BYTES);

    // FAT32 is decided by the cluster count the boot sector implies.
    let boot = &volume[..512];
    assert_eq!(u16_at(boot, 11), 512);
    assert_eq!(boot[13], 4);
    assert_eq!(u16_at(boot, 17), 0);
    assert_eq!(u16_at(boot, 19), 0);
    assert_eq!(u16_at(boot, 22), 0);
    assert_eq!(u64::from(u32_at(boot, 32)), ESP_BYTES / 512);
    let data_sectors =
        u32_at(boot, 32) - u32::from(u16_at(boot, 14)) - u32::from(boot[16]) * u32_at(boot, 36);
    let clusters = data_sectors / u32::from(boot[13]);
    assert!(clusters >= esp::FAT32_MIN_CLUSTERS, "{clusters} clusters");
    assert!(
        (130_000..=131_072).contains(&clusters),
        "{clusters} clusters"
    );
    assert_eq!(u32_at(boot, 67), esp::VOLUME_ID);
    assert_eq!(&boot[71..82], b"MIRROR     ");
    assert_eq!(&boot[82..90], b"FAT32   ");

    // FAT's dot entries, read raw because fatfs finds them anywhere: `.` and
    // `..` lead every subdirectory with no long-name slot before them, and
    // `..` holds 0 when the parent is the root.
    let data_offset = esp_data_offset(boot) as usize;
    let cluster = |number: u32| &volume[data_offset + (number as usize - 2) * 2048..][..2048];
    let root_cluster = u32_at(boot, 44);
    let efi = raw_subdirectory(cluster(root_cluster), b"EFI        ");
    let efi_boot = raw_subdirectory(cluster(efi), b"BOOT       ");
    assert_eq!(entry_cluster(&cluster(efi)[32..64]), 0);
    assert_eq!(entry_cluster(&cluster(efi_boot)[32..64]), efi);
    let mut pending: Vec<(u32, u32)> = raw_subdirectories(cluster(root_cluster))
        .into_iter()
        .map(|number| (number, 0))
        .collect();
    let mut subdirectories = 0;
    while let Some((number, parent)) = pending.pop() {
        let entries = cluster(number);
        assert_eq!(&entries[..12], b".          \x10");
        assert_eq!(entry_cluster(&entries[..32]), number);
        assert_eq!(&entries[32..44], b"..         \x10");
        assert_eq!(entry_cluster(&entries[32..64]), parent);
        pending.extend(
            raw_subdirectories(entries)
                .into_iter()
                .map(|child| (child, number)),
        );
        subdirectories += 1;
    }
    // EFI, EFI/BOOT, LICENSES, LICENSES/limine, the long name and the empty one.
    assert_eq!(subdirectories, 6);

    let filesystem =
        fatfs::FileSystem::new(Cursor::new(volume.as_mut_slice()), fatfs::FsOptions::new())
            .unwrap();
    assert_eq!(filesystem.fat_type(), fatfs::FatType::Fat32);
    let stats = filesystem.stats().unwrap();
    assert_eq!(stats.total_clusters(), clusters);
    assert_eq!(stats.cluster_size(), 2048);
    assert_eq!(filesystem.volume_label(), "MIRROR");
    assert_eq!(
        filesystem
            .read_volume_label_from_root_dir()
            .unwrap()
            .as_deref(),
        Some("MIRROR")
    );
    let root_dir = filesystem.root_dir();
    for (relative, bytes) in staged_files() {
        let mut file = root_dir.open_file(relative).unwrap();
        let mut stored = Vec::new();
        file.read_to_end(&mut stored).unwrap();
        assert!(stored == bytes, "/{relative} differs");
    }
    assert!(root_dir.open_dir("empty-directory").is_ok());

    // Sorted names and the DOS epoch, in every directory.
    let epoch = fatfs::Date {
        year: 1980,
        month: 1,
        day: 1,
    };
    let midnight = fatfs::Time {
        hour: 0,
        min: 0,
        sec: 0,
        millis: 0,
    };
    let mut directories = vec![(String::new(), root_dir.clone())];
    while let Some((path, directory)) = directories.pop() {
        let mut names = Vec::new();
        for entry in directory.iter() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            assert_eq!(
                entry.modified(),
                fatfs::DateTime {
                    date: epoch,
                    time: midnight
                }
            );
            assert_eq!(
                entry.created(),
                fatfs::DateTime {
                    date: epoch,
                    time: midnight
                }
            );
            assert_eq!(entry.accessed(), epoch);
            if name == "." || name == ".." {
                continue;
            }
            if entry.is_dir() {
                directories.push((format!("{path}/{name}"), entry.to_dir()));
            }
            names.push(name);
        }
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "{path}/ is not in sorted order");
    }
}

/// Reads partition bytes from the formatter's regions; the rest is zero.
fn read_regions(regions: &[ext2::Region], offset: u64, length: usize) -> Vec<u8> {
    let mut bytes = vec![0; length];
    for region in regions {
        let start = region.offset.max(offset);
        let end = (region.offset + region.bytes.len() as u64).min(offset + length as u64);
        if start < end {
            bytes[(start - offset) as usize..(end - offset) as usize].copy_from_slice(
                &region.bytes[(start - region.offset) as usize..(end - region.offset) as usize],
            );
        }
    }
    bytes
}

#[test]
fn ext2_superblock_descriptors_bitmaps_and_free_counts_agree() {
    let regions = ext2::format().unwrap();
    for pair in regions.windows(2) {
        assert!(pair[0].offset + pair[0].bytes.len() as u64 <= pair[1].offset);
    }
    let last = regions.last().unwrap();
    assert!(last.offset + last.bytes.len() as u64 <= ROOT_BYTES);
    let read = |offset: u64, length: usize| read_regions(&regions, offset, length);
    let block = |number: u32| read(u64::from(number) * 4096, 4096);

    let superblock = layout::Superblock::decode(&read(1024, 1024)).unwrap();
    let geometry = layout::Geometry::from_superblock(&superblock).unwrap();
    assert_eq!(geometry.block_size(), 4096);
    assert_eq!(geometry.blocks_count(), 196_096);
    assert_eq!(geometry.group_count(), 6);
    assert_eq!(
        (geometry.inodes_per_group(), geometry.inode_size()),
        (8_192, 256)
    );
    assert_eq!(superblock.revision, layout::DYNAMIC_REVISION);
    assert_eq!(
        (
            superblock.feature_compat,
            superblock.feature_incompat,
            superblock.feature_ro_compat
        ),
        (0, 0, 0)
    );
    assert_eq!(superblock.state, layout::STATE_CLEAN);
    assert_eq!(&superblock.volume_name, b"mirror-root\0\0\0\0\0");
    assert_eq!(superblock.first_inode, 11);
    assert_eq!(superblock.write_time, 315_532_800);

    let descriptor_table = block(1);
    let (mut free_blocks, mut free_inodes, mut directories) = (0, 0, 0);
    for group in 0..6u32 {
        let group_layout = geometry.group_layout(group).unwrap();
        // A superblock and descriptor-table copy in every group.
        let copy =
            layout::Superblock::decode(&read(geometry.superblock_offset(group).unwrap(), 1024))
                .unwrap();
        assert_eq!(
            copy,
            layout::Superblock {
                block_group: group as u16,
                ..superblock
            }
        );
        assert_eq!(block(group_layout.descriptor_table), descriptor_table);
        let descriptor =
            layout::GroupDescriptor::decode(&descriptor_table[group as usize * 32..]).unwrap();
        assert_eq!(descriptor.block_bitmap, group_layout.block_bitmap);
        assert_eq!(descriptor.inode_bitmap, group_layout.inode_bitmap);
        assert_eq!(descriptor.inode_table, group_layout.inode_table);

        let block_bitmap = block(group_layout.block_bitmap);
        let inode_bitmap = block(group_layout.inode_bitmap);
        let used_blocks = ext2::count_set(&block_bitmap, group_layout.block_count);
        let used_inodes = ext2::count_set(&inode_bitmap, 8_192);
        assert_eq!(
            u32::from(descriptor.free_blocks_count),
            group_layout.block_count - used_blocks
        );
        assert_eq!(u32::from(descriptor.free_inodes_count), 8_192 - used_inodes);
        // The metadata blocks lead each group, then group 0's three data blocks.
        let expected_used = 516 + if group == 0 { 3 } else { 0 };
        assert_eq!(used_blocks, expected_used);
        for index in 0..group_layout.block_count {
            assert_eq!(
                layout::bitmap_get(&block_bitmap, index),
                Ok(index < expected_used)
            );
        }
        // Padding: bits past a short last group, and bytes past 8,192 inodes.
        assert_eq!(
            ext2::count_set(&block_bitmap, 32_768) - used_blocks,
            32_768 - group_layout.block_count
        );
        assert!(inode_bitmap[1024..].iter().all(|byte| *byte == 0xff));
        assert_eq!(used_inodes, if group == 0 { 12 } else { 0 });
        free_blocks += u32::from(descriptor.free_blocks_count);
        free_inodes += u32::from(descriptor.free_inodes_count);
        directories += descriptor.used_directories_count;
    }
    assert_eq!(geometry.blocks_in_group(5), Ok(32_256));
    assert_eq!(
        (free_blocks, superblock.free_blocks_count),
        (192_997, 192_997)
    );
    assert_eq!(
        (free_inodes, superblock.free_inodes_count),
        (49_140, 49_140)
    );
    assert_eq!(directories, 2);

    let inode = |number: u32| {
        let position = geometry.inode_position(number).unwrap();
        let table = geometry.group_layout(position.group).unwrap().inode_table;
        let record = read(u64::from(table) * 4096 + position.table_offset, 256);
        assert!(record[128..].iter().all(|byte| *byte == 0));
        layout::Inode::decode(&record).unwrap()
    };
    let listing = |inode: &layout::Inode| -> Vec<(Vec<u8>, u32)> {
        let bytes = block(inode.block[0]);
        layout::directory_entries(&bytes)
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.inode != 0)
            .map(|entry| (entry.name.to_vec(), entry.inode))
            .collect()
    };
    let root = inode(layout::ROOT_INODE);
    assert!(root.is_directory());
    assert_eq!(root.links_count, 3);
    assert_eq!(
        listing(&root),
        [
            (b".".to_vec(), 2),
            (b"..".to_vec(), 2),
            (b"lost+found".to_vec(), 11),
            (b"mirror-root".to_vec(), 12)
        ]
    );
    let lost_and_found = inode(11);
    assert!(lost_and_found.is_directory());
    assert_eq!(lost_and_found.links_count, 2);
    assert_eq!(
        listing(&lost_and_found),
        [(b".".to_vec(), 11), (b"..".to_vec(), 2)]
    );
    let marker = inode(12);
    assert!(marker.is_regular_file());
    assert_eq!(
        (marker.links_count, marker.size, marker.sectors),
        (1, 14, 8)
    );
    assert_eq!(&block(marker.block[0])[..14], b"mirror-root 1\n");
    for number in (1..=10).filter(|number| *number != layout::ROOT_INODE) {
        assert_eq!(inode(number), layout::Inode::default());
    }
}

#[test]
fn vmdk_descriptor_has_the_d9_header_and_one_flat_extent() {
    let descriptor = vmdk::descriptor("mirror.img", false).unwrap();
    let lines: Vec<&str> = descriptor.lines().collect();
    assert_eq!(
        lines[..5],
        [
            "# Disk DescriptorFile",
            "version=1",
            "CID=4d495252",
            "parentCID=ffffffff",
            "createType=\"monolithicFlat\""
        ]
    );
    let cid = lines[2].strip_prefix("CID=").unwrap();
    assert!(cid.len() == 8 && cid.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let extents: Vec<&&str> = lines
        .iter()
        .filter(|line| {
            ["RW ", "RDONLY ", "NOACCESS "]
                .iter()
                .any(|access| line.starts_with(access))
        })
        .collect();
    assert_eq!(extents, [&"RW 2097152 FLAT \"mirror.img\" 0"]);
    assert_eq!(DISK_BYTES / 512, 2_097_152);
    assert!(lines.contains(&"#DDB"));
    assert!(lines.contains(&"ddb.virtualHWVersion = \"4\""));
    assert!(lines.contains(&"ddb.geometry.cylinders = \"2080\""));
    for forbidden in [
        "ddb.uuid",
        "ddb.iofilters",
        "ddb.sidecars",
        "ddb.longContentID",
    ] {
        assert!(!descriptor.contains(forbidden), "{forbidden}");
    }
    assert!(descriptor.ends_with('\n'));
    assert!(vmdk::descriptor("mirror\".img", false).is_err());
    assert!(vmdk::descriptor("", false).is_err());
}

#[test]
fn vmdk_validation_checks_both_identities_and_the_complete_schema() {
    for perf in [false, true] {
        let descriptor = vmdk::descriptor("mirror.img", perf).unwrap();
        vmdk::validate(&descriptor, "mirror.img", perf).unwrap();
        assert!(vmdk::validate(&descriptor, "mirror.img", !perf).is_err());
        assert!(vmdk::validate(&descriptor, "different.img", perf).is_err());
        // Every required record is checked, not merely CID presence.
        for line in descriptor
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let removed = descriptor.replace(&format!("{line}\n"), "");
            assert!(
                vmdk::validate(&removed, "mirror.img", perf).is_err(),
                "missing {line}"
            );
            let duplicate = format!("{descriptor}{line}\n");
            assert!(
                vmdk::validate(&duplicate, "mirror.img", perf).is_err(),
                "duplicate {line}"
            );
            let changed = descriptor.replace(line, &format!("{line}1"));
            assert!(
                vmdk::validate(&changed, "mirror.img", perf).is_err(),
                "changed {line}"
            );
        }
        for extra in [
            "ddb.uuid.image = \"unexpected\"",
            "ddb.iofilters = \"unexpected\"",
            "ddb.sidecars = \"unexpected\"",
            "unknown=1",
            "RDONLY 2097152 FLAT \"mirror.img\" 0",
            "\u{e9}",
        ] {
            assert!(
                vmdk::validate(&format!("{descriptor}{extra}\n"), "mirror.img", perf).is_err(),
                "{extra}"
            );
        }
    }
    let equals_name = vmdk::descriptor("mirror=perf.img", true).unwrap();
    vmdk::validate(&equals_name, "mirror=perf.img", true).unwrap();
    let daily = vmdk::descriptor("mirror.img", false).unwrap();
    let perf = vmdk::descriptor("mirror.img", true).unwrap();
    assert_eq!(daily.replace("CID=4d495252", "CID=50455246"), perf);
}

fn fill(file: &mut fs::File, buffer: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]).unwrap() {
            0 => break,
            count => filled += count,
        }
    }
    filled
}

fn same_files(first: &Path, second: &Path) -> bool {
    let (mut first, mut second) = (
        fs::File::open(first).unwrap(),
        fs::File::open(second).unwrap(),
    );
    let (mut first_chunk, mut second_chunk) = (vec![0; 1 << 20], vec![0; 1 << 20]);
    loop {
        let first_length = fill(&mut first, &mut first_chunk);
        let second_length = fill(&mut second, &mut second_chunk);
        if first_chunk[..first_length] != second_chunk[..second_length] {
            return false;
        }
        if first_length == 0 {
            return true;
        }
    }
}

fn patch(path: &Path, offset: u64, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn disk_images_are_byte_identical_and_verify() {
    let scratch = Scratch::new("reproducible");
    let esp = staged_esp(&scratch);
    let mut outputs = Vec::new();
    for run in ["first", "second"] {
        let directory = scratch.join(run);
        fs::create_dir_all(&directory).unwrap();
        let (image, vmdk) = (directory.join("mirror.img"), directory.join("mirror.vmdk"));
        let summary = write_disk(&esp, &image, &vmdk).unwrap();
        assert_eq!(summary.esp_files, staged_files().len());
        assert_eq!(files_in(&directory), ["mirror.img", "mirror.vmdk"]);
        assert_eq!(fs::metadata(&image).unwrap().len(), DISK_BYTES);
        let verified = verify(&esp, &image).unwrap();
        assert_eq!(verified.esp_files, staged_files().len());
        assert_eq!(verified.root_groups, 6);
        assert!(verified.esp_clusters >= esp::FAT32_MIN_CLUSTERS);
        outputs.push((image, vmdk));
    }
    let [(first_image, first_vmdk), (second_image, second_vmdk)] = &outputs[..] else {
        unreachable!();
    };
    assert!(same_files(first_image, second_image), "the images differ");
    let descriptor = fs::read_to_string(first_vmdk).unwrap();
    assert_eq!(descriptor, fs::read_to_string(second_vmdk).unwrap());
    for line in ["version=1", "CID=4d495252", "parentCID=ffffffff"] {
        assert!(
            descriptor.lines().any(|candidate| candidate == line),
            "{line}"
        );
    }
    let extent = format!(
        "RW {} FLAT \"mirror.img\" 0",
        fs::metadata(first_image).unwrap().len() / 512
    );
    assert!(descriptor.lines().any(|line| line == extent));
    assert!(!descriptor.contains("ddb.uuid.image"));

    // verify catches a changed staged file, ESP file, GPT header, `..` entry
    // and marker.
    let kernel = esp.join("kernel.elf");
    let original = fs::read(&kernel).unwrap();
    let mut changed = original.clone();
    changed[1000] ^= 1;
    fs::write(&kernel, &changed).unwrap();
    let error = verify(&esp, second_image).unwrap_err().to_string();
    assert!(error.contains("/kernel.elf in the ESP differs"), "{error}");
    fs::write(&kernel, &original).unwrap();
    patch(second_image, SECTOR_SIZE + 40, &[0xff]);
    let error = verify(&esp, second_image).unwrap_err().to_string();
    assert!(
        error.contains("primary GPT header fails its CRC32"),
        "{error}"
    );
    patch(second_image, SECTOR_SIZE + 40, &[34]);
    verify(&esp, second_image).unwrap();
    // A top-level `..` holding the root's cluster, as fatfs writes it.
    let mut reader = ImageReader::open(second_image).unwrap();
    let boot = reader.read_at(ESP_OFFSET, 512).unwrap();
    let cluster_offset =
        |number: u32| ESP_OFFSET + esp_data_offset(&boot) + u64::from(number - 2) * 2048;
    let root_cluster = u32_at(&boot, 44);
    let root_entries = reader.read_at(cluster_offset(root_cluster), 2048).unwrap();
    let dot_dot = cluster_offset(raw_subdirectory(&root_entries, b"EFI        ")) + 32;
    drop(reader);
    patch(second_image, dot_dot + 26, &root_cluster.to_le_bytes()[..2]);
    let error = verify(&esp, second_image).unwrap_err().to_string();
    assert!(
        error.contains("/EFI does not start with `.` and `..`"),
        "{error}"
    );
    patch(second_image, dot_dot + 26, &[0, 0]);
    verify(&esp, second_image).unwrap();
    let marker = ROOT_OFFSET + (516 + 2) * 4096;
    patch(second_image, marker, b"M");
    let error = verify(&esp, second_image).unwrap_err().to_string();
    assert!(error.contains("marker"), "{error}");
}

#[test]
fn perf_disk_writer_uses_the_separate_descriptor_identity() {
    let scratch = Scratch::new("perf-disk");
    let esp = staged_esp(&scratch);
    let image = scratch.join("mirror.img");
    let vmdk = scratch.join("mirror.vmdk");
    write_disk_with_perf(&esp, &image, &vmdk, true).unwrap();
    verify(&esp, &image).unwrap();
    let descriptor = fs::read_to_string(vmdk).unwrap();
    vmdk::validate(&descriptor, "mirror.img", true).unwrap();
    assert!(vmdk::validate(&descriptor, "mirror.img", false).is_err());
}

#[test]
fn an_esp_tree_over_256_mib_fails_and_leaves_no_output() {
    let scratch = Scratch::new("oversized");
    let esp = staged_esp(&scratch);
    // Sizing the file allocates no data, so this is quick.
    let big = fs::File::create(esp.join("big.bin")).unwrap();
    big.set_len(ESP_BYTES + 1).unwrap();
    drop(big);
    let directory = scratch.join("out");
    fs::create_dir_all(&directory).unwrap();
    let error = write_disk(
        &esp,
        &directory.join("mirror.img"),
        &directory.join("mirror.vmdk"),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("more than the 268435456-byte ESP partition"),
        "{error}"
    );
    assert!(files_in(&directory).is_empty());
}

#[test]
fn esp_content_beyond_the_free_clusters_fails_and_leaves_no_output() {
    let scratch = Scratch::new("full");
    let esp = staged_esp(&scratch);
    // One byte under 256 MiB of files, which passes the size check but needs
    // more clusters than the FATs and directories leave.
    let staged = esp::collect(&esp).unwrap().content_bytes();
    let big = fs::File::create(esp.join("big.bin")).unwrap();
    big.set_len(ESP_BYTES - staged - 1).unwrap();
    drop(big);
    let directory = scratch.join("out");
    fs::create_dir_all(&directory).unwrap();
    let error = write_disk(
        &esp,
        &directory.join("mirror.img"),
        &directory.join("mirror.vmdk"),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("No space left"), "{error}");
    assert!(files_in(&directory).is_empty());
}

#[test]
fn the_staged_tree_must_carry_limines_licenses() {
    let scratch = Scratch::new("licenses");
    let esp = staged_esp(&scratch);
    fs::remove_file(esp.join("LICENSES/limine/3RDPARTY.md")).unwrap();
    let error = esp::collect(&esp).unwrap_err().to_string();
    assert!(
        error.contains("lacks /LICENSES/limine/3RDPARTY.md"),
        "{error}"
    );
    let missing = esp::collect(&scratch.join("missing"))
        .unwrap_err()
        .to_string();
    assert!(missing.contains("cannot read"), "{missing}");
}

#[test]
fn the_descriptor_must_sit_beside_the_image() {
    let scratch = Scratch::new("directories");
    let esp = staged_esp(&scratch);
    let (first, second) = (scratch.join("first"), scratch.join("second"));
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let error = write_disk(&esp, &first.join("mirror.img"), &second.join("mirror.vmdk"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("same directory"), "{error}");
    let error = write_disk(&esp, &first.join("mirror.img"), &first.join("mirror.img"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("different names"), "{error}");
    assert!(files_in(&first).is_empty() && files_in(&second).is_empty());
}
