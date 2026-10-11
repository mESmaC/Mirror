//! The FAT32 EFI system partition of decision 0015-D5, written and read back
//! through the `fatfs` crate (0015-D4): label `MIRROR`, 2 KiB clusters, a
//! fixed volume ID, 1980-01-01 timestamps, and every directory's entries
//! written in sorted order. A raw pass after `fatfs` gives every subdirectory
//! the `.` and `..` entries FAT requires, and `check` reads them back raw.

use crate::{ESP_BYTES, ESP_OFFSET, Error, ImageReader, Result, SECTOR_SIZE};
use fatfs::{
    Date, DateTime, Dir, FatType, FileSystem, FormatVolumeOptions, FsOptions, ReadWriteSeek, Time,
    TimeProvider,
};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const CLUSTER_SIZE: u32 = 2048;
/// A FAT volume with fewer clusters is FAT16 by definition, whatever its
/// boot sector says.
pub const FAT32_MIN_CLUSTERS: u32 = 65_525;
/// Fixed, so every build writes the same bytes. Arbitrary: "MIRR" in ASCII.
pub const VOLUME_ID: u32 = 0x4d49_5252;
pub const VOLUME_LABEL: &str = "MIRROR";
/// Limine's license files, which the image must carry because it
/// redistributes Limine binaries (BSD-2-Clause). The caller stages them.
pub const LICENSE_FILES: [&str; 2] = ["LICENSES/limine/LICENSE", "LICENSES/limine/3RDPARTY.md"];
const MAX_DEPTH: usize = 16;
const COMPARE_CHUNK: usize = 64 * 1024;
/// FAT32 FAT entries at or above this end a cluster chain.
const END_OF_CHAIN: u32 = 0x0fff_fff8;
const ENTRY_BYTES: usize = 32;
const END_OF_DIRECTORY: u8 = 0x00;
const DELETED_ENTRY: u8 = 0xe5;
const ATTRIBUTE_VOLUME_ID: u8 = 0x08;
const ATTRIBUTE_DIRECTORY: u8 = 0x10;
/// A long-name slot's attribute byte, which includes the volume ID bit.
const ATTRIBUTE_LONG_NAME: u8 = 0x0f;
const DOT: &[u8; 11] = b".          ";
const DOT_DOT: &[u8; 11] = b"..         ";

/// A staged file or directory, by its `/`-separated path from the ESP root.
#[derive(Debug)]
pub enum Item {
    Directory(String),
    File {
        path: String,
        source: PathBuf,
        size: u64,
    },
}

/// A staged ESP tree in the order it is written: depth first, each
/// directory's entries sorted by name.
#[derive(Debug)]
pub struct Tree {
    items: Vec<Item>,
    content_bytes: u64,
}

impl Tree {
    pub fn file_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item, Item::File { .. }))
            .count()
    }

    /// The total size of the staged files.
    pub fn content_bytes(&self) -> u64 {
        self.content_bytes
    }

    fn listing(&self) -> Vec<(String, Option<u64>)> {
        self.items
            .iter()
            .map(|item| match item {
                Item::Directory(path) => (path.clone(), None),
                Item::File { path, size, .. } => (path.clone(), Some(*size)),
            })
            .collect()
    }
}

/// Reads and checks a staged ESP tree: plain files and directories only,
/// UTF-8 names that stay distinct without case, Limine's license files
/// present, and no more file bytes than the partition holds.
pub fn collect(root: &Path) -> Result<Tree> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| Error::io(format!("cannot read {}", root.display()), error))?;
    if !metadata.is_dir() {
        return Err(Error::new(format!("{} is not a directory", root.display())));
    }
    let mut tree = Tree {
        items: Vec::new(),
        content_bytes: 0,
    };
    walk(root, "", 0, &mut tree)?;
    if tree.content_bytes > ESP_BYTES {
        return Err(Error::new(format!(
            "the staged ESP {} holds {} bytes of files, more than the {ESP_BYTES}-byte ESP partition",
            root.display(),
            tree.content_bytes
        )));
    }
    for license in LICENSE_FILES {
        if !tree
            .items
            .iter()
            .any(|item| matches!(item, Item::File { path, .. } if path == license))
        {
            return Err(Error::new(format!(
                "the staged ESP {} lacks /{license} from the Limine release",
                root.display()
            )));
        }
    }
    Ok(tree)
}

