//! CPU-local setup and eager ownership of x87/MMX, SSE and AVX register state.
//!
//! Kernel Rust remains soft-float. Every scheduled user context owns one full
//! standard XSAVE64 image (x87 + SSE + AVX only), or its FXSAVE64 prefix on a
//! CPU without that contract. The user entry/return assembly restores/saves it
//! with IF=0. Every participating CPU uses the BSP's format and AVX2 feature
//! level, so migrating a context cannot lose state or instruction support.
//! FS/GS bases use the checked TLS ABI; direct base instructions stay disabled.
//!
//! State layout and controls: Intel SDM volumes 2 (XSAVE/XRSTOR, CPUID,
//! XSETBV, FXSAVE/FXRSTOR, FNINIT) and 3A:
//! https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
//! AMD APM volume 1, section 6.9.1.2, and volume 2, EFER.FFXSR:
//! https://docs.amd.com/api/khub/documents/sfvvekC9mDflu6vd3R0NXA/content
//! https://docs.amd.com/api/khub/documents/sD1_QL~h4Afq2_tvzxqqSQ/content

use core::arch::{
    asm,
    x86_64::{__cpuid, __cpuid_count, CpuidResult},
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
const CPUID_XSAVE: u32 = 1 << 26;
const CPUID_AVX: u32 = 1 << 28;
const CPUID_AVX2: u32 = 1 << 5;
pub(crate) const XSTATE_MASK: u32 = 7;
const FXSAVE_BYTES: usize = 512;
const YMM_OFFSET: usize = 576;
const YMM_BYTES: usize = 256;
const XSAVE_BYTES: usize = YMM_OFFSET + YMM_BYTES;

/// Supervisor-only, aligned state image. No user-provided image is restored:
/// reserved MXCSR/header bits can otherwise fault a privileged restore. The
/// fallback uses only the first 512 bytes; keeping one size lets contexts be
/// initialized before BSP discovery and preserves their alignment in arrays.
#[repr(C, align(64))]
pub(crate) struct State {
    bytes: [u8; XSAVE_BYTES],
}

impl State {
    /// Empty x87 stack, round-to-nearest, masked exceptions, and zero register
    /// payloads. FNINIT alone leaves the old x87 data and SIMD registers intact.
    pub(crate) const fn new() -> Self {
        let mut bytes = [0; XSAVE_BYTES];
        bytes[0] = 0x7f;
        bytes[1] = 0x03; // FCW = 0x037f.
        bytes[24] = 0x80;
        bytes[25] = 0x1f; // MXCSR = 0x1f80; DAZ/FTZ disabled.
        // Restore actual zero payloads, even for empty x87 registers. An
        // XRSTOR init-state request (a zero XSTATE_BV bit) need not clear the
        // underlying x87 data bytes. Standard format: XCOMP_BV and every
        // reserved header byte stay zero.
        bytes[512] = XSTATE_MASK as u8;
        Self { bytes }
    }
}

const _: () = assert!(size_of::<State>() == XSAVE_BYTES);
const _: () = assert!(align_of::<State>() == 64);

// Read-only supervisor mapping, also referenced directly by userspace_entry.S.
#[unsafe(no_mangle)]
static MIRROR_FPU_CLEAN: State = State::new();
// The BSP writes once before INITIALIZED's release publication. Assembly
// reads the byte only after initialization; every AP acquired that publication
// and configured the same contract before it can run a user context.
#[unsafe(no_mangle)]
static MIRROR_XSAVE_ACTIVE: AtomicBool = AtomicBool::new(false);
static AVX2_ACTIVE: AtomicBool = AtomicBool::new(false);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Contract {
    xsave: bool,
    avx2: bool,
}

impl Contract {
    fn supports(self, required: Self) -> bool {
        // A fallback BSP never enables AVX on another CPU, even when that CPU
        // offers it. With AVX enabled, AVX2 must match in both directions:
        // users can discover it on any CPU before moving to another one.
        !required.xsave || (self.xsave && self.avx2 == required.avx2)
    }
}

fn contract() -> Contract {
    Contract {
        xsave: MIRROR_XSAVE_ACTIVE.load(Ordering::Relaxed),
        avx2: AVX2_ACTIVE.load(Ordering::Relaxed),
    }
}

pub(crate) fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn avx_enabled() -> bool {
    MIRROR_XSAVE_ACTIVE.load(Ordering::Acquire)
}

/// Establish the boot CPU's extended-state contract before any user entry.
/// The installed exception gates handle #MF (16) and #XM (19); unmasked user
/// exceptions terminate their process through the ordinary fault path.
pub(crate) fn init() {
    assert!(interrupts::is_initialized(), "FPU setup requires the IDT");
    assert!(!arch::interrupts_enabled(), "FPU setup requires IF=0");
    assert!(!is_initialized(), "FPU already initialized");
    let (selected, cpuid_avx) = detect().expect("FPU feature discovery failed");
    configure_current(selected).expect("FPU CPU setup failed");
    MIRROR_XSAVE_ACTIVE.store(selected.xsave, Ordering::Relaxed);
    AVX2_ACTIVE.store(selected.avx2, Ordering::Relaxed);
    INITIALIZED.store(true, Ordering::Release);
    serial::println!(
        "MIRROR_FPU_READY format={} bytes={} xmm=16 eager=1 avx={} avx2={} cpuid_avx={} osxsave={} mask={}",
        if selected.xsave {
            "xsave64"
        } else {
            "fxsave64"
        },
        if selected.xsave {
            XSAVE_BYTES
        } else {
            FXSAVE_BYTES
        },
        u8::from(selected.xsave),
        u8::from(selected.avx2),
        u8::from(cpuid_avx),
        u8::from(selected.xsave),
        if selected.xsave { XSTATE_MASK } else { 0 }
    );
}

/// Establish the same control and clean-register contract on this secondary.
/// The caller has installed its private exception descriptors with IF clear.
/// No global publication, logging, allocation or BSP panic path is used here.
pub(crate) fn init_secondary() -> Result<(), &'static str> {
    if !INITIALIZED.load(Ordering::Acquire) {
        return Err("secondary FPU setup requires BSP initialization");
    }
    let required = contract();
    if !detect()?.0.supports(required) {
        return Err("secondary CPU extended state differs from BSP");
    }
    configure_current(required)
}

