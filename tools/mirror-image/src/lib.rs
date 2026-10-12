#![forbid(unsafe_code)]
//! Mirror's disk image writer (spec 0015a). `write_disk` turns a staged ESP
//! tree into the 1 GiB raw GPT disk of decision 0015-D5, with a FAT32 EFI
//! system partition and the minimal ext2 root of 0015-D6, plus the
//! `monolithicFlat` VMDK descriptor of 0015-D9. `verify` reads an image back
//! through its own structures and compares it with the staged tree.
//!
//! The output is byte-reproducible: every identifier and timestamp is fixed
//! and directories are written in sorted order. Each output is written to a
//! temporary name and renamed only once all of them are complete.

mod crc32;
mod esp;
mod ext2;
mod gpt;
mod output;
mod vmdk;

use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SECTOR_SIZE: u64 = 512;
/// 0015-D5: a 1 GiB disk.
pub const DISK_SECTORS: u64 = 2_097_152;
pub const DISK_BYTES: u64 = DISK_SECTORS * SECTOR_SIZE;
/// Partition 1, `MIRROR-ESP`: 256 MiB at 1 MiB.
pub const ESP_FIRST_LBA: u64 = 2048;
pub const ESP_LAST_LBA: u64 = 526_335;
/// Partition 2, `mirror-root`: 766 MiB after the ESP.
pub const ROOT_FIRST_LBA: u64 = 526_336;
pub const ROOT_LAST_LBA: u64 = 2_095_103;
pub const ESP_OFFSET: u64 = ESP_FIRST_LBA * SECTOR_SIZE;
pub const ESP_BYTES: u64 = (ESP_LAST_LBA + 1 - ESP_FIRST_LBA) * SECTOR_SIZE;
pub const ROOT_OFFSET: u64 = ROOT_FIRST_LBA * SECTOR_SIZE;
pub const ROOT_BYTES: u64 = (ROOT_LAST_LBA + 1 - ROOT_FIRST_LBA) * SECTOR_SIZE;

const _: () = assert!(ESP_BYTES == 256 * 1024 * 1024);
const _: () = assert!(ROOT_BYTES == 766 * 1024 * 1024);
const _: () = assert!(ESP_FIRST_LBA > gpt::LAST_PRIMARY_LBA);
const _: () = assert!(ROOT_LAST_LBA <= gpt::LAST_USABLE_LBA);

#[derive(Debug)]
pub struct Error(String);

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn io(context: impl fmt::Display, error: io::Error) -> Self {
        Self(format!("{context}: {error}"))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// What `write_disk` wrote.
#[derive(Debug)]
pub struct DiskSummary {
    pub esp_files: usize,
    pub esp_content_bytes: u64,
    pub descriptor_bytes: usize,
}

/// What `verify` checked.
#[derive(Debug)]
pub struct VerifySummary {
    pub esp_files: usize,
    pub esp_content_bytes: u64,
    pub esp_clusters: u32,
    pub root_groups: u32,
}

/// Writes the disk image to `image` and its VMDK descriptor to `vmdk`. The
/// descriptor names the image by file name, so both must be in the same
/// directory. Fails before creating any output if the ESP tree is invalid or
/// does not fit the ESP, and removes its temporary files on any failure.
pub fn write_disk(esp_tree: &Path, image: &Path, vmdk: &Path) -> Result<DiskSummary> {
    write_disk_with_perf(esp_tree, image, vmdk, false)
}

/// Like `write_disk`, with the dedicated performance CID when `perf` is true.
/// Only the descriptor identity changes; the staged tree defines the image.
pub fn write_disk_with_perf(
    esp_tree: &Path,
    image: &Path,
    vmdk: &Path,
    perf: bool,
) -> Result<DiskSummary> {
    let image_name = vmdk::image_file_name(image)?;
    if !same_directory(image, vmdk)? {
        return Err(Error::new(format!(
            "{} and {} must be in the same directory, because the descriptor names the image by file name",
            image.display(),
            vmdk.display()
        )));
    }
    // Windows compares file names without case.
    if vmdk
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(&image_name))
    {
        return Err(Error::new(
            "the image and the descriptor need different names",
        ));
    }
    let tree = esp::collect(esp_tree)?;
    let esp_volume = esp::build(&tree)?;
    let root = ext2::format()?;
    let tables = gpt::tables();
    let descriptor = vmdk::descriptor(&image_name, perf)?;
    vmdk::validate(&descriptor, &image_name, perf)?;

    let mut image_output = output::Pending::create(image)?;
    let image_context = format!("cannot write {}", image_output.temporary().display());
    let file = image_output.file();
    file.set_len(DISK_BYTES)
        .map_err(|error| Error::io(&image_context, error))?;
    let mut writes: Vec<(u64, &[u8])> = vec![
        (0, &tables.mbr),
        (SECTOR_SIZE, &tables.primary_header),
        (gpt::PRIMARY_ENTRIES_LBA * SECTOR_SIZE, &tables.entries),
        (ESP_OFFSET, &esp_volume),
    ];
    writes.extend(
        root.iter()
            .map(|write| (ROOT_OFFSET + write.offset, write.bytes.as_slice())),
    );
    writes.push((gpt::BACKUP_ENTRIES_LBA * SECTOR_SIZE, &tables.entries));
    writes.push((gpt::BACKUP_HEADER_LBA * SECTOR_SIZE, &tables.backup_header));
    // Ascending order, so the file grows front to back once.
    debug_assert!(
        writes
            .windows(2)
            .all(|pair| pair[0].0 + pair[0].1.len() as u64 <= pair[1].0)
    );
    for (offset, bytes) in writes {
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.write_all(bytes))
            .map_err(|error| Error::io(&image_context, error))?;
    }
    file.flush()
        .map_err(|error| Error::io(&image_context, error))?;

    let mut vmdk_output = output::Pending::create(vmdk)?;
    let vmdk_context = format!("cannot write {}", vmdk_output.temporary().display());
    vmdk_output
        .file()
        .write_all(descriptor.as_bytes())
        .and_then(|_| vmdk_output.file().flush())
        .map_err(|error| Error::io(&vmdk_context, error))?;
    let written_descriptor = fs::read_to_string(vmdk_output.temporary())
        .map_err(|error| Error::io("cannot read back the VMDK descriptor", error))?;
    vmdk::validate(&written_descriptor, &image_name, perf)?;

    image_output.commit()?;
    vmdk_output.commit()?;
    Ok(DiskSummary {
        esp_files: tree.file_count(),
        esp_content_bytes: tree.content_bytes(),
        descriptor_bytes: descriptor.len(),
    })
}

