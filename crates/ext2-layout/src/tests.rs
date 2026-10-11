use super::*;

fn sample_superblock() -> Superblock {
    // Distinct values in every field, so a swapped offset shows up.
    let mut uuid = [0; 16];
    let mut volume_name = [0; 16];
    let mut last_mounted = [0; 64];
    for (index, byte) in uuid.iter_mut().enumerate() {
        *byte = 0xa0 + index as u8;
    }
    volume_name[..11].copy_from_slice(b"mirror-root");
    for (index, byte) in last_mounted.iter_mut().enumerate() {
        *byte = 0x40 + index as u8;
    }
    Superblock {
        inodes_count: 0x0101_0101,
        blocks_count: 0x0202_0202,
        reserved_blocks_count: 0x0303_0303,
        free_blocks_count: 0x0404_0404,
        free_inodes_count: 0x0505_0505,
        first_data_block: 0x0606_0606,
        log_block_size: 0x0707_0707,
        log_fragment_size: 0x0808_0808,
        blocks_per_group: 0x0909_0909,
        fragments_per_group: 0x0a0a_0a0a,
        inodes_per_group: 0x0b0b_0b0b,
        mount_time: 0x0c0c_0c0c,
        write_time: 0x0d0d_0d0d,
        mount_count: 0x0e0e,
        max_mount_count: 0x0f0f,
        state: 0x1010,
        errors: 0x1111,
        minor_revision: 0x1212,
        last_check_time: 0x1313_1313,
        check_interval: 0x1414_1414,
        creator_os: 0x1515_1515,
        revision: DYNAMIC_REVISION,
        default_reserved_uid: 0x1616,
        default_reserved_gid: 0x1717,
        first_inode: 0x1818_1818,
        inode_size: 0x1919,
        block_group: 0x1a1a,
        feature_compat: 0x1b1b_1b1b,
        feature_incompat: 0x1c1c_1c1c,
        feature_ro_compat: 0x1d1d_1d1d,
        uuid,
        volume_name,
        last_mounted,
        mkfs_time: 0x1e1e_1e1e,
    }
}

/// The D6 root: 4 KiB blocks, 196,096 blocks in 6 groups of 32,768, and
/// 8,192 inodes of 256 bytes per group.
fn d6_geometry() -> Geometry {
    Geometry::new(4096, 196_096, 32_768, 8_192, 256).unwrap()
}

fn d6_superblock() -> Superblock {
    let geometry = d6_geometry();
    Superblock {
        inodes_count: geometry.inodes_count(),
        blocks_count: geometry.blocks_count(),
        reserved_blocks_count: 0,
        free_blocks_count: 0,
        free_inodes_count: 0,
        first_data_block: geometry.first_data_block(),
        log_block_size: geometry.log_block_size(),
        log_fragment_size: geometry.log_block_size(),
        blocks_per_group: geometry.blocks_per_group(),
        fragments_per_group: geometry.blocks_per_group(),
        inodes_per_group: geometry.inodes_per_group(),
        mount_time: 0,
        write_time: 0,
        mount_count: 0,
        max_mount_count: 0,
        state: STATE_CLEAN,
        errors: ERRORS_CONTINUE,
        minor_revision: 0,
        last_check_time: 0,
        check_interval: 0,
        creator_os: CREATOR_OS_LINUX,
        revision: DYNAMIC_REVISION,
        default_reserved_uid: 0,
        default_reserved_gid: 0,
        first_inode: GOOD_OLD_FIRST_INODE,
        inode_size: geometry.inode_size(),
        block_group: 0,
        feature_compat: 0,
        feature_incompat: 0,
        feature_ro_compat: 0,
        uuid: [0; 16],
        volume_name: [0; 16],
        last_mounted: [0; 64],
        mkfs_time: 0,
    }
}