/// Discover hardware independently of the bootloader's CR4.OSXSAVE and XCR0.
/// The enabled-size field (CPUID.D.0:EBX) is checked only after XSETBV.
fn detect() -> Result<(Contract, bool), &'static str> {
    let basic_limit = cpuid(0).eax;
    if basic_limit < 1 {
        return Err("CPU lacks architectural feature discovery");
    }
    let features = cpuid(1);
    let required = (1 << 0) | (1 << 24) | (1 << 25) | (1 << 26);
    if features.edx & required != required {
        return Err("CPU requires x87, FXSR, SSE and SSE2");
    }
    let xsave = basic_limit >= 0xd
        && features.ecx & (CPUID_XSAVE | CPUID_AVX) == (CPUID_XSAVE | CPUID_AVX)
        && supported_layout(__cpuid_count(0xd, 0), __cpuid_count(0xd, 2));
    let avx2 = xsave && __cpuid_count(7, 0).ebx & CPUID_AVX2 != 0;
    Ok((Contract { xsave, avx2 }, features.ecx & CPUID_AVX != 0))
}

/// Accept only the standard x87/SSE/AVX layout that fits our fixed image. The
/// maximum size can include additional components, but none are enabled or
/// requested by our saves. YMM must belong to XCR0 rather than IA32_XSS.
fn supported_layout(state: CpuidResult, ymm: CpuidResult) -> bool {
    state.eax & XSTATE_MASK == XSTATE_MASK
        && state.ecx >= XSAVE_BYTES as u32
        && ymm.eax == YMM_BYTES as u32
        && ymm.ebx == YMM_OFFSET as u32
        && ymm.ecx & 1 == 0
        && ymm.edx == 0
}

