//! Bounded host controllers for Mirror's opt-in lane (0015c).
//! Host fixtures do not enable VMware operations or prove D11 completion.

pub mod bounds;
pub mod frame;
pub mod lease;
#[cfg(windows)]
pub mod native_package;
pub mod package;
pub mod paths;
pub mod record;
pub mod rfb;
pub mod sha256;
pub mod staging;
pub mod timing;
pub mod vm;
#[cfg(windows)]
pub mod windows_job;
