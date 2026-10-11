//! The minimal ext2 root of decision 0015-D6: revision 1 with no features,
//! 4 KiB blocks, groups of 32,768 blocks and 8,192 256-byte inodes, a
//! superblock and descriptor-table copy in every group, and a root directory
//! holding `lost+found` (inode 11) and the marker file `mirror-root` (inode
//! 12). Everything else is zero. The on-disk records come from
//! `mirror-ext2-layout` (0015-D12).

use crate::{Error, ImageReader, ROOT_BYTES, ROOT_OFFSET, Result};
use mirror_ext2_layout::{
    self as layout, DirectoryEntry, Geometry, GroupDescriptor, Inode, ROOT_INODE, Superblock,
};

pub const BLOCK_SIZE: u32 = 4096;
pub const BLOCKS_PER_GROUP: u32 = 32_768;
pub const INODES_PER_GROUP: u32 = 8_192;
pub const INODE_SIZE: u16 = 256;
pub const LOST_AND_FOUND_INODE: u32 = 11;
pub const MARKER_INODE: u32 = 12;
pub const MARKER_NAME: &[u8] = b"mirror-root";
pub const MARKER_CONTENT: &[u8] = b"mirror-root 1\n";
pub const VOLUME_NAME: &[u8] = b"mirror-root";
/// Fixed, so every build writes the same bytes. Arbitrary: like the GPT
/// GUIDs, its first six bytes spell "MIRROR".
pub const UUID: [u8; 16] = [
    0x4d, 0x49, 0x52, 0x52, 0x4f, 0x52, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03,
];
/// 1980-01-01 00:00:00 UTC, the image's fixed timestamp.
pub const TIMESTAMP: u32 = 315_532_800;
/// The root directory, `lost+found` and the marker each take one block, at
/// the start of group 0's data area.
const DATA_BLOCKS: u32 = 3;
/// Inodes 1-12 are in use: the reserved inodes 1-10 (2 is the root), then
/// `lost+found` and the marker.
const USED_INODES: u32 = MARKER_INODE;
const DIRECTORIES: u32 = 2;
const SECTORS_PER_BLOCK: u32 = BLOCK_SIZE / 512;

impl From<layout::Error> for Error {
    fn from(error: layout::Error) -> Self {
        Error::new(format!("ext2 layout error: {error:?}"))
    }
}