#[test]
fn superblock_round_trips_at_its_offsets() {
    let superblock = sample_superblock();
    let bytes = superblock.encode();
    assert_eq!(&bytes[0..4], &0x0101_0101u32.to_le_bytes());
    assert_eq!(&bytes[24..28], &0x0707_0707u32.to_le_bytes());
    assert_eq!(&bytes[56..58], &[0x53, 0xef]);
    assert_eq!(&bytes[58..60], &0x1010u16.to_le_bytes());
    assert_eq!(&bytes[76..80], &1u32.to_le_bytes());
    assert_eq!(&bytes[84..88], &0x1818_1818u32.to_le_bytes());
    assert_eq!(&bytes[88..90], &0x1919u16.to_le_bytes());
    assert_eq!(&bytes[90..92], &0x1a1au16.to_le_bytes());
    assert_eq!(&bytes[100..104], &0x1d1d_1d1du32.to_le_bytes());
    assert_eq!(bytes[104], 0xa0);
    assert_eq!(&bytes[120..131], b"mirror-root");
    assert_eq!(bytes[136], 0x40);
    assert_eq!(&bytes[264..268], &0x1e1e_1e1eu32.to_le_bytes());
    // Nothing outside the encoded fields.
    assert!(bytes[200..264].iter().all(|byte| *byte == 0));
    assert!(bytes[268..].iter().all(|byte| *byte == 0));
    assert_eq!(Superblock::decode(&bytes), Ok(superblock));
}

#[test]
fn superblock_decode_rejects_bad_records() {
    let bytes = sample_superblock().encode();
    assert_eq!(
        Superblock::decode(&bytes[..SUPERBLOCK_SIZE - 1]),
        Err(Error::Truncated)
    );
    let mut bad_magic = bytes;
    bad_magic[56] = 0x54;
    assert_eq!(Superblock::decode(&bad_magic), Err(Error::Magic));
    let mut bad_revision = bytes;
    bad_revision[76] = 2;
    assert_eq!(Superblock::decode(&bad_revision), Err(Error::Revision));
}

#[test]
fn superblock_block_and_inode_sizes() {
    let mut superblock = d6_superblock();
    assert_eq!(superblock.block_size(), Ok(4096));
    assert_eq!(superblock.inode_record_size(), 256);
    superblock.log_block_size = 6;
    assert_eq!(superblock.block_size(), Ok(65536));
    superblock.log_block_size = 7;
    assert_eq!(superblock.block_size(), Err(Error::BlockSize));
    superblock.revision = GOOD_OLD_REVISION;
    assert_eq!(superblock.inode_record_size(), GOOD_OLD_INODE_SIZE);
}

#[test]
fn group_descriptor_round_trips_at_its_offsets() {
    let descriptor = GroupDescriptor {
        block_bitmap: 0x0102_0304,
        inode_bitmap: 0x0506_0708,
        inode_table: 0x090a_0b0c,
        free_blocks_count: 0x0d0e,
        free_inodes_count: 0x0f10,
        used_directories_count: 0x1112,
        flags: 0x1314,
    };
    let bytes = descriptor.encode();
    assert_eq!(&bytes[0..4], &[0x04, 0x03, 0x02, 0x01]);
    assert_eq!(&bytes[8..12], &[0x0c, 0x0b, 0x0a, 0x09]);
    assert_eq!(&bytes[12..14], &[0x0e, 0x0d]);
    assert_eq!(&bytes[16..18], &[0x12, 0x11]);
    assert!(bytes[20..].iter().all(|byte| *byte == 0));
    assert_eq!(GroupDescriptor::decode(&bytes), Ok(descriptor));
    assert_eq!(GroupDescriptor::decode(&bytes[..31]), Err(Error::Truncated));
}

#[test]
fn inode_round_trips_at_its_offsets() {
    let mut block = [0; BLOCK_POINTERS];
    for (index, pointer) in block.iter_mut().enumerate() {
        *pointer = 0x1000 + index as u32;
    }
    let inode = Inode {
        mode: MODE_DIRECTORY | 0o755,
        uid: 0x0102,
        size: 0x0304_0506,
        access_time: 0x0708_090a,
        change_time: 0x0b0c_0d0e,
        modification_time: 0x0f10_1112,
        deletion_time: 0x1314_1516,
        gid: 0x1718,
        links_count: 0x191a,
        sectors: 0x1b1c_1d1e,
        flags: 0x1f20_2122,
        block,
        generation: 0x2324_2526,
        file_acl: 0x2728_292a,
        size_high: 0x2b2c_2d2e,
    };
    let bytes = inode.encode();
    assert_eq!(&bytes[0..2], &(0x4000u16 | 0o755).to_le_bytes());
    assert_eq!(&bytes[4..8], &0x0304_0506u32.to_le_bytes());
    assert_eq!(&bytes[26..28], &0x191au16.to_le_bytes());
    assert_eq!(&bytes[28..32], &0x1b1c_1d1eu32.to_le_bytes());
    assert_eq!(&bytes[40..44], &0x1000u32.to_le_bytes());
    assert_eq!(&bytes[96..100], &0x100eu32.to_le_bytes());
    assert_eq!(&bytes[108..112], &0x2b2c_2d2eu32.to_le_bytes());
    assert!(bytes[112..].iter().all(|byte| *byte == 0));
    let decoded = Inode::decode(&bytes).unwrap();
    assert_eq!(decoded, inode);
    assert!(decoded.is_directory());
    assert!(!decoded.is_regular_file());
    assert_eq!(Inode::decode(&bytes[..127]), Err(Error::Truncated));
}

