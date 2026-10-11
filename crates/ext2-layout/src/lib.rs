#![no_std]
#![forbid(unsafe_code)]
//! ext2 on-disk format definitions (spec 0015a, decision 0015-D12): the
//! superblock, block group descriptor, inode and directory-entry records with
//! their little-endian encodings, and the block-group geometry. The host tool
//! `tools/mirror-image` formats Mirror's root partition with it, and spec
//! 0064's driver reuses and extends it, so the formatter and the reader share
//! one definition of the format.
//!
//! Format definitions only: no allocation and no I/O. Callers own the bytes.
//! Decoders check lengths and the fields a record needs to be meaningful;
//! which features and values a filesystem may use is the caller's policy.
//!
//! The geometry covers the layout without `sparse_super`, `resize_inode`,
//! `flex_bg`, `meta_bg`, `64bit` or `bigalloc`: every group holds a superblock
//! copy, a descriptor-table copy, its block bitmap, its inode bitmap and its
//! inode table, in that order, then data blocks.
//!
//! Layouts follow "The Second Extended File System" (Dave Poirier),
//! <https://www.nongnu.org/ext2-doc/ext2.html>, and the Linux kernel's
//! `Documentation/filesystems/ext4/` for the revision 1 fields.

/// Byte offset of the primary superblock from the start of the filesystem,
/// whatever the block size.
pub const SUPERBLOCK_OFFSET: u64 = 1024;
pub const SUPERBLOCK_SIZE: usize = 1024;
pub const MAGIC: u16 = 0xef53;
pub const GOOD_OLD_REVISION: u32 = 0;
pub const DYNAMIC_REVISION: u32 = 1;
/// The inode size of revision 0, and the bytes `Inode` encodes in any
/// revision. Larger revision 1 inodes keep their extra fields after these.
pub const GOOD_OLD_INODE_SIZE: u16 = 128;
/// The first non-reserved inode of revision 0 (and the usual revision 1
/// value). Inodes below it are reserved.
pub const GOOD_OLD_FIRST_INODE: u32 = 11;
pub const ROOT_INODE: u32 = 2;
pub const GROUP_DESCRIPTOR_SIZE: usize = 32;
pub const MIN_BLOCK_SIZE: u32 = 1024;
pub const MAX_BLOCK_SIZE: u32 = 65536;
pub const DIRECT_BLOCKS: usize = 12;
pub const BLOCK_POINTERS: usize = 15;
pub const DIRECTORY_ENTRY_HEADER_SIZE: usize = 8;
pub const MAX_NAME_LENGTH: usize = 255;

pub const STATE_CLEAN: u16 = 1;
pub const STATE_ERRORS: u16 = 2;
pub const ERRORS_CONTINUE: u16 = 1;
pub const ERRORS_READ_ONLY: u16 = 2;
pub const ERRORS_PANIC: u16 = 3;
pub const CREATOR_OS_LINUX: u32 = 0;

pub const COMPAT_RESIZE_INODE: u32 = 0x0010;
/// Directory entries carry a file type byte. Without it that byte is zero.
pub const INCOMPAT_FILETYPE: u32 = 0x0002;
pub const INCOMPAT_META_BG: u32 = 0x0010;
pub const INCOMPAT_64BIT: u32 = 0x0080;
pub const INCOMPAT_FLEX_BG: u32 = 0x0200;
pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const RO_COMPAT_BIGALLOC: u32 = 0x0200;

pub const MODE_TYPE_MASK: u16 = 0xf000;
pub const MODE_DIRECTORY: u16 = 0x4000;
pub const MODE_REGULAR: u16 = 0x8000;

/// Directory-entry file types, used only with `INCOMPAT_FILETYPE`.
pub const FILE_TYPE_UNKNOWN: u8 = 0;
pub const FILE_TYPE_REGULAR: u8 = 1;
pub const FILE_TYPE_DIRECTORY: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Magic,
    Revision,
    BlockSize,
    InodeSize,
    Geometry,
    Feature,
    Group,
    InodeNumber,
    RecordLength,
    NameLength,
}

