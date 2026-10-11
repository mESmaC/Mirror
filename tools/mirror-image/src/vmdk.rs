//! The `monolithicFlat` VMDK descriptor of decision 0015-D9: VMware's text
//! header with a fixed CID, one flat extent over the raw image, and a disk
//! database holding the hardware version and the IDE geometry that VMware
//! also uses for SATA disks. It never carries `ddb.uuid.image`,
//! `ddb.iofilters` or `ddb.sidecars`.

use crate::{DISK_SECTORS, Error, Result};
use std::path::Path;

/// Fixed, so a snapshot chain over the disk stays valid across rebuilds
/// (0015-D9). The value is arbitrary: "MIRR" in ASCII.
pub const CID: u32 = 0x4d49_5252;
/// The oldest virtual hardware version, which every VMware release accepts.
pub const VIRTUAL_HW_VERSION: u32 = 4;
const HEADS: u64 = 16;
const SECTORS_PER_TRACK: u64 = 63;
const MAX_IDE_CYLINDERS: u64 = 16_383;

/// The image's file name, which the extent line quotes. It must be printable
/// ASCII without a double quote.
pub fn image_file_name(image: &Path) -> Result<String> {
    let name = image
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::new(format!("{} does not name a file", image.display())))?;
    check_name(name)?;
    Ok(name.to_string())
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| (b' '..=b'~').contains(&byte) && byte != b'"')
    {
        return Err(Error::new(format!(
            "the image name '{name}' must be printable ASCII without a double quote"
        )));
    }
    Ok(())
}

pub fn descriptor(image_name: &str) -> Result<String> {
    check_name(image_name)?;
    let cylinders = (DISK_SECTORS / (HEADS * SECTORS_PER_TRACK)).min(MAX_IDE_CYLINDERS);
    Ok(format!(
        "# Disk DescriptorFile\n\
         version=1\n\
         CID={CID:08x}\n\
         parentCID=ffffffff\n\
         createType=\"monolithicFlat\"\n\
         \n\
         # Extent description\n\
         RW {DISK_SECTORS} FLAT \"{image_name}\" 0\n\
         \n\
         # The Disk Data Base\n\
         #DDB\n\
         \n\
         ddb.adapterType = \"ide\"\n\
         ddb.geometry.cylinders = \"{cylinders}\"\n\
         ddb.geometry.heads = \"{HEADS}\"\n\
         ddb.geometry.sectors = \"{SECTORS_PER_TRACK}\"\n\
         ddb.virtualHWVersion = \"{VIRTUAL_HW_VERSION}\"\n"
    ))
}
