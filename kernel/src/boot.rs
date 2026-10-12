//! Limine provides the initial page tables and bootstrap stack.
//! Keep all bootloader-reclaimable regions reserved while using these responses.

use limine::{
    BaseRevision,
    request::{
        FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest, MpRequest,
        RequestsEndMarker, RequestsStartMarker, RsdpRequest, StackSizeRequest,
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
static RSDP: RsdpRequest = RsdpRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MP: MpRequest = MpRequest::new(); // Keep xAPIC mode; do not request x2APIC.

#[used]
#[unsafe(link_section = ".limine_requests")]
static MODULES: ModuleRequest = ModuleRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

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
    /// Limine base revision 3 returns a physical address, not an HHDM pointer.
    pub rsdp_physical: Option<u64>,
    pub processors: Option<&'static limine::response::MpResponse>,
    /// Only smoke kernels run init (spec 0010-D3). Every flavor still requires
    /// the module, so all flavors boot from the same staged ESP.
    #[cfg(feature = "boot-smoke")]
    pub init: &'static [u8],
    pub desktop: &'static [u8],
    pub app: &'static [u8],
    pub framebuffer: Option<limine::framebuffer::Framebuffer<'static>>,
}

pub fn info() -> BootInfo {
    assert!(BASE.is_supported(), "unsupported Limine base revision");
    let modules = MODULES
        .get_response()
        .expect("missing native modules")
        .modules();
    assert_eq!(modules.len(), 3, "expected three native boot modules");
    let mut init = None;
    let mut desktop = None;
    let mut app = None;
    for module in modules {
        let slot = match module.string().to_bytes() {
            b"mirror-init" => &mut init,
            b"mirror-desktop" => &mut desktop,
            b"mirror-app" => &mut app,
            _ => panic!("unexpected native boot module"),
        };
        assert!(slot.is_none(), "duplicate native boot module");
        let length = usize::try_from(module.size()).expect("native module too large");
        assert!(
            (64..=16 * 1024 * 1024).contains(&length),
            "invalid module size"
        );
        assert!(!module.addr().is_null(), "null native module");
        assert!(
            (module.addr() as usize).checked_add(length).is_some(),
            "module wraps"
        );
        // SAFETY: Limine supplies each immutable, contiguous module of the
        // reported length; reserved module pages remain live for the kernel.
        *slot = Some(unsafe { core::slice::from_raw_parts(module.addr(), length) });
    }
    #[cfg(not(feature = "boot-smoke"))]
    assert!(init.is_some(), "missing native init");
    BootInfo {
        memory_map: MEMORY_MAP.get_response().expect("missing memory map"),
        direct_map_offset: HHDM.get_response().expect("missing direct map").offset(),
        rsdp_physical: RSDP
            .get_response()
            .map(|response| response.address() as u64),
        processors: MP.get_response(),
        #[cfg(feature = "boot-smoke")]
        init: init.expect("missing native init"),
        desktop: desktop.expect("missing native desktop"),
        app: app.expect("missing native application"),
        framebuffer: FRAMEBUFFER
            .get_response()
            .and_then(|response| response.framebuffers().next()),
    }
}

/// A normal boot policy as well as a regression path: keep firmware's APIC
/// available during UEFI startup, then select Mirror's legacy PIC backend.
pub fn legacy_pic() -> bool {
    command_line()
        .split(|byte| byte.is_ascii_whitespace())
        .any(|token| token == b"mirror.irq=pic")
}

/// Keep the PIT time source while retaining APIC device routing and SMP.
pub fn pit_timer() -> bool {
    command_line()
        .split(|byte| byte.is_ascii_whitespace())
        .any(|token| token == b"mirror.timer=pit")
}

fn command_line() -> &'static [u8] {
    CMDLINE
        .get_response()
        .expect("missing kernel command line")
        .cmdline()
        .to_bytes()
}

#[cfg(feature = "boot-smoke")]
pub enum SmokeMode {
    Normal,
    PageFault,
    InvalidOpcode,
    KernelStackOverflow,
    KernelSmep,
    KernelSmap,
    BlockIoError,
    BlockWrite,
    BlockWriteError,
    BlockFlushError,
    BlockNoFlush,
    AhciIoError,
    AhciInitFailure,
    XhciInitFailure,
    XhciDeviceFailure,
    SmpUnmap,
    SmpTlbTimeout,
    TimerCalibration,
    ClockActivation,
    Ps2BatTimeout,
    Ps2StaleScancode,
    VmmouseError,
    /// Restart through `reset::restart(test)` after every self-test, in place
    /// of the expected page fault (spec 0058a, D11).
    Reset(ResetTest),
    /// The 0054-D5 scenario: a normal requester and a background victim
    /// replace the relaunch pair, and the armed frame budget drives a kill.
    OomOrder,
    User(u64),
}