/// Checks an image `write_disk` wrote from the staged tree `esp_tree`: both
/// GPT headers, their CRCs and the D5 partitions; every staged file read back
/// through the ESP's own FAT32 structures, in sorted order with nothing extra,
/// and `.` and `..` leading every ESP subdirectory; and the ext2 superblocks,
/// descriptors, bitmaps, directories and marker.
pub fn verify(esp_tree: &Path, image: &Path) -> Result<VerifySummary> {
    let tree = esp::collect(esp_tree)?;
    let mut reader = ImageReader::open(image)?;
    gpt::check(&mut reader)?;
    let esp_clusters = esp::check(&reader, &tree)?;
    let root_groups = ext2::check(&mut reader)?;
    Ok(VerifySummary {
        esp_files: tree.file_count(),
        esp_content_bytes: tree.content_bytes(),
        esp_clusters,
        root_groups,
    })
}

/// Whether both paths are in the same directory.
fn same_directory(first: &Path, second: &Path) -> Result<bool> {
    let parent = |path: &Path| -> Result<PathBuf> {
        let directory = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        fs::canonicalize(directory)
            .map_err(|error| Error::io(format!("cannot open {}", directory.display()), error))
    };
    Ok(parent(first)? == parent(second)?)
}

/// Bounded reads from an image file of exactly `DISK_BYTES`.
struct ImageReader {
    file: fs::File,
    path: PathBuf,
}

impl ImageReader {
    fn open(path: &Path) -> Result<Self> {
        let context = format!("cannot read {}", path.display());
        let file = fs::File::open(path).map_err(|error| Error::io(&context, error))?;
        let length = file
            .metadata()
            .map_err(|error| Error::io(&context, error))?
            .len();
        if length != DISK_BYTES {
            return Err(Error::new(format!(
                "{} is {length} bytes, not the {DISK_BYTES}-byte D5 disk",
                path.display()
            )));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    fn read_at(&mut self, offset: u64, length: usize) -> Result<Vec<u8>> {
        if offset
            .checked_add(length as u64)
            .is_none_or(|end| end > DISK_BYTES)
        {
            return Err(Error::new(format!(
                "read past the end of {}",
                self.path.display()
            )));
        }
        let mut bytes = vec![0; length];
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_exact(&mut bytes))
            .map_err(|error| Error::io(format!("cannot read {}", self.path.display()), error))?;
        Ok(bytes)
    }

    fn read_sectors(&mut self, lba: u64, count: u64) -> Result<Vec<u8>> {
        self.read_at(lba * SECTOR_SIZE, (count * SECTOR_SIZE) as usize)
    }
}

#[cfg(test)]
mod tests;
