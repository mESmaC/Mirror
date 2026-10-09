//! CPU-local setup and BSP ownership of x87/MMX and legacy SIMD register state.
//!
//! Kernel Rust remains soft-float. Every scheduled user context owns one full
//! FXSAVE64 image; the user entry/return assembly restores/saves it with IF=0.
//! XSAVE/AVX stays disabled until its additional state has a context-switch
//! contract. FS/GS bases use the checked TLS ABI; direct base instructions
//! remain disabled by the native execution policy.
//!
//! State layout and controls: Intel SDM volumes 2 (FXSAVE/FXRSTOR, FNINIT) and 3A:
//! https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
//! AMD APM volume 1, section 6.9.1.2, and volume 2, EFER.FFXSR:
//! https://docs.amd.com/api/khub/documents/sfvvekC9mDflu6vd3R0NXA/content
//! https://docs.amd.com/api/khub/documents/sD1_QL~h4Afq2_tvzxqqSQ/content

use core::arch::{
    asm,
    x86_64::{__cpuid, CpuidResult},
};
use core::mem::{align_of, size_of};
use core::ptr::addr_of;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{arch, interrupts, serial};

const CR0_MP: u64 = 1 << 1;
const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR0_NE: u64 = 1 << 5;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const CR4_FSGSBASE: u64 = 1 << 16;
const CR4_OSXSAVE: u64 = 1 << 18;
const EFER: u32 = 0xc000_0080;
const EFER_FFXSR: u64 = 1 << 14;

/// Supervisor-only, aligned state image. No user-provided image is restored:
/// reserved MXCSR bits can otherwise make FXRSTOR fault in kernel context.
#[repr(C, align(16))]
pub(crate) struct State {
    bytes: [u8; 512],
}

impl State {
    /// Empty x87 stack, round-to-nearest, masked exceptions, and zero register
    /// payloads. FNINIT alone leaves the old x87 data and SIMD registers intact.
    pub(crate) const fn new() -> Self {
        let mut bytes = [0; 512];
        bytes[0] = 0x7f;
        bytes[1] = 0x03; // FCW = 0x037f.
        bytes[24] = 0x80;
        bytes[25] = 0x1f; // MXCSR = 0x1f80; DAZ/FTZ disabled.
        Self { bytes }
    }
}

const _: () = assert!(size_of::<State>() == 512);
const _: () = assert!(align_of::<State>() == 16);

// Read-only supervisor mapping, also referenced directly by userspace_entry.S.
#[unsafe(no_mangle)]
static MIRROR_FPU_CLEAN: State = State::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub(crate) fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

/// Establish the boot CPU's extended-state contract before any user entry.
/// The installed exception gates handle #MF (16) and #XM (19); unmasked user
/// exceptions terminate their process through the ordinary fault path.
pub(crate) fn init() {
    assert!(interrupts::is_initialized(), "FPU setup requires the IDT");
    assert!(!arch::interrupts_enabled(), "FPU setup requires IF=0");
    assert!(!is_initialized(), "FPU already initialized");
    let cpuid_avx = configure_current().expect("FPU CPU setup failed");
    INITIALIZED.store(true, Ordering::Release);
    serial::println!(
        "MIRROR_FPU_READY format=fxsave64 bytes=512 xmm=16 eager=1 avx=0 cpuid_avx={} osxsave=0",
        u8::from(cpuid_avx)
    );
}

/// Establish the same control and clean-register contract on this secondary.
/// The caller has installed its private exception descriptors with IF clear.
/// No global publication, logging, allocation or BSP panic path is used here.
pub(crate) fn init_secondary() -> Result<(), &'static str> {
    if !INITIALIZED.load(Ordering::Acquire) {
        return Err("secondary FPU setup requires BSP initialization");
    }
    configure_current().map(|_| ())
}

