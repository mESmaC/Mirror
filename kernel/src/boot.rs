//! Limine provides the initial page tables and bootstrap stack.
//! Keep all bootloader-reclaimable regions reserved while using these responses.

use limine::{
    BaseRevision,
    request::{
        FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest, RequestsEndMarker,
        RequestsStartMarker, StackSizeRequest,
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
static MODULES: ModuleRequest = ModuleRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

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
    pub init: &'static [u8],
    pub framebuffer: Option<limine::framebuffer::Framebuffer<'static>>,
}

pub fn info() -> BootInfo {
    assert!(BASE.is_supported(), "unsupported Limine base revision");
    let modules = MODULES
        .get_response()
        .expect("missing native init module")
        .modules();
    assert_eq!(modules.len(), 1, "expected exactly one boot module");
    let init = modules[0];
    assert_eq!(
        init.string().to_bytes(),
        b"mirror-init",
        "unexpected boot module"
    );
    let length = usize::try_from(init.size()).expect("init module too large");
    assert!(
        (64..=16 * 1024 * 1024).contains(&length),
        "invalid init module size"
    );
    assert!(!init.addr().is_null(), "null init module");
    assert!(
        (init.addr() as usize).checked_add(length).is_some(),
        "init module wraps"
    );
    // SAFETY: Limine supplies this immutable, contiguous module of the reported
    // length; its reserved pages and boot responses remain live for the kernel.
    let init = unsafe { core::slice::from_raw_parts(init.addr(), length) };
    BootInfo {
        memory_map: MEMORY_MAP.get_response().expect("missing memory map"),
        direct_map_offset: HHDM.get_response().expect("missing direct map").offset(),
        init,
        framebuffer: FRAMEBUFFER
            .get_response()
            .and_then(|response| response.framebuffers().next()),
    }
}

#[cfg(feature = "boot-smoke")]
pub enum SmokeMode {
    Normal,
    PageFault,
    InvalidOpcode,
    BlockIoError,
    User(u64),
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
        b"mirror.test=user-kernel-read" => SmokeMode::User(1),
        b"mirror.test=user-code-write" => SmokeMode::User(2),
        b"mirror.test=user-nx" => SmokeMode::User(3),
        b"mirror.test=user-guard" => SmokeMode::User(4),
        b"mirror.test=user-privileged" => SmokeMode::User(5),
        b"mirror.test=user-bad-stack" => SmokeMode::User(6),
        b"mirror.test=user-budget" => SmokeMode::User(7),
        b"mirror.test=ipc-peer-exit" => SmokeMode::User(8),
        b"mirror.test=ipc-timeout" => SmokeMode::User(9),
        b"mirror.test=block-io-error" => SmokeMode::BlockIoError,
        _ => panic!("unknown smoke-test command line"),
    }
}