/// The restart case: the method the chain starts at, every earlier one
/// skipped with `test_skip`, or the whole chain with its writes suppressed.
#[cfg(feature = "boot-smoke")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetTest {
    Fadt,
    Kbc,
    Cf9,
    TripleFault,
    Suppressed,
}

#[cfg(feature = "boot-smoke")]
pub fn smoke_mode() -> SmokeMode {
    // Interrupt policy composes with exactly one optional smoke scenario.
    // Ignore only the recognized policy token; other unknown arguments and
    // multiple fault selectors must retain the strict smoke-test rejection.
    let mut tokens = command_line()
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|token| {
            !token.is_empty() && *token != b"mirror.irq=pic" && *token != b"mirror.timer=pit"
        });
    let mode = match tokens.next().unwrap_or(b"") {
        b"" => SmokeMode::Normal,
        b"mirror.test=page-fault" => SmokeMode::PageFault,
        b"mirror.test=invalid-opcode" => SmokeMode::InvalidOpcode,
        b"mirror.test=kernel-stack-overflow" => SmokeMode::KernelStackOverflow,
        b"mirror.test=kernel-smep" => SmokeMode::KernelSmep,
        b"mirror.test=kernel-smap" => SmokeMode::KernelSmap,
        b"mirror.test=user-kernel-read" => SmokeMode::User(1),
        b"mirror.test=user-code-write" => SmokeMode::User(2),
        b"mirror.test=user-nx" => SmokeMode::User(3),
        b"mirror.test=user-guard" => SmokeMode::User(4),
        b"mirror.test=user-privileged" => SmokeMode::User(5),
        b"mirror.test=user-bad-stack" => SmokeMode::User(6),
        b"mirror.test=user-budget" => SmokeMode::User(7),
        b"mirror.test=ipc-peer-exit" => SmokeMode::User(8),
        b"mirror.test=ipc-timeout" => SmokeMode::User(9),
        b"mirror.test=vm-read-only" => SmokeMode::User(10),
        b"mirror.test=vm-nx" => SmokeMode::User(11),
        b"mirror.test=vm-released" => SmokeMode::User(12),
        b"mirror.test=vm-guard" => SmokeMode::User(13),
        b"mirror.test=user-x87" => SmokeMode::User(14),
        b"mirror.test=user-simd" => SmokeMode::User(15),
        b"mirror.test=user-avx" => SmokeMode::User(16),
        b"mirror.test=user-fsgsbase" => SmokeMode::User(17),
        b"mirror.test=thread-last-exit" => SmokeMode::User(18),
        b"mirror.test=thread-process-exit" => SmokeMode::User(19),
        b"mirror.test=thread-fault" => SmokeMode::User(20),
        b"mirror.test=user-syscall" => SmokeMode::User(21),
        b"mirror.test=user-umip" => SmokeMode::User(22),
        b"mirror.test=block-io-error" => SmokeMode::BlockIoError,
        b"mirror.test=block-write" => SmokeMode::BlockWrite,
        b"mirror.test=block-write-error" => SmokeMode::BlockWriteError,
        b"mirror.test=block-flush-error" => SmokeMode::BlockFlushError,
        b"mirror.test=block-no-flush" => SmokeMode::BlockNoFlush,
        b"mirror.test=ahci-io-error" => SmokeMode::AhciIoError,
        b"mirror.test=ahci-init-failure" => SmokeMode::AhciInitFailure,
        b"mirror.test=xhci-init-failure" => SmokeMode::XhciInitFailure,
        b"mirror.test=xhci-device-failure" => SmokeMode::XhciDeviceFailure,
        b"mirror.test=smp-unmap" => SmokeMode::SmpUnmap,
        b"mirror.test=smp-tlb-timeout" => SmokeMode::SmpTlbTimeout,
        b"mirror.test=timer-calibration" => SmokeMode::TimerCalibration,
        b"mirror.test=clock-activation" => SmokeMode::ClockActivation,
        b"mirror.test=ps2-bat-timeout" => SmokeMode::Ps2BatTimeout,
        b"mirror.test=ps2-stale-scancode" => SmokeMode::Ps2StaleScancode,
        b"mirror.test=vmmouse-error" => SmokeMode::VmmouseError,
        b"mirror.test=reset" => SmokeMode::Reset(ResetTest::Fadt),
        b"mirror.test=reset-from-kbc" => SmokeMode::Reset(ResetTest::Kbc),
        b"mirror.test=reset-from-cf9" => SmokeMode::Reset(ResetTest::Cf9),
        b"mirror.test=reset-from-triple-fault" => SmokeMode::Reset(ResetTest::TripleFault),
        b"mirror.test=reset-suppressed" => SmokeMode::Reset(ResetTest::Suppressed),
        b"mirror.test=oom-order" => SmokeMode::OomOrder,
        _ => panic!("unknown smoke-test command line"),
    };
    assert!(tokens.next().is_none(), "multiple smoke-test arguments");
    mode
}