/// The superblock fields this crate encodes: everything up to offset 200,
/// plus `mkfs_time` at 264. Encoding zeroes every other byte, which holds
/// the preallocation, journal, hash and other `ext3`/`ext4` fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub reserved_blocks_count: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub log_fragment_size: u32,
    pub blocks_per_group: u32,
    pub fragments_per_group: u32,
    pub inodes_per_group: u32,
    pub mount_time: u32,
    pub write_time: u32,
    pub mount_count: u16,
    pub max_mount_count: u16,
    pub state: u16,
    pub errors: u16,
    pub minor_revision: u16,
    pub last_check_time: u32,
    pub check_interval: u32,
    pub creator_os: u32,
    pub revision: u32,
    pub default_reserved_uid: u16,
    pub default_reserved_gid: u16,
    pub first_inode: u32,
    pub inode_size: u16,
    /// The group holding this copy: zero in the primary superblock.
    pub block_group: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub volume_name: [u8; 16],
    pub last_mounted: [u8; 64],
    pub mkfs_time: u32,
}

impl Superblock {
    /// Decodes a superblock. Checks the length, the magic number and the
    /// revision, which must be 0 or 1.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        require(bytes, SUPERBLOCK_SIZE)?;
        if u16_at(bytes, 56) != MAGIC {
            return Err(Error::Magic);
        }
        let revision = u32_at(bytes, 76);
        if revision > DYNAMIC_REVISION {
            return Err(Error::Revision);
        }
        Ok(Self {
            inodes_count: u32_at(bytes, 0),
            blocks_count: u32_at(bytes, 4),
            reserved_blocks_count: u32_at(bytes, 8),
            free_blocks_count: u32_at(bytes, 12),
            free_inodes_count: u32_at(bytes, 16),
            first_data_block: u32_at(bytes, 20),
            log_block_size: u32_at(bytes, 24),
            log_fragment_size: u32_at(bytes, 28),
            blocks_per_group: u32_at(bytes, 32),
            fragments_per_group: u32_at(bytes, 36),
            inodes_per_group: u32_at(bytes, 40),
            mount_time: u32_at(bytes, 44),
            write_time: u32_at(bytes, 48),
            mount_count: u16_at(bytes, 52),
            max_mount_count: u16_at(bytes, 54),
            state: u16_at(bytes, 58),
            errors: u16_at(bytes, 60),
            minor_revision: u16_at(bytes, 62),
            last_check_time: u32_at(bytes, 64),
            check_interval: u32_at(bytes, 68),
            creator_os: u32_at(bytes, 72),
            revision,
            default_reserved_uid: u16_at(bytes, 80),
            default_reserved_gid: u16_at(bytes, 82),
            first_inode: u32_at(bytes, 84),
            inode_size: u16_at(bytes, 88),
            block_group: u16_at(bytes, 90),
            feature_compat: u32_at(bytes, 92),
            feature_incompat: u32_at(bytes, 96),
            feature_ro_compat: u32_at(bytes, 100),
            uuid: array_at(bytes, 104),
            volume_name: array_at(bytes, 120),
            last_mounted: array_at(bytes, 136),
            mkfs_time: u32_at(bytes, 264),
        })
    }

    pub fn encode(&self) -> [u8; SUPERBLOCK_SIZE] {
        let mut bytes = [0; SUPERBLOCK_SIZE];
        put_u32(&mut bytes, 0, self.inodes_count);
        put_u32(&mut bytes, 4, self.blocks_count);
        put_u32(&mut bytes, 8, self.reserved_blocks_count);
        put_u32(&mut bytes, 12, self.free_blocks_count);
        put_u32(&mut bytes, 16, self.free_inodes_count);
        put_u32(&mut bytes, 20, self.first_data_block);
        put_u32(&mut bytes, 24, self.log_block_size);
        put_u32(&mut bytes, 28, self.log_fragment_size);
        put_u32(&mut bytes, 32, self.blocks_per_group);
        put_u32(&mut bytes, 36, self.fragments_per_group);
        put_u32(&mut bytes, 40, self.inodes_per_group);
        put_u32(&mut bytes, 44, self.mount_time);
        put_u32(&mut bytes, 48, self.write_time);
        put_u16(&mut bytes, 52, self.mount_count);
        put_u16(&mut bytes, 54, self.max_mount_count);
        put_u16(&mut bytes, 56, MAGIC);
        put_u16(&mut bytes, 58, self.state);
        put_u16(&mut bytes, 60, self.errors);
        put_u16(&mut bytes, 62, self.minor_revision);
        put_u32(&mut bytes, 64, self.last_check_time);
        put_u32(&mut bytes, 68, self.check_interval);
        put_u32(&mut bytes, 72, self.creator_os);
        put_u32(&mut bytes, 76, self.revision);
        put_u16(&mut bytes, 80, self.default_reserved_uid);
        put_u16(&mut bytes, 82, self.default_reserved_gid);
        put_u32(&mut bytes, 84, self.first_inode);
        put_u16(&mut bytes, 88, self.inode_size);
        put_u16(&mut bytes, 90, self.block_group);
        put_u32(&mut bytes, 92, self.feature_compat);
        put_u32(&mut bytes, 96, self.feature_incompat);
        put_u32(&mut bytes, 100, self.feature_ro_compat);
        bytes[104..120].copy_from_slice(&self.uuid);
        bytes[120..136].copy_from_slice(&self.volume_name);
        bytes[136..200].copy_from_slice(&self.last_mounted);
        put_u32(&mut bytes, 264, self.mkfs_time);
        bytes
    }

    /// The block size, if `log_block_size` names one from 1 KiB to 64 KiB.
    pub fn block_size(&self) -> Result<u32, Error> {
        if self.log_block_size > (MAX_BLOCK_SIZE / MIN_BLOCK_SIZE).trailing_zeros() {
            return Err(Error::BlockSize);
        }
        Ok(MIN_BLOCK_SIZE << self.log_block_size)
    }

    /// The inode record size: revision 0 fixes it at 128 bytes.
    pub fn inode_record_size(&self) -> u16 {
        if self.revision == GOOD_OLD_REVISION {
            GOOD_OLD_INODE_SIZE
        } else {
            self.inode_size
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupDescriptor {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_directories_count: u16,
    pub flags: u16,
}

impl GroupDescriptor {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        require(bytes, GROUP_DESCRIPTOR_SIZE)?;
        Ok(Self {
            block_bitmap: u32_at(bytes, 0),
            inode_bitmap: u32_at(bytes, 4),
            inode_table: u32_at(bytes, 8),
            free_blocks_count: u16_at(bytes, 12),
            free_inodes_count: u16_at(bytes, 14),
            used_directories_count: u16_at(bytes, 16),
            flags: u16_at(bytes, 18),
        })
    }

    /// Encodes the descriptor. The checksum and `ext4` fields from offset 20
    /// on are zero.
    pub fn encode(&self) -> [u8; GROUP_DESCRIPTOR_SIZE] {
        let mut bytes = [0; GROUP_DESCRIPTOR_SIZE];
        put_u32(&mut bytes, 0, self.block_bitmap);
        put_u32(&mut bytes, 4, self.inode_bitmap);
        put_u32(&mut bytes, 8, self.inode_table);
        put_u16(&mut bytes, 12, self.free_blocks_count);
        put_u16(&mut bytes, 14, self.free_inodes_count);
        put_u16(&mut bytes, 16, self.used_directories_count);
        put_u16(&mut bytes, 18, self.flags);
        bytes
    }
}

/// The first 128 bytes of an inode. In a larger revision 1 inode the bytes
/// after them start with `i_extra_isize`; leaving them zero means the inode
/// has no extra fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inode {
    pub mode: u16,
    pub uid: u16,
    pub size: u32,
    pub access_time: u32,
    pub change_time: u32,
    pub modification_time: u32,
    pub deletion_time: u32,
    pub gid: u16,
    pub links_count: u16,
    /// `i_blocks`: the 512-byte sectors the inode's blocks occupy.
    pub sectors: u32,
    pub flags: u32,
    /// Twelve direct block numbers, then the single, double and triple
    /// indirect blocks.
    pub block: [u32; BLOCK_POINTERS],
    pub generation: u32,
    pub file_acl: u32,
    /// The high 32 bits of a regular file's size (`i_size_high`).
    pub size_high: u32,
}

