//! The `monolithicFlat` VMDK descriptor of decision 0015-D9: VMware's text
//! header with a fixed CID, one flat extent over the raw image, and a disk
//! database holding the hardware version and the IDE geometry that VMware
//! also uses for SATA disks. It never carries `ddb.uuid.image`,
//! `ddb.iofilters` or `ddb.sidecars`.

use crate::{DISK_SECTORS, Error, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Fixed, so a snapshot chain over the disk stays valid across rebuilds
/// (0015-D9). The value is arbitrary: "MIRR" in ASCII.
pub const CID: u32 = 0x4d49_5252;
/// Performance disks form a separate snapshot chain (0019-D12): "PERF".
pub const PERF_CID: u32 = 0x5045_5246;
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

pub fn descriptor(image_name: &str, perf: bool) -> Result<String> {
    check_name(image_name)?;
    let cid = if perf { PERF_CID } else { CID };
    let cylinders = (DISK_SECTORS / (HEADS * SECTORS_PER_TRACK)).min(MAX_IDE_CYLINDERS);
    Ok(format!(
        "# Disk DescriptorFile\n\
         version=1\n\
         CID={cid:08x}\n\
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

/// Validate the complete closed descriptor schema, including the selected
/// snapshot-chain identity. Unknown and repeated fields or extents fail.
pub fn validate(descriptor: &str, image_name: &str, perf: bool) -> Result<()> {
    check_name(image_name)?;
    if !descriptor.is_ascii() || descriptor.len() > 4096 {
        return Err(Error::new("invalid VMDK descriptor encoding or length"));
    }
    let mut fields = BTreeMap::new();
    let mut extent = None;
    for line in descriptor.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("RW ") {
            if extent.replace(line).is_some() {
                return Err(Error::new("extra VMDK extent"));
            }
        } else if let Some((key, value)) = line.split_once('=') {
            if fields.insert(key.trim(), value.trim()).is_some() {
                return Err(Error::new("duplicate VMDK descriptor field"));
            }
        } else {
            return Err(Error::new("unrecognized VMDK descriptor line"));
        }
    }
    let expected_cid = if perf { "50455246" } else { "4d495252" };
    let expected_fields = BTreeMap::from([
        ("version", "1"),
        ("CID", expected_cid),
        ("parentCID", "ffffffff"),
        ("createType", "\"monolithicFlat\""),
        ("ddb.adapterType", "\"ide\""),
        ("ddb.geometry.cylinders", "\"2080\""),
        ("ddb.geometry.heads", "\"16\""),
        ("ddb.geometry.sectors", "\"63\""),
        ("ddb.virtualHWVersion", "\"4\""),
    ]);
    let expected_extent = format!("RW {DISK_SECTORS} FLAT \"{image_name}\" 0");
    if fields != expected_fields || extent != Some(expected_extent.as_str()) {
        return Err(Error::new(
            "VMDK descriptor does not match the complete disk schema",
        ));
    }
    Ok(())
}
