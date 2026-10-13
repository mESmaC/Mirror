//! Windows directory operations anchored to an open identity, never a reusable pathname.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub volume: u32,
    pub index: u64,
}

impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:08x}:{:016x}", self.volume, self.index)
    }
}

#[repr(C)]
#[derive(Default)]
struct Information {
    attributes: u32,
    creation: [u32; 2],
    access: [u32; 2],
    write: [u32; 2],
    volume: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}

#[repr(C)]
struct RenameHeader {
    replace: u32,
    root: *mut c_void,
    name_len: u32,
    name: [u16; 1],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandle(handle: *mut c_void, information: *mut Information) -> i32;
    fn SetFileInformationByHandle(
        handle: *mut c_void,
        class: u32,
        information: *const c_void,
        size: u32,
    ) -> i32;
}

pub fn identity(file: &File) -> io::Result<Identity> {
    let mut info = Information::default();
    // SAFETY: File owns a valid handle and info is writable storage with the documented layout.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.attributes & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reparse identity rejected",
        ));
    }
    Ok(Identity {
        volume: info.volume,
        index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
    })
}

/// DELETE access plus no FILE_SHARE_DELETE prevents competing directory detachers.
pub fn directory(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(0x10000 | 0x80)
        .share_mode(1 | 2)
        .custom_flags(0x0200_0000 | 0x0020_0000)
        .open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "lease is not a directory",
        ));
    }
    identity(&file)?;
    Ok(file)
}

pub fn owner(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create)
        .share_mode(1)
        .custom_flags(0x0020_0000)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owner is not a regular file",
        ));
    }
    identity(&file)?;
    Ok(file)
}

/// Lock the exact detached owner against writes and replacement while retaining
/// DELETE access for identity-bound, non-recursive cleanup.
pub fn cleanup_owner(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(0x8000_0000 | 0x10000) // GENERIC_READ | DELETE
        .share_mode(1)
        .custom_flags(0x0020_0000)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "detached owner is not a regular file",
        ));
    }
    identity(&file)?;
    Ok(file)
}

/// Delete only this held object on close. For a directory Windows requires it to
/// be empty; no pathname lookup, recursive traversal or readonly override occurs.
pub fn delete(file: &File) -> io::Result<()> {
    let delete_file = 1u8; // FILE_DISPOSITION_INFO's BOOLEAN DeleteFile.
    // SAFETY: The caller owns a DELETE-capable File handle. FileDispositionInfo=4
    // takes a one-byte BOOLEAN that remains live for this synchronous call.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            4,
            (&delete_file as *const u8).cast(),
            1,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Verify that the directory guarded after opening the owner still contains that
/// exact file. This closes the owner-first acquisition/path-replacement interval.
pub fn verify_owner(file: &File, path: &Path) -> io::Result<()> {
    let observed = OpenOptions::new()
        .read(true)
        .share_mode(1 | 2 | 4)
        .custom_flags(0x0020_0000)
        .open(path)?;
    if identity(file)? != identity(&observed)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "owner pathname changed during acquisition",
        ));
    }
    Ok(())
}

/// Rename the exact opened generation. A preexisting destination is never replaced.
pub fn rename(directory: &File, target: &Path) -> io::Result<()> {
    // Canonical Windows extended paths also avoid drive-relative interpretation by
    // the handle API. Validate the existing parent before constructing a new name.
    let target = crate::paths::resolve_no_reparse(target)?;
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in rename path",
        ));
    }
    let name_bytes = name
        .len()
        .checked_mul(2)
        .ok_or_else(|| io::Error::other("rename path too long"))?;
    let offset = std::mem::offset_of!(RenameHeader, name);
    let size = offset
        .checked_add(name_bytes)
        .and_then(|size| size.checked_add(2))
        .ok_or_else(|| io::Error::other("rename buffer too long"))?;
    let size32 = u32::try_from(size).map_err(|_| io::Error::other("rename buffer too long"))?;
    let name32 = u32::try_from(name_bytes).map_err(|_| io::Error::other("rename path too long"))?;
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let pointer = buffer.as_mut_ptr().cast::<u8>();
    // SAFETY: The u64 buffer provides sufficient size/alignment for all header fields and
    // the trailing UTF-16 bytes INCLUDING the documented NUL terminator. NameLength
    // excludes that terminator; zero initialization also supplies no-replace and NULL root.
    unsafe {
        pointer
            .add(std::mem::offset_of!(RenameHeader, name_len))
            .cast::<u32>()
            .write(name32);
        std::ptr::copy_nonoverlapping(name.as_ptr(), pointer.add(offset).cast::<u16>(), name.len());
    }
    // SAFETY: Directory owns a DELETE-capable handle. The counted FILE_RENAME_INFO buffer
    // remains live for the synchronous call, with FileRenameInfo=3 and no-replace semantics.
    if unsafe { SetFileInformationByHandle(directory.as_raw_handle(), 3, pointer.cast(), size32) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