impl Inode {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        require(bytes, usize::from(GOOD_OLD_INODE_SIZE))?;
        let mut block = [0; BLOCK_POINTERS];
        for (index, pointer) in block.iter_mut().enumerate() {
            *pointer = u32_at(bytes, 40 + 4 * index);
        }
        Ok(Self {
            mode: u16_at(bytes, 0),
            uid: u16_at(bytes, 2),
            size: u32_at(bytes, 4),
            access_time: u32_at(bytes, 8),
            change_time: u32_at(bytes, 12),
            modification_time: u32_at(bytes, 16),
            deletion_time: u32_at(bytes, 20),
            gid: u16_at(bytes, 24),
            links_count: u16_at(bytes, 26),
            sectors: u32_at(bytes, 28),
            flags: u32_at(bytes, 32),
            block,
            generation: u32_at(bytes, 100),
            file_acl: u32_at(bytes, 104),
            size_high: u32_at(bytes, 108),
        })
    }

    /// Encodes the inode. The OS-dependent fields and the fragment address
    /// are zero.
    pub fn encode(&self) -> [u8; GOOD_OLD_INODE_SIZE as usize] {
        let mut bytes = [0; GOOD_OLD_INODE_SIZE as usize];
        put_u16(&mut bytes, 0, self.mode);
        put_u16(&mut bytes, 2, self.uid);
        put_u32(&mut bytes, 4, self.size);
        put_u32(&mut bytes, 8, self.access_time);
        put_u32(&mut bytes, 12, self.change_time);
        put_u32(&mut bytes, 16, self.modification_time);
        put_u32(&mut bytes, 20, self.deletion_time);
        put_u16(&mut bytes, 24, self.gid);
        put_u16(&mut bytes, 26, self.links_count);
        put_u32(&mut bytes, 28, self.sectors);
        put_u32(&mut bytes, 32, self.flags);
        for (index, pointer) in self.block.iter().enumerate() {
            put_u32(&mut bytes, 40 + 4 * index, *pointer);
        }
        put_u32(&mut bytes, 100, self.generation);
        put_u32(&mut bytes, 104, self.file_acl);
        put_u32(&mut bytes, 108, self.size_high);
        bytes
    }

    pub fn is_directory(&self) -> bool {
        self.mode & MODE_TYPE_MASK == MODE_DIRECTORY
    }

    pub fn is_regular_file(&self) -> bool {
        self.mode & MODE_TYPE_MASK == MODE_REGULAR
    }
}