/// Configure only the executing CPU. Return AVX hardware availability for the
/// BSP's diagnostic; OSXSAVE remains disabled regardless of that capability.
fn configure_current() -> Result<bool, &'static str> {
    if !interrupts::is_initialized() {
        return Err("FPU setup requires the IDT");
    }
    if arch::interrupts_enabled() {
        return Err("FPU setup requires IF=0");
    }
    let basic_limit = cpuid(0).eax;
    if basic_limit < 1 {
        return Err("CPU lacks architectural feature discovery");
    }
    let features = cpuid(1);
    let required = (1 << 0) | (1 << 24) | (1 << 25) | (1 << 26);
    if features.edx & required != required {
        return Err("CPU requires x87, FXSR, SSE and SSE2");
    }

    // A bootloader can leave AMD fast saves enabled. At CPL0 that mode skips
    // XMM0..15, so explicitly disable it whenever the CPU advertises the bit.
    let extended_limit = cpuid(0x8000_0000).eax;
    if extended_limit >= 0x8000_0001 {
        let extended_features = cpuid(0x8000_0001).edx;
        if extended_features & (1 << 25) != 0 {
            let low: u32;
            let high: u32;
            let observed_low: u32;
            let observed_high: u32;
            // SAFETY: EFER exists at CPL0 on x86-64. The advertised FFXSR bit
            // is writable; every unrelated EFER bit remains unchanged.
            unsafe {
                asm!("rdmsr", in("ecx") EFER, out("eax") low, out("edx") high,
                    options(nomem, nostack, preserves_flags));
                let efer = ((u64::from(high) << 32) | u64::from(low)) & !EFER_FFXSR;
                asm!("wrmsr", in("ecx") EFER, in("eax") efer as u32,
                    in("edx") (efer >> 32) as u32, options(nostack, preserves_flags));
                asm!("rdmsr", in("ecx") EFER,
                    out("eax") observed_low, out("edx") observed_high,
                    options(nomem, nostack, preserves_flags));
            }
            if ((u64::from(observed_high) << 32) | u64::from(observed_low)) & EFER_FFXSR != 0 {
                return Err("CPU retained fast FXSAVE mode");
            }
        }
    }

    let cr0: u64;
    let cr4: u64;
    let observed_cr0: u64;
    let observed_cr4: u64;
    // SAFETY: Executing CPU at CPL0 with IF clear. Feature gates were checked;
    // installed #MF/#XM handlers are ready before enabling native exceptions.
    // Soft-float Rust never uses the affected register files implicitly.
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        let enabled_cr0 = (cr0 | CR0_MP | CR0_NE) & !(CR0_EM | CR0_TS);
        let enabled_cr4 = (cr4 | CR4_OSFXSR | CR4_OSXMMEXCPT) & !(CR4_OSXSAVE | CR4_FSGSBASE);
        asm!("mov cr0, {}", in(reg) enabled_cr0, options(nostack, preserves_flags));
        asm!("mov cr4, {}", in(reg) enabled_cr4, options(nostack, preserves_flags));
        // AMD may leave x87 FIP/FDP/FOP unchanged during FXRSTOR when the
        // incoming image has FSW.ES=0. FNINIT clears those stale pointers first
        // without delivering pending user exceptions or recording a kernel
        // instruction/data pointer, then FXRSTOR fills every register payload.
        asm!("fninit", "fxrstor64 [{}]", in(reg) addr_of!(MIRROR_FPU_CLEAN),
            options(nostack));
        // Catch accidental kernel FPU use outside the explicit user transition.
        // Handlers still run with TS=0 while retaining the interrupted user
        // state, so they too must remain compiled for the soft-float target.
        asm!("mov cr0, {}", in(reg) (enabled_cr0 | CR0_TS),
            options(nostack, preserves_flags));
        asm!("mov {}, cr0", out(reg) observed_cr0,
            options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) observed_cr4,
            options(nomem, nostack, preserves_flags));
    }
    if observed_cr0 & (CR0_MP | CR0_NE | CR0_TS | CR0_EM) != (CR0_MP | CR0_NE | CR0_TS)
        || observed_cr4 & (CR4_OSFXSR | CR4_OSXMMEXCPT | CR4_OSXSAVE | CR4_FSGSBASE)
            != (CR4_OSFXSR | CR4_OSXMMEXCPT)
    {
        return Err("CPU retained unexpected FPU controls");
    }
    Ok(features.ecx & (1 << 28) != 0)
}

// Rust 1.88 declares this intrinsic unsafe; newer compilers declare it safe.
// Keep the advertised minimum compiler without lint noise on newer toolchains.
#[allow(unused_unsafe)]
fn cpuid(leaf: u32) -> CpuidResult {
    // SAFETY: CPUID is guaranteed on the x86-64 target, including extended-leaf
    // discovery. The caller checks the maximum leaf before querying features.
    unsafe { __cpuid(leaf) }
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn self_test() {
    assert_kernel_clean();
    serial::println!("MIRROR_FPU_TESTS_OK clean=1 controls=1 kernel_ts=1");
}

/// Check the actual privileged register file after boot or any user return.
/// No log/heap work occurs unless an invariant fails.
#[cfg(feature = "boot-smoke")]
pub(crate) fn assert_kernel_clean() {
    assert!(is_initialized(), "FPU check requires initialization");
    assert!(!arch::interrupts_enabled(), "FPU check requires IF=0");
    let cr0: u64;
    let cr4: u64;
    let mut observed = State::new();
    // SAFETY: The boot CPU is initialized, IF is clear, and the kernel owns the
    // live register file. Temporarily clear TS to read the clean hardware image
    // into this aligned buffer; FXSAVE preserves the register state itself.
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        asm!("mov cr0, {}", in(reg) (cr0 & !CR0_TS),
            options(nostack, preserves_flags));
        asm!("fxsave64 [{}]", in(reg) &mut observed, options(nostack));
        asm!("mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags));
    }
    assert_eq!(cr0 & (CR0_MP | CR0_NE | CR0_TS), CR0_MP | CR0_NE | CR0_TS);
    assert_eq!(cr0 & CR0_EM, 0);
    assert_eq!(
        cr4 & (CR4_OSFXSR | CR4_OSXMMEXCPT),
        CR4_OSFXSR | CR4_OSXMMEXCPT
    );
    assert_eq!(cr4 & (CR4_OSXSAVE | CR4_FSGSBASE), 0);
    assert_eq!(&observed.bytes[0..2], &[0x7f, 0x03]);
    assert_eq!(&observed.bytes[2..4], &[0, 0]); // FSW, including TOP and flags.
    assert_eq!(observed.bytes[4], 0); // Abridged FTW: every register empty.
    assert_eq!(&observed.bytes[6..8], &[0, 0]); // FOP.
    assert!(observed.bytes[8..24].iter().all(|byte| *byte == 0)); // FIP/FDP.
    assert_eq!(&observed.bytes[24..28], &[0x80, 0x1f, 0, 0]);
    for slot in 0..8 {
        // Only the meaningful 80-bit payload, excluding six reserved bytes.
        let start = 32 + slot * 16;
        assert!(
            observed.bytes[start..start + 10]
                .iter()
                .all(|byte| *byte == 0)
        );
    }
    assert!(observed.bytes[160..416].iter().all(|byte| *byte == 0));
}