#[test]
fn directory_entries_round_trip_and_fill_a_block() {
    let mut block = [0xffu8; 64];
    let names: [(&[u8], u32); 3] = [(b".", 2), (b"..", 2), (b"lost+found", 11)];
    let mut offset = 0;
    for (index, (name, inode)) in names.iter().enumerate() {
        let record_length = if index + 1 == names.len() {
            block.len() - offset
        } else {
            DirectoryEntry::record_length_for(name.len())
        };
        let entry = DirectoryEntry {
            inode: *inode,
            record_length: record_length as u16,
            file_type: FILE_TYPE_UNKNOWN,
            name,
        };
        entry.encode(&mut block[offset..]).unwrap();
        offset += record_length;
    }
    assert_eq!(&block[..12], &[2, 0, 0, 0, 12, 0, 1, 0, b'.', 0, 0, 0]);
    assert_eq!(&block[12..24], &[2, 0, 0, 0, 12, 0, 2, 0, b'.', b'.', 0, 0]);
    assert_eq!(&block[24..28], &[11, 0, 0, 0]);
    assert_eq!(&block[28..30], &40u16.to_le_bytes());
    // The last record's padding is zeroed out to the end of the block.
    assert!(block[42..].iter().all(|byte| *byte == 0));
    let mut count = 0;
    for (entry, (name, inode)) in directory_entries(&block).zip(names.iter()) {
        let entry = entry.unwrap();
        assert_eq!((entry.name, entry.inode), (*name, *inode));
        count += 1;
    }
    assert_eq!(count, names.len());
    assert_eq!(directory_entries(&block).count(), names.len());
}

#[test]
fn directory_entries_reject_malformed_records() {
    assert_eq!(DirectoryEntry::record_length_for(1), 12);
    assert_eq!(DirectoryEntry::record_length_for(4), 12);
    assert_eq!(DirectoryEntry::record_length_for(5), 16);
    assert_eq!(DirectoryEntry::record_length_for(11), 20);
    let mut block = [0u8; 16];
    // A record length that is not a multiple of four.
    block[..8].copy_from_slice(&[2, 0, 0, 0, 14, 0, 1, 0]);
    assert_eq!(DirectoryEntry::decode(&block), Err(Error::RecordLength));
    // A record too short for its name.
    block[..8].copy_from_slice(&[2, 0, 0, 0, 12, 0, 5, 0]);
    assert_eq!(DirectoryEntry::decode(&block), Err(Error::RecordLength));
    // A record that runs past the block.
    block[..8].copy_from_slice(&[2, 0, 0, 0, 20, 0, 1, 0]);
    assert_eq!(DirectoryEntry::decode(&block), Err(Error::RecordLength));
    // A used record with no name.
    block[..8].copy_from_slice(&[2, 0, 0, 0, 16, 0, 0, 0]);
    assert_eq!(DirectoryEntry::decode(&block), Err(Error::NameLength));
    // The iterator yields the error once, then stops.
    let mut entries = directory_entries(&block);
    assert_eq!(entries.next(), Some(Err(Error::NameLength)));
    assert_eq!(entries.next(), None);
    // An unused record with no name is valid.
    block[..8].copy_from_slice(&[0, 0, 0, 0, 16, 0, 0, 0]);
    assert_eq!(DirectoryEntry::decode(&block).unwrap().inode, 0);
    let entry = DirectoryEntry {
        inode: 2,
        record_length: 8,
        file_type: 0,
        name: b"abc",
    };
    assert_eq!(entry.encode(&mut block), Err(Error::RecordLength));
    let entry = DirectoryEntry {
        inode: 2,
        record_length: 12,
        file_type: 0,
        name: b"",
    };
    assert_eq!(entry.encode(&mut block), Err(Error::NameLength));
    let entry = DirectoryEntry {
        inode: 2,
        record_length: 20,
        file_type: 0,
        name: b"abc",
    };
    assert_eq!(entry.encode(&mut block), Err(Error::Truncated));
}