/// One directory record. Byte 6 is the name length and byte 7 the file type,
/// which must be zero without `INCOMPAT_FILETYPE` (revision 0 reads the two
/// bytes as one 16-bit length, the same value for names of 255 bytes or fewer).
/// An entry with inode 0 is unused space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryEntry<'a> {
    pub inode: u32,
    pub record_length: u16,
    pub file_type: u8,
    pub name: &'a [u8],
}

impl<'a> DirectoryEntry<'a> {
    /// The smallest record that holds a name of this length: the header and
    /// the name, rounded up to four bytes.
    pub const fn record_length_for(name_length: usize) -> usize {
        (DIRECTORY_ENTRY_HEADER_SIZE + name_length + 3) & !3
    }

    /// Decodes the record at the start of `bytes`, which ends where the
    /// directory block ends. The record must lie within it.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Error> {
        require(bytes, DIRECTORY_ENTRY_HEADER_SIZE)?;
        let inode = u32_at(bytes, 0);
        let record_length = u16_at(bytes, 4);
        let name_length = usize::from(bytes[6]);
        let length = usize::from(record_length);
        if !length.is_multiple_of(4)
            || length < Self::record_length_for(name_length)
            || length > bytes.len()
        {
            return Err(Error::RecordLength);
        }
        if inode != 0 && name_length == 0 {
            return Err(Error::NameLength);
        }
        Ok(Self {
            inode,
            record_length,
            file_type: bytes[7],
            name: &bytes[DIRECTORY_ENTRY_HEADER_SIZE..DIRECTORY_ENTRY_HEADER_SIZE + name_length],
        })
    }

    /// Writes the record to the start of `out` and zeroes the rest of it.
    pub fn encode(&self, out: &mut [u8]) -> Result<(), Error> {
        if self.name.is_empty() || self.name.len() > MAX_NAME_LENGTH {
            return Err(Error::NameLength);
        }
        let length = usize::from(self.record_length);
        if !length.is_multiple_of(4) || length < Self::record_length_for(self.name.len()) {
            return Err(Error::RecordLength);
        }
        require(out, length)?;
        let record = &mut out[..length];
        record.fill(0);
        put_u32(record, 0, self.inode);
        put_u16(record, 4, self.record_length);
        record[6] = self.name.len() as u8;
        record[7] = self.file_type;
        record[DIRECTORY_ENTRY_HEADER_SIZE..DIRECTORY_ENTRY_HEADER_SIZE + self.name.len()]
            .copy_from_slice(self.name);
        Ok(())
    }
}