/// Bytes to write at an offset from the start of the root partition.
pub struct Region {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// The D6 geometry: 196,096 blocks in 6 groups for the 766 MiB partition.
pub fn geometry() -> Result<Geometry> {
    Ok(Geometry::new(
        BLOCK_SIZE,
        (ROOT_BYTES / u64::from(BLOCK_SIZE)) as u32,
        BLOCKS_PER_GROUP,
        INODES_PER_GROUP,
        INODE_SIZE,
    )?)
}

/// The blocks of the root directory, `lost+found` and the marker.
struct DataBlocks {
    root: u32,
    lost_and_found: u32,
    marker: u32,
}

fn data_blocks(geometry: &Geometry) -> Result<DataBlocks> {
    let start = geometry.group_layout(0)?.data_start;
    Ok(DataBlocks {
        root: start,
        lost_and_found: start + 1,
        marker: start + 2,
    })
}

/// The non-zero metadata and data of the filesystem, in ascending order.
/// The caller zero-fills the rest of the partition.
pub fn format() -> Result<Vec<Region>> {
    let geometry = geometry()?;
    let blocks = data_blocks(&geometry)?;
    let mut descriptor_table =
        vec![0; geometry.descriptor_table_blocks() as usize * BLOCK_SIZE as usize];
    let mut groups = Vec::new();
    let (mut free_blocks, mut free_inodes) = (0, 0);
    for group in 0..geometry.group_count() {
        let group_layout = geometry.group_layout(group)?;
        let block_bitmap = block_bitmap(&geometry, group)?;
        let inode_bitmap = inode_bitmap(group)?;
        let descriptor = GroupDescriptor {
            block_bitmap: group_layout.block_bitmap,
            inode_bitmap: group_layout.inode_bitmap,
            inode_table: group_layout.inode_table,
            free_blocks_count: count_u16(
                group_layout.block_count - count_set(&block_bitmap, group_layout.block_count),
            )?,
            free_inodes_count: count_u16(
                INODES_PER_GROUP - count_set(&inode_bitmap, INODES_PER_GROUP),
            )?,
            used_directories_count: if group == 0 { DIRECTORIES as u16 } else { 0 },
            flags: 0,
        };
        free_blocks += u32::from(descriptor.free_blocks_count);
        free_inodes += u32::from(descriptor.free_inodes_count);
        let start = group as usize * layout::GROUP_DESCRIPTOR_SIZE;
        descriptor_table[start..start + layout::GROUP_DESCRIPTOR_SIZE]
            .copy_from_slice(&descriptor.encode());
        groups.push((group_layout, block_bitmap, inode_bitmap));
    }

    let mut regions = Vec::new();
    for (group, (group_layout, block_bitmap, inode_bitmap)) in (0..).zip(groups) {
        let superblock = superblock(&geometry, free_blocks, free_inodes, group)?;
        regions.push(Region {
            offset: geometry.superblock_offset(group)?,
            bytes: superblock.encode().to_vec(),
        });
        regions.push(Region {
            offset: block_offset(group_layout.descriptor_table),
            bytes: descriptor_table.clone(),
        });
        regions.push(Region {
            offset: block_offset(group_layout.block_bitmap),
            bytes: block_bitmap,
        });
        regions.push(Region {
            offset: block_offset(group_layout.inode_bitmap),
            bytes: inode_bitmap,
        });
        if group != 0 {
            continue;
        }
        // The first inode-table block holds inodes 1-16; only 2, 11 and 12 have
        // contents.
        let mut inode_table = vec![0; BLOCK_SIZE as usize];
        for (number, inode) in inodes(&blocks) {
            let position = geometry.inode_position(number)?;
            let start = position.table_offset as usize;
            inode_table
                .get_mut(start..start + usize::from(layout::GOOD_OLD_INODE_SIZE))
                .ok_or_else(|| Error::new("an ext2 inode lies past the first table block"))?
                .copy_from_slice(&inode.encode());
        }
        regions.push(Region {
            offset: block_offset(group_layout.inode_table),
            bytes: inode_table,
        });
        regions.push(Region {
            offset: block_offset(blocks.root),
            bytes: directory_block(&root_entries())?,
        });
        regions.push(Region {
            offset: block_offset(blocks.lost_and_found),
            bytes: directory_block(&lost_and_found_entries())?,
        });
        let mut marker = vec![0; BLOCK_SIZE as usize];
        marker[..MARKER_CONTENT.len()].copy_from_slice(MARKER_CONTENT);
        regions.push(Region {
            offset: block_offset(blocks.marker),
            bytes: marker,
        });
    }
    Ok(regions)
}

fn superblock(
    geometry: &Geometry,
    free_blocks: u32,
    free_inodes: u32,
    group: u32,
) -> Result<Superblock> {
    let mut volume_name = [0; 16];
    volume_name[..VOLUME_NAME.len()].copy_from_slice(VOLUME_NAME);
    Ok(Superblock {
        inodes_count: geometry.inodes_count(),
        blocks_count: geometry.blocks_count(),
        reserved_blocks_count: 0,
        free_blocks_count: free_blocks,
        free_inodes_count: free_inodes,
        first_data_block: geometry.first_data_block(),
        log_block_size: geometry.log_block_size(),
        log_fragment_size: geometry.log_block_size(),
        blocks_per_group: geometry.blocks_per_group(),
        fragments_per_group: geometry.blocks_per_group(),
        inodes_per_group: geometry.inodes_per_group(),
        mount_time: 0,
        write_time: TIMESTAMP,
        mount_count: 0,
        // -1, as mke2fs writes: no mount-count check. Linux warns on every
        // mount when this is 0.
        max_mount_count: u16::MAX,
        state: layout::STATE_CLEAN,
        // mke2fs's default; 0 is not a defined behavior.
        errors: layout::ERRORS_CONTINUE,
        minor_revision: 0,
        last_check_time: TIMESTAMP,
        check_interval: 0,
        creator_os: layout::CREATOR_OS_LINUX,
        revision: layout::DYNAMIC_REVISION,
        default_reserved_uid: 0,
        default_reserved_gid: 0,
        first_inode: layout::GOOD_OLD_FIRST_INODE,
        inode_size: INODE_SIZE,
        block_group: u16::try_from(group).map_err(|_| Error::new("too many ext2 groups"))?,
        feature_compat: 0,
        feature_incompat: 0,
        feature_ro_compat: 0,
        uuid: UUID,
        volume_name,
        last_mounted: [0; 64],
        mkfs_time: TIMESTAMP,
    })
}

/// The used inodes. Directories count a link from their parent's entry, one
/// from their own `.` and one from each subdirectory's `..`.
fn inodes(blocks: &DataBlocks) -> [(u32, Inode); 3] {
    let inode = |mode: u16, size: u32, links_count: u16, block: u32| {
        let mut pointers = [0; layout::BLOCK_POINTERS];
        pointers[0] = block;
        Inode {
            mode,
            size,
            access_time: TIMESTAMP,
            change_time: TIMESTAMP,
            modification_time: TIMESTAMP,
            links_count,
            sectors: SECTORS_PER_BLOCK,
            block: pointers,
            ..Inode::default()
        }
    };
    [
        (
            ROOT_INODE,
            inode(layout::MODE_DIRECTORY | 0o755, BLOCK_SIZE, 3, blocks.root),
        ),
        (
            LOST_AND_FOUND_INODE,
            inode(
                layout::MODE_DIRECTORY | 0o700,
                BLOCK_SIZE,
                2,
                blocks.lost_and_found,
            ),
        ),
        (
            MARKER_INODE,
            inode(
                layout::MODE_REGULAR | 0o644,
                MARKER_CONTENT.len() as u32,
                1,
                blocks.marker,
            ),
        ),
    ]
}

fn root_entries() -> [(&'static [u8], u32); 4] {
    [
        (b".", ROOT_INODE),
        (b"..", ROOT_INODE),
        (b"lost+found", LOST_AND_FOUND_INODE),
        (MARKER_NAME, MARKER_INODE),
    ]
}

fn lost_and_found_entries() -> [(&'static [u8], u32); 2] {
    [(b".", LOST_AND_FOUND_INODE), (b"..", ROOT_INODE)]
}

/// One directory block; the last entry's record runs to the end of it. The
/// file type bytes stay zero, as a filesystem without `filetype` requires.
fn directory_block(entries: &[(&[u8], u32)]) -> Result<Vec<u8>> {
    let mut block = vec![0; BLOCK_SIZE as usize];
    let mut offset = 0;
    for (index, (name, inode)) in entries.iter().enumerate() {
        let length = if index + 1 == entries.len() {
            block.len() - offset
        } else {
            DirectoryEntry::record_length_for(name.len())
        };
        let entry = DirectoryEntry {
            inode: *inode,
            record_length: u16::try_from(length).map_err(|_| Error::new("ext2 record too long"))?,
            file_type: layout::FILE_TYPE_UNKNOWN,
            name,
        };
        entry.encode(&mut block[offset..])?;
        offset += length;
    }
    Ok(block)
}

/// A group's block bitmap: its metadata blocks, then in group 0 the three
/// data blocks, are in use. Bits past the end of a short last group are set,
/// as e2fsck expects.
fn block_bitmap(geometry: &Geometry, group: u32) -> Result<Vec<u8>> {
    let block_count = geometry.blocks_in_group(group)?;
    let used = geometry.overhead_blocks() + if group == 0 { DATA_BLOCKS } else { 0 };
    let mut bitmap = vec![0; BLOCK_SIZE as usize];
    for index in (0..used).chain(block_count..BLOCKS_PER_GROUP) {
        layout::bitmap_set(&mut bitmap, index)?;
    }
    Ok(bitmap)
}

/// A group's inode bitmap. The bytes past the group's 8,192 inodes are
/// padding, all ones, as e2fsck expects.
fn inode_bitmap(group: u32) -> Result<Vec<u8>> {
    let mut bitmap = vec![0; BLOCK_SIZE as usize];
    if group == 0 {
        for index in 0..USED_INODES {
            layout::bitmap_set(&mut bitmap, index)?;
        }
    }
    bitmap[(INODES_PER_GROUP / 8) as usize..].fill(0xff);
    Ok(bitmap)
}

/// Reads the root filesystem back and checks it is exactly D6's: the primary
/// superblock and every group's copies, the descriptors against the layout,
/// both bitmaps and every free count, the root directory, `lost+found` and
/// the marker. Returns the group count.
pub fn check(reader: &mut ImageReader) -> Result<u32> {
    let geometry = geometry()?;
    let blocks = data_blocks(&geometry)?;
    let primary_bytes = reader.read_at(
        ROOT_OFFSET + layout::SUPERBLOCK_OFFSET,
        layout::SUPERBLOCK_SIZE,
    )?;
    let primary = Superblock::decode(&primary_bytes)?;
    if Geometry::from_superblock(&primary)? != geometry {
        return Err(Error::new(
            "the ext2 superblock does not describe the D6 geometry",
        ));
    }
    let expected = superblock(
        &geometry,
        primary.free_blocks_count,
        primary.free_inodes_count,
        0,
    )?;
    if primary_bytes != expected.encode() {
        return Err(Error::new(
            "the ext2 superblock differs from the one D6 specifies",
        ));
    }
    let table_bytes = geometry.descriptor_table_blocks() as usize * BLOCK_SIZE as usize;
    let descriptor_table = reader.read_at(
        block_position(geometry.group_layout(0)?.descriptor_table),
        table_bytes,
    )?;
    let (mut free_blocks, mut free_inodes, mut directories) = (0, 0, 0);
    for group in 0..geometry.group_count() {
        let group_layout = geometry.group_layout(group)?;
        if group != 0 {
            let copy = reader.read_at(
                ROOT_OFFSET + geometry.superblock_offset(group)?,
                layout::SUPERBLOCK_SIZE,
            )?;
            let mut expected_copy = primary;
            expected_copy.block_group = group as u16;
            if copy != expected_copy.encode() {
                return Err(Error::new(format!(
                    "ext2 group {group}'s superblock copy differs from the primary"
                )));
            }
            if reader.read_at(block_position(group_layout.descriptor_table), table_bytes)?
                != descriptor_table
            {
                return Err(Error::new(format!(
                    "ext2 group {group}'s descriptor table copy differs from the primary"
                )));
            }
        }
        let start = group as usize * layout::GROUP_DESCRIPTOR_SIZE;
        let descriptor = GroupDescriptor::decode(&descriptor_table[start..])?;
        if descriptor.block_bitmap != group_layout.block_bitmap
            || descriptor.inode_bitmap != group_layout.inode_bitmap
            || descriptor.inode_table != group_layout.inode_table
            || descriptor.flags != 0
            || descriptor_table[start + 20..start + layout::GROUP_DESCRIPTOR_SIZE]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(Error::new(format!(
                "ext2 group {group}'s descriptor does not match the layout"
            )));
        }
        let block_map = reader.read_at(
            block_position(group_layout.block_bitmap),
            BLOCK_SIZE as usize,
        )?;
        let inode_map = reader.read_at(
            block_position(group_layout.inode_bitmap),
            BLOCK_SIZE as usize,
        )?;
        if block_map != block_bitmap(&geometry, group)? || inode_map != inode_bitmap(group)? {
            return Err(Error::new(format!(
                "ext2 group {group}'s bitmaps are wrong"
            )));
        }
        let group_free_blocks =
            group_layout.block_count - count_set(&block_map, group_layout.block_count);
        let group_free_inodes = INODES_PER_GROUP - count_set(&inode_map, INODES_PER_GROUP);
        if u32::from(descriptor.free_blocks_count) != group_free_blocks
            || u32::from(descriptor.free_inodes_count) != group_free_inodes
        {
            return Err(Error::new(format!(
                "ext2 group {group}'s free counts disagree with its bitmaps"
            )));
        }
        free_blocks += group_free_blocks;
        free_inodes += group_free_inodes;
        directories += u32::from(descriptor.used_directories_count);
    }
    if primary.free_blocks_count != free_blocks || primary.free_inodes_count != free_inodes {
        return Err(Error::new(
            "the ext2 superblock's free counts disagree with the groups'",
        ));
    }
    if directories != DIRECTORIES {
        return Err(Error::new(
            "the ext2 groups count the wrong number of directories",
        ));
    }

    for (number, inode) in inodes(&blocks) {
        let record = read_inode(reader, &geometry, number)?;
        let mut expected_record = vec![0; usize::from(INODE_SIZE)];
        expected_record[..usize::from(layout::GOOD_OLD_INODE_SIZE)]
            .copy_from_slice(&inode.encode());
        if record != expected_record {
            return Err(Error::new(format!("ext2 inode {number} is wrong")));
        }
    }
    let root = read_inode(reader, &geometry, ROOT_INODE)?;
    let root = Inode::decode(&root)?;
    if listing(&reader.read_at(block_position(root.block[0]), BLOCK_SIZE as usize)?)?
        != root_entries()
    {
        return Err(Error::new(
            "the ext2 root directory does not hold lost+found and the marker",
        ));
    }
    let lost_and_found = Inode::decode(&read_inode(reader, &geometry, LOST_AND_FOUND_INODE)?)?;
    if listing(&reader.read_at(block_position(lost_and_found.block[0]), BLOCK_SIZE as usize)?)?
        != lost_and_found_entries()
    {
        return Err(Error::new("the ext2 lost+found directory is wrong"));
    }
    let marker = Inode::decode(&read_inode(reader, &geometry, MARKER_INODE)?)?;
    let content = reader.read_at(block_position(marker.block[0]), BLOCK_SIZE as usize)?;
    if !marker.is_regular_file()
        || marker.size as usize != MARKER_CONTENT.len()
        || &content[..MARKER_CONTENT.len()] != MARKER_CONTENT
        || content[MARKER_CONTENT.len()..]
            .iter()
            .any(|byte| *byte != 0)
    {
        return Err(Error::new(
            "the ext2 marker file does not hold 'mirror-root 1'",
        ));
    }
    Ok(geometry.group_count())
}

fn read_inode(reader: &mut ImageReader, geometry: &Geometry, number: u32) -> Result<Vec<u8>> {
    let position = geometry.inode_position(number)?;
    let table = geometry.group_layout(position.group)?.inode_table;
    reader.read_at(
        block_position(table) + position.table_offset,
        usize::from(INODE_SIZE),
    )
}

/// A directory block's used entries, as (name, inode).
fn listing(block: &[u8]) -> Result<Vec<(&[u8], u32)>> {
    let mut entries = Vec::new();
    for entry in layout::directory_entries(block) {
        let entry = entry?;
        if entry.file_type != layout::FILE_TYPE_UNKNOWN {
            return Err(Error::new(
                "an ext2 directory entry has a file type without `filetype`",
            ));
        }
        if entry.inode != 0 {
            entries.push((entry.name, entry.inode));
        }
    }
    Ok(entries)
}

fn block_offset(block: u32) -> u64 {
    u64::from(block) * u64::from(BLOCK_SIZE)
}

/// A block's offset in the image.
fn block_position(block: u32) -> u64 {
    ROOT_OFFSET + block_offset(block)
}

/// The set bits among the first `bits` of a bitmap.
pub fn count_set(bitmap: &[u8], bits: u32) -> u32 {
    (0..bits)
        .filter(|index| layout::bitmap_get(bitmap, *index) == Ok(true))
        .count() as u32
}

fn count_u16(count: u32) -> Result<u16> {
    u16::try_from(count).map_err(|_| Error::new("an ext2 group count exceeds 16 bits"))
}