fn walk(directory: &Path, prefix: &str, depth: usize, tree: &mut Tree) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::new(format!(
            "{} is nested too deeply",
            directory.display()
        )));
    }
    let context = || format!("cannot read {}", directory.display());
    let mut children = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| Error::io(context(), error))? {
        let entry = entry.map_err(|error| Error::io(context(), error))?;
        let name = entry.file_name().into_string().map_err(|name| {
            Error::new(format!(
                "{} has a name that is not UTF-8: {name:?}",
                directory.display()
            ))
        })?;
        children.push((name, entry.path()));
    }
    children.sort();
    // FAT compares names without case, so two such names would be one file.
    let mut folded = HashSet::new();
    for (name, _) in &children {
        if !folded.insert(name.to_uppercase()) {
            return Err(Error::new(format!(
                "{} holds names that differ only in case, such as {name}",
                directory.display()
            )));
        }
    }
    for (name, path) in children {
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| Error::io(format!("cannot read {}", path.display()), error))?;
        if metadata.is_dir() {
            tree.items.push(Item::Directory(relative.clone()));
            walk(&path, &relative, depth + 1, tree)?;
        } else if metadata.is_file() {
            tree.content_bytes = tree.content_bytes.saturating_add(metadata.len());
            tree.items.push(Item::File {
                path: relative,
                source: path,
                size: metadata.len(),
            });
        } else {
            return Err(Error::new(format!(
                "{} is neither a file nor a directory",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Stamps every entry with the DOS epoch, 1980-01-01 00:00:00.
#[derive(Debug)]
struct DosEpoch;

impl TimeProvider for DosEpoch {
    fn get_current_date(&self) -> Date {
        Date {
            year: 1980,
            month: 1,
            day: 1,
        }
    }

    fn get_current_date_time(&self) -> DateTime {
        DateTime {
            date: self.get_current_date(),
            time: Time {
                hour: 0,
                min: 0,
                sec: 0,
                millis: 0,
            },
        }
    }
}

static DOS_EPOCH: DosEpoch = DosEpoch;

/// Formats the partition in memory and copies the tree into it. Fails if the
/// files and their directories do not fit.
pub fn build(tree: &Tree) -> Result<Vec<u8>> {
    let mut volume = vec![0; ESP_BYTES as usize];
    let mut cursor = Cursor::new(volume.as_mut_slice());
    let mut label = [b' '; 11];
    label[..VOLUME_LABEL.len()].copy_from_slice(VOLUME_LABEL.as_bytes());
    let options = FormatVolumeOptions::new()
        .bytes_per_sector(SECTOR_SIZE as u16)
        .total_sectors((ESP_BYTES / SECTOR_SIZE) as u32)
        .bytes_per_cluster(CLUSTER_SIZE)
        .fat_type(FatType::Fat32)
        .volume_id(VOLUME_ID)
        .volume_label(label);
    fatfs::format_volume(&mut cursor, options)
        .map_err(|error| Error::io("cannot format the ESP", error))?;
    let filesystem = FileSystem::new(&mut cursor, FsOptions::new().time_provider(&DOS_EPOCH))
        .map_err(|error| Error::io("cannot mount the new ESP", error))?;
    copy_tree(&filesystem.root_dir(), tree)?;
    filesystem
        .unmount()
        .map_err(|error| Error::io("cannot unmount the new ESP", error))?;
    fix_dot_entries(&mut cursor)?;
    Ok(volume)
}

/// `fatfs` writes a long-name slot before each new directory's `.` and `..`
/// entries, and gives a `..` whose parent is the root the root's cluster.
/// FAT requires `.` and `..` as a subdirectory's first two entries, and 0 in
/// a `..` whose parent is the root; chkdsk and fsck_msdosfs repair or remove
/// a directory that breaks either rule. So this drops the two slots from
/// every subdirectory, moving its other entries up, and sets each `..`.
/// `fatfs` reads either form.
fn fix_dot_entries(volume: &mut Cursor<&mut [u8]>) -> Result<()> {
    let geometry = Geometry::read(volume)?;
    walk_subdirectories(volume, &geometry, |volume, directory| {
        let slots = directory.entries.as_chunks::<ENTRY_BYTES>().0;
        let written_by_fatfs = slots.len() >= 4
            && slots[0][11] == ATTRIBUTE_LONG_NAME
            && is_dot_entry(&slots[1], DOT, directory.cluster)
            && slots[2][11] == ATTRIBUTE_LONG_NAME
            && slots[3][..11] == DOT_DOT[..];
        if !written_by_fatfs {
            return Err(Error::new(format!(
                "fatfs did not write the ESP's {} with the expected `.` and `..` entries",
                directory.path
            )));
        }
        let entries = &mut directory.entries;
        let length = entries.len();
        entries.drain(2 * ENTRY_BYTES..3 * ENTRY_BYTES);
        entries.drain(..ENTRY_BYTES);
        entries.resize(length, END_OF_DIRECTORY);
        set_entry_cluster(&mut entries[ENTRY_BYTES..2 * ENTRY_BYTES], directory.parent);
        geometry.write_clusters(volume, &directory.chain, entries)
    })?;
    Ok(())
}

fn copy_tree<T: ReadWriteSeek>(root: &Dir<'_, T>, tree: &Tree) -> Result<()> {
    for item in &tree.items {
        match item {
            Item::Directory(path) => {
                root.create_dir(path).map_err(|error| {
                    Error::io(format!("cannot create /{path} in the ESP"), error)
                })?;
            }
            Item::File { path, source, size } => {
                let context = || format!("cannot copy {} to /{path} in the ESP", source.display());
                let mut input =
                    fs::File::open(source).map_err(|error| Error::io(context(), error))?;
                let mut output = root
                    .create_file(path)
                    .map_err(|error| Error::io(context(), error))?;
                let copied = io::copy(&mut input, &mut output)
                    .and_then(|copied| output.flush().map(|_| copied))
                    .map_err(|error| Error::io(context(), error))?;
                if copied != *size {
                    return Err(Error::new(format!(
                        "{} changed size while it was copied",
                        source.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Mounts the image's ESP read-only and checks it: FAT32 by cluster count
/// with 2 KiB clusters, the fixed volume ID and label, cleanly unmounted,
/// exactly the staged tree in sorted order with byte-identical files, and
/// `.` and `..` leading every subdirectory. Returns the cluster count.
pub fn check(image: &ImageReader, tree: &Tree) -> Result<u32> {
    let filesystem = FileSystem::new(Partition::open(image)?, FsOptions::new())
        .map_err(|error| Error::io("cannot mount the image's ESP", error))?;
    let stats = filesystem
        .stats()
        .map_err(|error| Error::io("cannot read the ESP", error))?;
    if filesystem.fat_type() != FatType::Fat32
        || stats.total_clusters() < FAT32_MIN_CLUSTERS
        || stats.cluster_size() != CLUSTER_SIZE
    {
        return Err(Error::new(format!(
            "the ESP is not FAT32 with {CLUSTER_SIZE}-byte clusters: {:?}, {} clusters of {} bytes",
            filesystem.fat_type(),
            stats.total_clusters(),
            stats.cluster_size()
        )));
    }
    if filesystem.volume_id() != VOLUME_ID || filesystem.volume_label() != VOLUME_LABEL {
        return Err(Error::new("the ESP has the wrong volume ID or label"));
    }
    let flags = filesystem
        .read_status_flags()
        .map_err(|error| Error::io("cannot read the ESP", error))?;
    if flags.dirty() || flags.io_error() {
        return Err(Error::new("the ESP is marked dirty or failed"));
    }
    let root = filesystem.root_dir();
    let mut found = Vec::new();
    list(&root, "", 0, &mut found)?;
    let expected = tree.listing();
    if found != expected {
        let position = found
            .iter()
            .zip(&expected)
            .take_while(|(found, expected)| found == expected);
        let index = position.count();
        return Err(Error::new(format!(
            "the ESP does not list the staged tree in sorted order: at entry {index} it has {:?} where the tree has {:?}",
            found.get(index),
            expected.get(index)
        )));
    }
    for item in &tree.items {
        let Item::File { path, source, .. } = item else {
            continue;
        };
        let context = || {
            format!(
                "cannot compare /{path} in the ESP with {}",
                source.display()
            )
        };
        let staged = fs::File::open(source).map_err(|error| Error::io(context(), error))?;
        let stored = root
            .open_file(path)
            .map_err(|error| Error::io(context(), error))?;
        if !same_bytes(staged, stored).map_err(|error| Error::io(context(), error))? {
            return Err(Error::new(format!(
                "/{path} in the ESP differs from {}",
                source.display()
            )));
        }
    }
    // The listing matched, so this walk covers exactly the staged directories.
    let directories = check_dot_entries(&mut Partition::open(image)?)?;
    let staged = tree
        .items
        .iter()
        .filter(|item| matches!(item, Item::Directory(_)))
        .count();
    if directories != staged {
        return Err(Error::new(format!(
            "the ESP's directory entries reach {directories} subdirectories, not the {staged} staged"
        )));
    }
    Ok(stats.total_clusters())
}

/// Checks every subdirectory's first two entries raw, because `fatfs` finds
/// `.` and `..` wherever they are: `.` with the directory's own cluster, then
/// `..` with its parent's, or 0 when the parent is the root. Returns the
/// number of subdirectories.
fn check_dot_entries(volume: &mut Partition) -> Result<usize> {
    let geometry = Geometry::read(volume)?;
    walk_subdirectories(volume, &geometry, |_, directory| {
        let slots = directory.entries.as_chunks::<ENTRY_BYTES>().0;
        if slots.len() < 2
            || !is_dot_entry(&slots[0], DOT, directory.cluster)
            || !is_dot_entry(&slots[1], DOT_DOT, directory.parent)
        {
            return Err(Error::new(format!(
                "the ESP's {} does not start with `.` and `..` entries for itself and its parent",
                directory.path
            )));
        }
        Ok(())
    })
}

/// Where a FAT32 volume keeps its FAT and its clusters, from its boot sector:
/// enough to follow cluster chains without `fatfs`.
struct Geometry {
    fat_offset: u64,
    data_offset: u64,
    /// One past the highest cluster number.
    cluster_end: u64,
    root_cluster: u32,
}

impl Geometry {
    fn read(volume: &mut (impl Read + Seek)) -> Result<Self> {
        let mut boot = [0; SECTOR_SIZE as usize];
        read_at(volume, 0, &mut boot)?;
        let sector_size = u64::from(u16_at(&boot, 11));
        let sectors_per_cluster = u64::from(boot[13]);
        let reserved_sectors = u64::from(u16_at(&boot, 14));
        let fat_count = u64::from(boot[16]);
        let fat_sectors = u64::from(u32_at(&boot, 36));
        let total_sectors = u64::from(u32_at(&boot, 32));
        let metadata_sectors = reserved_sectors + fat_count * fat_sectors;
        if sector_size != SECTOR_SIZE
            || sectors_per_cluster * sector_size != u64::from(CLUSTER_SIZE)
            || total_sectors * SECTOR_SIZE != ESP_BYTES
            || fat_count == 0
            || metadata_sectors >= total_sectors
        {
            return Err(Error::new("the ESP's boot sector has the wrong geometry"));
        }
        let cluster_end = (total_sectors - metadata_sectors) / sectors_per_cluster + 2;
        let root_cluster = u32_at(&boot, 44);
        // Each FAT holds a 4-byte entry for every cluster.
        if cluster_end * 4 > fat_sectors * SECTOR_SIZE
            || !(2..cluster_end).contains(&u64::from(root_cluster))
        {
            return Err(Error::new(
                "the ESP's FAT is too small or its root cluster is out of range",
            ));
        }
        Ok(Self {
            fat_offset: reserved_sectors * SECTOR_SIZE,
            data_offset: metadata_sectors * SECTOR_SIZE,
            cluster_end,
            root_cluster,
        })
    }

    /// The cluster chain from `first`, through the first FAT. Fails on a
    /// cluster out of range or a chain longer than the cluster count.
    fn chain(&self, volume: &mut (impl Read + Seek), first: u32) -> Result<Vec<u32>> {
        let mut chain = Vec::new();
        let mut cluster = first;
        loop {
            if !(2..self.cluster_end).contains(&u64::from(cluster))
                || chain.len() as u64 >= self.cluster_end
            {
                return Err(Error::new(format!(
                    "the ESP's cluster chain from cluster {first} is broken"
                )));
            }
            chain.push(cluster);
            let mut entry = [0; 4];
            read_at(volume, self.fat_offset + u64::from(cluster) * 4, &mut entry)?;
            let next = u32::from_le_bytes(entry) & 0x0fff_ffff;
            if next >= END_OF_CHAIN {
                return Ok(chain);
            }
            cluster = next;
        }
    }

    /// The byte offset of a cluster `chain` returned.
    fn cluster_offset(&self, cluster: u32) -> u64 {
        self.data_offset + u64::from(cluster - 2) * u64::from(CLUSTER_SIZE)
    }

    fn read_clusters(&self, volume: &mut (impl Read + Seek), chain: &[u32]) -> Result<Vec<u8>> {
        let mut bytes = vec![0; chain.len() * CLUSTER_SIZE as usize];
        for (cluster, chunk) in chain.iter().zip(bytes.chunks_mut(CLUSTER_SIZE as usize)) {
            read_at(volume, self.cluster_offset(*cluster), chunk)?;
        }
        Ok(bytes)
    }

    fn write_clusters(
        &self,
        volume: &mut (impl Write + Seek),
        chain: &[u32],
        bytes: &[u8],
    ) -> Result<()> {
        for (cluster, chunk) in chain.iter().zip(bytes.chunks(CLUSTER_SIZE as usize)) {
            volume
                .seek(SeekFrom::Start(self.cluster_offset(*cluster)))
                .and_then(|_| volume.write_all(chunk))
                .map_err(|error| Error::io("cannot write the ESP", error))?;
        }
        Ok(())
    }
}

/// A subdirectory a raw walk reached: its path by short names, its cluster
/// chain and entries, and the cluster its `..` must hold, 0 for the root.
struct Subdirectory {
    path: String,
    cluster: u32,
    parent: u32,
    chain: Vec<u32>,
    entries: Vec<u8>,
}

/// Visits every subdirectory below the root, each before its children, and
/// returns how many there are. The children come from the entries as
/// `visit` leaves them. Fails on a directory nested too deeply or reached
/// twice.
fn walk_subdirectories<V: Read + Seek>(
    volume: &mut V,
    geometry: &Geometry,
    mut visit: impl FnMut(&mut V, &mut Subdirectory) -> Result<()>,
) -> Result<usize> {
    let root = geometry.chain(volume, geometry.root_cluster)?;
    let root_entries = geometry.read_clusters(volume, &root)?;
    let mut pending: Vec<_> = subdirectories(&root_entries)
        .into_iter()
        .map(|(name, cluster)| (format!("/{name}"), cluster, 0, 0))
        .collect();
    let mut seen = HashSet::from([geometry.root_cluster]);
    while let Some((path, cluster, parent, depth)) = pending.pop() {
        if depth > MAX_DEPTH || !seen.insert(cluster) {
            return Err(Error::new(format!(
                "the ESP's {path} is nested too deeply or shares its cluster"
            )));
        }
        let chain = geometry.chain(volume, cluster)?;
        let entries = geometry.read_clusters(volume, &chain)?;
        let mut directory = Subdirectory {
            path,
            cluster,
            parent,
            chain,
            entries,
        };
        visit(volume, &mut directory)?;
        for (name, child) in subdirectories(&directory.entries) {
            pending.push((
                format!("{}/{name}", directory.path),
                child,
                cluster,
                depth + 1,
            ));
        }
    }
    Ok(seen.len() - 1)
}

/// The subdirectories that a directory's entries list before the end
/// marker, by short name and first cluster, without `.` and `..`.
fn subdirectories(entries: &[u8]) -> Vec<(String, u32)> {
    let mut found = Vec::new();
    for entry in entries.as_chunks::<ENTRY_BYTES>().0 {
        match entry[0] {
            END_OF_DIRECTORY => break,
            DELETED_ENTRY => continue,
            _ => {}
        }
        // Long-name slots carry the volume ID bit, like the label itself.
        if entry[11] & (ATTRIBUTE_VOLUME_ID | ATTRIBUTE_DIRECTORY) != ATTRIBUTE_DIRECTORY
            || entry[..11] == DOT[..]
            || entry[..11] == DOT_DOT[..]
        {
            continue;
        }
        let base = String::from_utf8_lossy(&entry[..8]).trim_end().to_owned();
        let extension = String::from_utf8_lossy(&entry[8..11]).trim_end().to_owned();
        let name = if extension.is_empty() {
            base
        } else {
            format!("{base}.{extension}")
        };
        found.push((name, entry_cluster(entry)));
    }
    found
}

/// Whether `entry` is the short directory entry `name` holding `cluster`.
fn is_dot_entry(entry: &[u8], name: &[u8; 11], cluster: u32) -> bool {
    entry[..11] == name[..] && entry[11] == ATTRIBUTE_DIRECTORY && entry_cluster(entry) == cluster
}

/// A short entry's first cluster, from its high and low 16-bit halves.
fn entry_cluster(entry: &[u8]) -> u32 {
    (u32::from(u16_at(entry, 20)) << 16) | u32::from(u16_at(entry, 26))
}

fn set_entry_cluster(entry: &mut [u8], cluster: u32) {
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
}

fn read_at(volume: &mut (impl Read + Seek), offset: u64, buffer: &mut [u8]) -> Result<()> {
    volume
        .seek(SeekFrom::Start(offset))
        .and_then(|_| volume.read_exact(buffer))
        .map_err(|error| Error::io("cannot read the ESP", error))
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

fn list<T: ReadWriteSeek>(
    directory: &Dir<'_, T>,
    prefix: &str,
    depth: usize,
    found: &mut Vec<(String, Option<u64>)>,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::new(format!(
            "the ESP's /{prefix} is nested too deeply"
        )));
    }
    for entry in directory.iter() {
        let entry = entry.map_err(|error| Error::io("cannot read the ESP", error))?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.is_dir() {
            found.push((path.clone(), None));
            list(&entry.to_dir(), &path, depth + 1, found)?;
        } else {
            found.push((path, Some(entry.len())));
        }
    }
    Ok(())
}

fn same_bytes(mut first: impl Read, mut second: impl Read) -> io::Result<bool> {
    let mut first_chunk = vec![0; COMPARE_CHUNK];
    let mut second_chunk = vec![0; COMPARE_CHUNK];
    loop {
        let first_length = read_chunk(&mut first, &mut first_chunk)?;
        let second_length = read_chunk(&mut second, &mut second_chunk)?;
        if first_chunk[..first_length] != second_chunk[..second_length] {
            return Ok(false);
        }
        if first_length == 0 {
            return Ok(true);
        }
    }
}

/// Fills `buffer` unless the reader ends first, and returns the bytes read.
fn read_chunk(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 => break,
            count => filled += count,
        }
    }
    Ok(filled)
}

/// A read-only window onto one partition of the image file. Writes fail, so
/// a mount that tried to change the image would fail instead.
struct Partition {
    file: fs::File,
    start: u64,
    length: u64,
    position: u64,
}

impl Partition {
    /// The image's ESP, through its own handle to the image file.
    fn open(image: &ImageReader) -> Result<Self> {
        let file = image
            .file
            .try_clone()
            .map_err(|error| Error::io(format!("cannot read {}", image.path.display()), error))?;
        Ok(Self {
            file,
            start: ESP_OFFSET,
            length: ESP_BYTES,
            position: 0,
        })
    }
}

impl Read for Partition {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = self.length - self.position;
        let length = (buffer.len() as u64).min(available) as usize;
        if length == 0 {
            return Ok(0);
        }
        self.file
            .seek(SeekFrom::Start(self.start + self.position))?;
        let count = self.file.read(&mut buffer[..length])?;
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for Partition {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let position = match target {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self.length.checked_add_signed(delta),
        };
        match position {
            Some(position) if position <= self.length => {
                self.position = position;
                Ok(position)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek outside the partition",
            )),
        }
    }
}

impl Write for Partition {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "verify never writes the image",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