/// Iterates the records of one directory block, unused ones included. After
/// the first malformed record it yields that error and then stops.
pub struct DirectoryEntries<'a> {
    block: &'a [u8],
    offset: usize,
}

pub fn directory_entries(block: &[u8]) -> DirectoryEntries<'_> {
    DirectoryEntries { block, offset: 0 }
}

impl<'a> Iterator for DirectoryEntries<'a> {
    type Item = Result<DirectoryEntry<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.block.len() {
            return None;
        }
        match DirectoryEntry::decode(&self.block[self.offset..]) {
            Ok(entry) => {
                self.offset += usize::from(entry.record_length);
                Some(Ok(entry))
            }
            Err(error) => {
                self.offset = self.block.len();
                Some(Err(error))
            }
        }
    }
}

/// Reads bit `index` of a block or inode bitmap. Bit 0 is the low bit of byte 0.
pub fn bitmap_get(bitmap: &[u8], index: u32) -> Result<bool, Error> {
    let byte = bitmap.get(index as usize / 8).ok_or(Error::Truncated)?;
    Ok(byte & (1 << (index % 8)) != 0)
}

pub fn bitmap_set(bitmap: &mut [u8], index: u32) -> Result<(), Error> {
    let byte = bitmap.get_mut(index as usize / 8).ok_or(Error::Truncated)?;
    *byte |= 1 << (index % 8);
    Ok(())
}

/// Validated block-group geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    block_size: u32,
    blocks_count: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    inode_size: u16,
    group_count: u32,
    descriptor_table_blocks: u32,
    inode_table_blocks: u32,
}

/// Where one group's blocks lie, as absolute block numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupLayout {
    pub first_block: u32,
    pub block_count: u32,
    /// The block holding this group's superblock copy. In group 0 with blocks
    /// larger than 1 KiB, block 0 holds the superblock at byte 1024.
    pub superblock: u32,
    pub descriptor_table: u32,
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    /// The first block after the inode table: the group's first data block.
    pub data_start: u32,
}

