//! Limine owns page tables and the bootstrap stack during this first milestone.
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