#[test]
fn d6_geometry_matches_the_spec() {
    // 0015-D5: the root partition is 766 MiB.
    assert_eq!(766u64 * 1024 * 1024 / 4096, 196_096);
    let geometry = d6_geometry();
    assert_eq!(geometry.group_count(), 6);
    assert_eq!(geometry.inodes_count(), 49_152);
    assert_eq!(geometry.first_data_block(), 0);
    assert_eq!(geometry.log_block_size(), 2);
    assert_eq!(geometry.descriptor_table_blocks(), 1);
    assert_eq!(geometry.inode_table_blocks(), 512);
    assert_eq!(geometry.overhead_blocks(), 516);
    for group in 0..5 {
        assert_eq!(geometry.blocks_in_group(group), Ok(32_768));
    }
    assert_eq!(geometry.blocks_in_group(5), Ok(32_256));
    assert_eq!(geometry.blocks_in_group(6), Err(Error::Group));
    assert_eq!(
        geometry.group_layout(0),
        Ok(GroupLayout {
            first_block: 0,
            block_count: 32_768,
            superblock: 0,
            descriptor_table: 1,
            block_bitmap: 2,
            inode_bitmap: 3,
            inode_table: 4,
            data_start: 516,
        })
    );
    assert_eq!(
        geometry.group_layout(5),
        Ok(GroupLayout {
            first_block: 163_840,
            block_count: 32_256,
            superblock: 163_840,
            descriptor_table: 163_841,
            block_bitmap: 163_842,
            inode_bitmap: 163_843,
            inode_table: 163_844,
            data_start: 164_356,
        })
    );
    assert_eq!(geometry.superblock_offset(0), Ok(1024));
    assert_eq!(geometry.superblock_offset(1), Ok(32_768 * 4096));
    assert_eq!(
        geometry.inode_position(ROOT_INODE),
        Ok(InodePosition {
            group: 0,
            index: 1,
            table_offset: 256
        })
    );
    assert_eq!(
        geometry.inode_position(8_193),
        Ok(InodePosition {
            group: 1,
            index: 0,
            table_offset: 0
        })
    );
    assert_eq!(
        geometry.inode_position(49_152),
        Ok(InodePosition {
            group: 5,
            index: 8_191,
            table_offset: 8_191 * 256
        })
    );
    assert_eq!(geometry.inode_position(0), Err(Error::InodeNumber));
    assert_eq!(geometry.inode_position(49_153), Err(Error::InodeNumber));
}

#[test]
fn one_kib_blocks_start_at_block_one() {
    let geometry = Geometry::new(1024, 20_000, 8_192, 2_048, 128).unwrap();
    assert_eq!(geometry.first_data_block(), 1);
    assert_eq!(geometry.group_count(), 3);
    assert_eq!(geometry.blocks_in_group(2), Ok(20_000 - 1 - 2 * 8_192));
    assert_eq!(geometry.superblock_offset(0), Ok(1024));
    assert_eq!(geometry.group_layout(0).unwrap().superblock, 1);
    assert_eq!(geometry.group_layout(1).unwrap().first_block, 8_193);
    assert_eq!(geometry.superblock_offset(1), Ok(8_193 * 1024));
}