/// Configure only the executing CPU, after hardware discovery validated the
/// requested format. The BSP calls this with its own detected contract; APs
/// call it only after matching the BSP's published contract.
fn configure_current(selected: Contract) -> Result<(), &'static str> {
    if !interrupts::is_initialized() {
        return Err("FPU setup requires the IDT");
    }
    if arch::interrupts_enabled() {
        return Err("FPU setup requires IF=0");
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
    let xsave_cr4 = if selected.xsave { CR4_OSXSAVE } else { 0 };
    // SAFETY: Executing CPU at CPL0 with IF clear. Feature gates were checked;
    // installed #MF/#XM handlers are ready before enabling native exceptions.
    // Soft-float Rust never uses the affected register files implicitly.
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        let enabled_cr0 = (cr0 | CR0_MP | CR0_NE) & !(CR0_EM | CR0_TS);
        let enabled_cr4 =
            ((cr4 | CR4_OSFXSR | CR4_OSXMMEXCPT) & !(CR4_OSXSAVE | CR4_FSGSBASE)) | xsave_cr4;
        asm!("mov cr0, {}", in(reg) enabled_cr0, options(nostack, preserves_flags));
        asm!("mov cr4, {}", in(reg) enabled_cr4, options(nostack, preserves_flags));
        if selected.xsave {
            // This mask excludes every unsupported or unowned component,
            // including AVX-512, AMX and supervisor state. CR4.OSXSAVE is set
            // and discovery proved x87/SSE/AVX are all legal XCR0 bits.
            asm!("xsetbv", in("ecx") 0_u32, in("eax") XSTATE_MASK,
                in("edx") 0_u32, options(nostack, preserves_flags));
            if xcr0() != u64::from(XSTATE_MASK) || __cpuid_count(0xd, 0).ebx != XSAVE_BYTES as u32 {
                // Preserve the kernel TS guard even on an AP rejected here.
                asm!("mov cr0, {}", in(reg) (enabled_cr0 | CR0_TS),
                    options(nostack, preserves_flags));
                return Err("CPU retained unexpected XCR0 or XSAVE size");
            }
        }
        // AMD may leave x87 FIP/FDP/FOP unchanged during FXRSTOR when the
        // incoming image has FSW.ES=0. FNINIT clears those stale pointers first
        // without delivering pending user exceptions or recording a kernel
        // instruction/data pointer. Both formats explicitly load the empty
        // x87 payload and all enabled vector registers from the clean image.
        if selected.xsave {
            asm!("fninit", "xrstor64 [{}]", in(reg) addr_of!(MIRROR_FPU_CLEAN),
                in("eax") XSTATE_MASK, in("edx") 0_u32, options(nostack));
        } else {
            asm!("fninit", "fxrstor64 [{}]", in(reg) addr_of!(MIRROR_FPU_CLEAN),
                options(nostack));
        }
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
            != (CR4_OSFXSR | CR4_OSXMMEXCPT | xsave_cr4)
    {
        return Err("CPU retained unexpected FPU controls");
    }
    Ok(())
}

fn cpuid(leaf: u32) -> CpuidResult {
    __cpuid(leaf)
}