/// An inode's group, its index within the group, and the byte offset of its
/// record from the start of that group's inode table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InodePosition {
    pub group: u32,
    pub index: u32,
    pub table_offset: u64,
}

impl Geometry {
    /// Checks a geometry: a block size from 1 KiB to 64 KiB; an inode size
    /// that is a power of two from 128 bytes to the block size; per-group
    /// block and inode counts that are multiples of eight and fit one bitmap
    /// block, with whole inode-table blocks; and every group, the last
    /// included, large enough for its metadata and at least one data block.
    pub fn new(
        block_size: u32,
        blocks_count: u32,
        blocks_per_group: u32,
        inodes_per_group: u32,
        inode_size: u16,
    ) -> Result<Self, Error> {
        if !block_size.is_power_of_two() || !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size)
        {
            return Err(Error::BlockSize);
        }
        if !inode_size.is_power_of_two()
            || inode_size < GOOD_OLD_INODE_SIZE
            || u32::from(inode_size) > block_size
        {
            return Err(Error::InodeSize);
        }
        let bitmap_bits = block_size * 8;
        let inodes_per_block = block_size / u32::from(inode_size);
        if blocks_per_group < 8
            || blocks_per_group > bitmap_bits
            || !blocks_per_group.is_multiple_of(8)
        {
            return Err(Error::Geometry);
        }
        if inodes_per_group < 8
            || inodes_per_group > bitmap_bits
            || !inodes_per_group.is_multiple_of(8)
            || !inodes_per_group.is_multiple_of(inodes_per_block)
        {
            return Err(Error::Geometry);
        }
        let first_data_block = u32::from(block_size == MIN_BLOCK_SIZE);
        if blocks_count <= first_data_block {
            return Err(Error::Geometry);
        }
        let group_count = (blocks_count - first_data_block).div_ceil(blocks_per_group);
        if u64::from(group_count) * u64::from(inodes_per_group) > u64::from(u32::MAX) {
            return Err(Error::Geometry);
        }
        let descriptor_table_blocks =
            (u64::from(group_count) * GROUP_DESCRIPTOR_SIZE as u64).div_ceil(u64::from(block_size));
        let inode_table_blocks = inodes_per_group / inodes_per_block;
        let geometry = Self {
            block_size,
            blocks_count,
            blocks_per_group,
            inodes_per_group,
            inode_size,
            group_count,
            descriptor_table_blocks: u32::try_from(descriptor_table_blocks)
                .map_err(|_| Error::Geometry)?,
            inode_table_blocks,
        };
        let overhead = 3 + descriptor_table_blocks + u64::from(inode_table_blocks);
        if overhead >= u64::from(geometry.blocks_in_group(group_count - 1)?) {
            return Err(Error::Geometry);
        }
        Ok(geometry)
    }

    /// The geometry a superblock describes. Rejects revisions and features
    /// that change the group layout, a fragment size other than the block
    /// size, and counts that disagree with the computed geometry.
    pub fn from_superblock(superblock: &Superblock) -> Result<Self, Error> {
        if superblock.revision > DYNAMIC_REVISION {
            return Err(Error::Revision);
        }
        if superblock.feature_compat & COMPAT_RESIZE_INODE != 0
            || superblock.feature_incompat & (INCOMPAT_META_BG | INCOMPAT_64BIT | INCOMPAT_FLEX_BG)
                != 0
            || superblock.feature_ro_compat & (RO_COMPAT_SPARSE_SUPER | RO_COMPAT_BIGALLOC) != 0
        {
            return Err(Error::Feature);
        }
        let block_size = superblock.block_size()?;
        if superblock.log_fragment_size != superblock.log_block_size
            || superblock.fragments_per_group != superblock.blocks_per_group
        {
            return Err(Error::Geometry);
        }
        let geometry = Self::new(
            block_size,
            superblock.blocks_count,
            superblock.blocks_per_group,
            superblock.inodes_per_group,
            superblock.inode_record_size(),
        )?;
        if superblock.first_data_block != geometry.first_data_block()
            || superblock.inodes_count != geometry.inodes_count()
        {
            return Err(Error::Geometry);
        }
        Ok(geometry)
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// `log_block_size` for the superblock: the block size is 1024 << this.
    pub fn log_block_size(&self) -> u32 {
        (self.block_size / MIN_BLOCK_SIZE).trailing_zeros()
    }

    pub fn blocks_count(&self) -> u32 {
        self.blocks_count
    }

    pub fn blocks_per_group(&self) -> u32 {
        self.blocks_per_group
    }

    pub fn inodes_per_group(&self) -> u32 {
        self.inodes_per_group
    }

    pub fn inode_size(&self) -> u16 {
        self.inode_size
    }

    pub fn group_count(&self) -> u32 {
        self.group_count
    }

    pub fn inodes_count(&self) -> u32 {
        self.group_count * self.inodes_per_group
    }

    /// Block 1 with 1 KiB blocks, where block 0 is the boot block; otherwise
    /// block 0, which holds the superblock at byte 1024.
    pub fn first_data_block(&self) -> u32 {
        u32::from(self.block_size == MIN_BLOCK_SIZE)
    }

    pub fn descriptor_table_blocks(&self) -> u32 {
        self.descriptor_table_blocks
    }

    pub fn inode_table_blocks(&self) -> u32 {
        self.inode_table_blocks
    }

    /// The metadata blocks at the start of every group: the superblock copy,
    /// the descriptor table, the two bitmaps and the inode table.
    pub fn overhead_blocks(&self) -> u32 {
        3 + self.descriptor_table_blocks + self.inode_table_blocks
    }

    /// Blocks in a group: `blocks_per_group`, except a shorter last group.
    pub fn blocks_in_group(&self, group: u32) -> Result<u32, Error> {
        if group >= self.group_count {
            return Err(Error::Group);
        }
        let first = self.first_data_block() + group * self.blocks_per_group;
        Ok((self.blocks_count - first).min(self.blocks_per_group))
    }

    pub fn group_layout(&self, group: u32) -> Result<GroupLayout, Error> {
        let block_count = self.blocks_in_group(group)?;
        let first_block = self.first_data_block() + group * self.blocks_per_group;
        let descriptor_table = first_block + 1;
        let block_bitmap = descriptor_table + self.descriptor_table_blocks;
        let inode_bitmap = block_bitmap + 1;
        let inode_table = inode_bitmap + 1;
        Ok(GroupLayout {
            first_block,
            block_count,
            superblock: first_block,
            descriptor_table,
            block_bitmap,
            inode_bitmap,
            inode_table,
            data_start: inode_table + self.inode_table_blocks,
        })
    }

    /// The byte offset of a group's superblock copy from the start of the
    /// filesystem: 1024 for group 0, otherwise the start of the group's
    /// first block.
    pub fn superblock_offset(&self, group: u32) -> Result<u64, Error> {
        let layout = self.group_layout(group)?;
        if group == 0 {
            Ok(SUPERBLOCK_OFFSET)
        } else {
            Ok(u64::from(layout.superblock) * u64::from(self.block_size))
        }
    }

    /// Where inode `inode` (numbered from 1) lives.
    pub fn inode_position(&self, inode: u32) -> Result<InodePosition, Error> {
        if inode == 0 || inode > self.inodes_count() {
            return Err(Error::InodeNumber);
        }
        let index = (inode - 1) % self.inodes_per_group;
        Ok(InodePosition {
            group: (inode - 1) / self.inodes_per_group,
            index,
            table_offset: u64::from(index) * u64::from(self.inode_size),
        })
    }
}

fn require(bytes: &[u8], length: usize) -> Result<(), Error> {
    if bytes.len() < length {
        Err(Error::Truncated)
    } else {
        Ok(())
    }
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

fn array_at<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    let mut array = [0; N];
    array.copy_from_slice(&bytes[offset..offset + N]);
    array
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests;