#[test]
fn geometry_rejects_impossible_layouts() {
    assert_eq!(
        Geometry::new(3000, 196_096, 32_768, 8_192, 256),
        Err(Error::BlockSize)
    );
    assert_eq!(
        Geometry::new(512, 196_096, 4_096, 1_024, 128),
        Err(Error::BlockSize)
    );
    assert_eq!(
        Geometry::new(131_072, 196_096, 32_768, 8_192, 256),
        Err(Error::BlockSize)
    );
    assert_eq!(
        Geometry::new(4096, 196_096, 32_768, 8_192, 192),
        Err(Error::InodeSize)
    );
    assert_eq!(
        Geometry::new(4096, 196_096, 32_768, 8_192, 64),
        Err(Error::InodeSize)
    );
    assert_eq!(
        Geometry::new(1024, 196_096, 8_192, 2_048, 2048),
        Err(Error::InodeSize)
    );
    // More blocks or inodes per group than one bitmap block holds.
    assert_eq!(
        Geometry::new(4096, 196_096, 32_776, 8_192, 256),
        Err(Error::Geometry)
    );
    assert_eq!(
        Geometry::new(4096, 196_096, 32_768, 32_776, 256),
        Err(Error::Geometry)
    );
    // Counts that are not multiples of eight, or partial inode-table blocks.
    assert_eq!(
        Geometry::new(4096, 196_096, 32_764, 8_192, 256),
        Err(Error::Geometry)
    );
    assert_eq!(
        Geometry::new(4096, 196_096, 32_768, 8_200, 256),
        Err(Error::Geometry)
    );
    // A last group too small for its own metadata.
    assert_eq!(
        Geometry::new(4096, 32_768 + 516, 32_768, 8_192, 256),
        Err(Error::Geometry)
    );
    assert!(Geometry::new(4096, 32_768 + 517, 32_768, 8_192, 256).is_ok());
    assert_eq!(
        Geometry::new(4096, 0, 32_768, 8_192, 256),
        Err(Error::Geometry)
    );
    assert_eq!(
        Geometry::new(1024, 1, 8_192, 2_048, 128),
        Err(Error::Geometry)
    );
}

#[test]
fn geometry_from_superblock_checks_consistency() {
    let superblock = d6_superblock();
    assert_eq!(Geometry::from_superblock(&superblock), Ok(d6_geometry()));
    let mut wrong_inodes = superblock;
    wrong_inodes.inodes_count += 1;
    assert_eq!(
        Geometry::from_superblock(&wrong_inodes),
        Err(Error::Geometry)
    );
    let mut wrong_first_block = superblock;
    wrong_first_block.first_data_block = 1;
    assert_eq!(
        Geometry::from_superblock(&wrong_first_block),
        Err(Error::Geometry)
    );
    let mut fragments = superblock;
    fragments.log_fragment_size = 3;
    assert_eq!(Geometry::from_superblock(&fragments), Err(Error::Geometry));
    let mut fragments_per_group = superblock;
    fragments_per_group.fragments_per_group = 16_384;
    assert_eq!(
        Geometry::from_superblock(&fragments_per_group),
        Err(Error::Geometry)
    );
    for (compat, incompat, ro_compat) in [
        (COMPAT_RESIZE_INODE, 0, 0),
        (0, INCOMPAT_META_BG, 0),
        (0, INCOMPAT_64BIT, 0),
        (0, INCOMPAT_FLEX_BG, 0),
        (0, 0, RO_COMPAT_SPARSE_SUPER),
        (0, 0, RO_COMPAT_BIGALLOC),
    ] {
        let mut featured = superblock;
        featured.feature_compat = compat;
        featured.feature_incompat = incompat;
        featured.feature_ro_compat = ro_compat;
        assert_eq!(Geometry::from_superblock(&featured), Err(Error::Feature));
    }
    // Features that leave the group layout alone are the caller's policy.
    let mut filetype = superblock;
    filetype.feature_incompat = INCOMPAT_FILETYPE;
    assert_eq!(Geometry::from_superblock(&filetype), Ok(d6_geometry()));
}

#[test]
fn bitmaps_count_from_the_low_bit() {
    let mut bitmap = [0u8; 2];
    bitmap_set(&mut bitmap, 0).unwrap();
    bitmap_set(&mut bitmap, 9).unwrap();
    assert_eq!(bitmap, [0x01, 0x02]);
    assert_eq!(bitmap_get(&bitmap, 0), Ok(true));
    assert_eq!(bitmap_get(&bitmap, 1), Ok(false));
    assert_eq!(bitmap_get(&bitmap, 9), Ok(true));
    assert_eq!(bitmap_get(&bitmap, 16), Err(Error::Truncated));
    assert_eq!(bitmap_set(&mut bitmap, 16), Err(Error::Truncated));
}
