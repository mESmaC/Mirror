//! Limine provides the initial page tables and bootstrap stack.
//! Keep all bootloader-reclaimable regions reserved while using these responses.

use limine::{
    BaseRevision,
    request::{
        HhdmRequest, MemoryMapRequest, RequestsEndMarker, RequestsStartMarker, StackSizeRequest,
    },
    response::MemoryMapResponse,
};

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE: BaseRevision = BaseRevision::with_revision(3);

#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMORY_MAP: MemoryMapRequest = MemoryMapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM: HhdmRequest = HhdmRequest::new();

#[cfg(feature = "boot-smoke")]
#[used]
#[unsafe(link_section = ".limine_requests")]
static CMDLINE: limine::request::ExecutableCmdlineRequest =
    limine::request::ExecutableCmdlineRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static STACK: StackSizeRequest = StackSizeRequest::new().with_size(256 * 1024);

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static END: RequestsEndMarker = RequestsEndMarker::new();

pub struct BootInfo {
    pub memory_map: &'static MemoryMapResponse,
    pub direct_map_offset: u64,
}

pub fn info() -> BootInfo {
    assert!(BASE.is_supported(), "unsupported Limine base revision");
    BootInfo {
        memory_map: MEMORY_MAP.get_response().expect("missing memory map"),
        direct_map_offset: HHDM.get_response().expect("missing direct map").offset(),
    }
}

#[cfg(feature = "boot-smoke")]
pub enum SmokeMode {
    Normal,
    PageFault,
    InvalidOpcode,
}

#[cfg(feature = "boot-smoke")]
pub fn smoke_mode() -> SmokeMode {
    let cmdline = CMDLINE
        .get_response()
        .expect("missing test command line")
        .cmdline();
    match cmdline.to_bytes() {
        b"" => SmokeMode::Normal,
        b"mirror.test=page-fault" => SmokeMode::PageFault,
        b"mirror.test=invalid-opcode" => SmokeMode::InvalidOpcode,
        _ => panic!("unknown smoke-test command line"),
    }
}
