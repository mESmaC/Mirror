//! Outputs written under a temporary name and renamed only when complete, so
//! a failed run never leaves a partial artifact.

use crate::{Error, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Pending {
    file: Option<fs::File>,
    temporary: PathBuf,
    destination: PathBuf,
    committed: bool,
}

impl Pending {
    /// Creates `<destination>.tmp`, replacing a leftover one.
    pub fn create(destination: &Path) -> Result<Self> {
        let mut name = OsString::from(destination.file_name().ok_or_else(|| {
            Error::new(format!("{} does not name a file", destination.display()))
        })?);
        name.push(".tmp");
        let temporary = destination.with_file_name(name);
        let file = fs::File::create(&temporary)
            .map_err(|error| Error::io(format!("cannot create {}", temporary.display()), error))?;
        Ok(Self {
            file: Some(file),
            temporary,
            destination: destination.to_path_buf(),
            committed: false,
        })
    }

    pub fn temporary(&self) -> &Path {
        &self.temporary
    }

    pub fn file(&mut self) -> &mut fs::File {
        self.file
            .as_mut()
            .expect("a pending output keeps its file until it is committed")
    }

    /// Closes the file and renames it over the destination.
    pub fn commit(mut self) -> Result<()> {
        drop(self.file.take());
        fs::rename(&self.temporary, &self.destination).map_err(|error| {
            Error::io(
                format!(
                    "cannot rename {} to {}",
                    self.temporary.display(),
                    self.destination.display()
                ),
                error,
            )
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Pending {
    /// Removes the temporary file of an output that was not committed,
    /// including one whose rename failed.
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.committed {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}