/// # Safety
/// The executing CPU advertises XSAVE and has CR4.OSXSAVE set.
unsafe fn xcr0() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: The caller established XSAVE and OSXSAVE. XCR0 exists on every
    // such CPU, and this read changes no processor state.
    unsafe {
        asm!("xgetbv", in("ecx") 0_u32, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
    }
    (u64::from(high) << 32) | u64::from(low)
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn self_test() {
    policy_self_test();
    assert_kernel_clean();
    serial::println!("MIRROR_FPU_TESTS_OK clean=1 controls=1 kernel_ts=1");
}

/// Pure fixtures exercise CPUID layout rejection and migration admission
/// without executing any optional instruction or mutating live controls.
#[cfg(feature = "boot-smoke")]
fn policy_self_test() {
    let state = CpuidResult {
        eax: XSTATE_MASK,
        // Before XSETBV this depends on the loader's XCR0 and is ignored.
        ebx: 576,
        ecx: XSAVE_BYTES as u32,
        edx: 0,
    };
    let ymm = CpuidResult {
        eax: YMM_BYTES as u32,
        ebx: YMM_OFFSET as u32,
        ecx: 0,
        edx: 0,
    };
    assert!(supported_layout(state, ymm));
    // Future components do not invalidate the bounded three-component mask.
    assert!(supported_layout(
        CpuidResult {
            eax: state.eax | (1 << 9),
            ecx: 4096,
            ..state
        },
        ymm
    ));
    for bit in 0..3 {
        assert!(!supported_layout(
            CpuidResult {
                eax: state.eax & !(1 << bit),
                ..state
            },
            ymm
        ));
    }
    assert!(!supported_layout(
        CpuidResult {
            ecx: XSAVE_BYTES as u32 - 1,
            ..state
        },
        ymm
    ));
    for rejected in [
        CpuidResult { eax: 255, ..ymm },
        CpuidResult { eax: 257, ..ymm },
        CpuidResult { ebx: 512, ..ymm },
        CpuidResult { ebx: 640, ..ymm },
        CpuidResult { ecx: 1, ..ymm },
        CpuidResult { edx: 1, ..ymm },
    ] {
        assert!(!supported_layout(state, rejected));
    }
    let fallback = Contract {
        xsave: false,
        avx2: false,
    };
    let avx = Contract {
        xsave: true,
        avx2: false,
    };
    let avx2 = Contract {
        xsave: true,
        avx2: true,
    };
    for local in [fallback, avx, avx2] {
        assert!(local.supports(fallback));
        assert!(local.supports(local));
    }
    assert!(!fallback.supports(avx));
    assert!(!fallback.supports(avx2));
    assert!(!avx.supports(avx2));
    assert!(!avx2.supports(avx));
}

/// Check the actual privileged register file after boot or any user return.
/// No log/heap work occurs unless an invariant fails.
#[cfg(feature = "boot-smoke")]
pub(crate) fn assert_kernel_clean() {
    assert!(is_initialized(), "FPU check requires initialization");
    assert!(!arch::interrupts_enabled(), "FPU check requires IF=0");
    let cr0: u64;
    let cr4: u64;
    let selected = contract();
    let mut observed = State::new();
    // Poison the meaningful payloads so an omitted hardware save cannot pass
    // as a clean register file. Header reserved bytes stay valid and zero.
    observed.bytes[..FXSAVE_BYTES].fill(0xa5);
    observed.bytes[YMM_OFFSET..].fill(0xa5);
    // SAFETY: The boot CPU is initialized, IF is clear, and the kernel owns the
    // live register file. Temporarily clear TS to read the clean hardware image
    // into this aligned buffer; both saves preserve the register state itself.
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        asm!("mov cr0, {}", in(reg) (cr0 & !CR0_TS),
            options(nostack, preserves_flags));
        if selected.xsave {
            asm!("xsave64 [{}]", in(reg) &mut observed,
                in("eax") XSTATE_MASK, in("edx") 0_u32, options(nostack));
        } else {
            asm!("fxsave64 [{}]", in(reg) &mut observed, options(nostack));
        }
        asm!("mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags));
    }
    assert_eq!(cr0 & (CR0_MP | CR0_NE | CR0_TS), CR0_MP | CR0_NE | CR0_TS);
    assert_eq!(cr0 & CR0_EM, 0);
    assert_eq!(
        cr4 & (CR4_OSFXSR | CR4_OSXMMEXCPT),
        CR4_OSFXSR | CR4_OSXMMEXCPT
    );
    assert_eq!(
        cr4 & (CR4_OSXSAVE | CR4_FSGSBASE),
        if selected.xsave { CR4_OSXSAVE } else { 0 }
    );
    if selected.xsave {
        // SAFETY: This CPU initialized the XSAVE contract, and the readback
        // immediately above established CR4.OSXSAVE is still set.
        assert_eq!(unsafe { xcr0() }, u64::from(XSTATE_MASK));
        assert_eq!(__cpuid_count(0xd, 0).ebx, XSAVE_BYTES as u32);
        assert_eq!(observed.bytes[512] & !(XSTATE_MASK as u8), 0);
        assert!(
            observed.bytes[513..YMM_OFFSET]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert!(observed.bytes[YMM_OFFSET..].iter().all(|byte| *byte == 0));
    }
    assert_eq!(&observed.bytes[0..2], &[0x7f, 0x03]);
    assert_eq!(&observed.bytes[2..4], &[0, 0]); // FSW, including TOP and flags.
    assert_eq!(observed.bytes[4], 0); // Abridged FTW: every register empty.
    // QEMU TCG's do_xsave_fpu leaves FOP (bytes 6..8) unwritten for both
    // FXSAVE and XSAVE, so those buffer bytes cannot verify the live opcode.
    // FNINIT before every restore still clears stale x87 pointers/opcodes.
    // https://github.com/qemu/qemu/blob/v11.1.0/target/i386/tcg/fpu_helper.c#L2481-L2508
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
