//! Per-CPU supervisor protections (spec 0016b).
//!
//! Every CPU enables SMEP, SMAP and UMIP exactly where its CPUID reports them
//! and forces EFER.SCE off, so a user `syscall` raises #UD until the SYSCALL
//! entry programs its MSRs (0016-D1 to D3). While SMAP is on, the kernel runs
//! with RFLAGS.AC clear: each CPU clears AC before enabling SMAP, the common
//! exception entry executes CLAC once the BSP has set `MIRROR_SMAP_ACTIVE`, and
//! the kernel continuation after a user return starts with AC clear. Each
//! setting is verified by reading the register back. An AP whose CPUID set
//! differs from the BSP's fails startup.
//!
//! CR4 and EFER: Intel SDM vol. 3A, sections 2.5 and 2.2.1; SMEP and SMAP:
//! section 4.6; CPUID leaf 7: vol. 2A, CPUID.
//! https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
//! AMD APM vol. 2, sections 3.1.3 (CR4) and 3.1.7 (EFER):
//! https://docs.amd.com/v/u/en-US/24593_3.44_APM_Vol2

use core::arch::{
    asm,
    x86_64::{__cpuid, __cpuid_count},
};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::{arch, interrupts, serial};

const CR4_UMIP: u64 = 1 << 11;
const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;
const EFER: u32 = 0xc000_0080;
const EFER_SCE: u64 = 1 << 0;
const RFLAGS_AC: u64 = 1 << 18;
const CPUID_7_EBX_SMEP: u32 = 1 << 7;
const CPUID_7_EBX_SMAP: u32 = 1 << 20;
const CPUID_7_ECX_UMIP: u32 = 1 << 2;

/// Set in `BSP_CONTROLS` once the BSP has configured itself; bits 0-2 then
/// hold its `Controls`.
const PUBLISHED: u8 = 1 << 7;
static BSP_CONTROLS: AtomicU8 = AtomicU8::new(0);

/// Read by `mirror_exception_common` (interrupts.S), which executes CLAC on
/// every entry while it is set. Only the BSP sets it, in `init`, before any CPU
/// enables SMAP and only when its own CPUID reports SMAP, where CLAC is
/// defined. A CPU enables SMAP only while it is set, and an AP whose SMAP
/// support differs from the BSP's fails startup (0016-D1). No code is patched.
#[unsafe(no_mangle)]
static MIRROR_SMAP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The protections one CPU's CPUID reports.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Controls {
    smep: bool,
    smap: bool,
    umip: bool,
}

impl Controls {
    /// Read CPUID leaf 7, subleaf 0, if the maximum basic leaf includes it.
    fn detect() -> Self {
        if __cpuid(0).eax < 7 {
            return Self::from_bits(0);
        }
        let leaf = __cpuid_count(7, 0);
        Self {
            smep: leaf.ebx & CPUID_7_EBX_SMEP != 0,
            smap: leaf.ebx & CPUID_7_EBX_SMAP != 0,
            umip: leaf.ecx & CPUID_7_ECX_UMIP != 0,
        }
    }

    /// The CR4 bits this CPU enables. SMAP also needs the entry stub's CLAC,
    /// which `smap_active` reports.
    fn enabled_cr4(self, smap_active: bool) -> u64 {
        let mut bits = 0;
        if self.smep {
            bits |= CR4_SMEP;
        }
        if self.smap && smap_active {
            bits |= CR4_SMAP;
        }
        if self.umip {
            bits |= CR4_UMIP;
        }
        bits
    }

    /// Pack into bits 0-2, for an atomic slot field.
    pub(crate) fn bits(self) -> u8 {
        u8::from(self.smep) | (u8::from(self.smap) << 1) | (u8::from(self.umip) << 2)
    }

    pub(crate) fn from_bits(bits: u8) -> Self {
        Self {
            smep: bits & 1 != 0,
            smap: bits & 2 != 0,
            umip: bits & 4 != 0,
        }
    }
}

/// Configure the BSP and publish its controls for the APs to compare against.
/// Panics if the CPU does not hold the expected values.
pub(crate) fn init() {
    assert_eq!(
        BSP_CONTROLS.load(Ordering::Relaxed),
        0,
        "CPU controls already configured"
    );
    // From here on every entry clears AC, before any CPU enables SMAP.
    MIRROR_SMAP_ACTIVE.store(Controls::detect().smap, Ordering::SeqCst);
    let controls = configure_current().expect("CPU control setup failed");
    BSP_CONTROLS.store(controls.bits() | PUBLISHED, Ordering::Release);
}

/// The BSP's controls, once `init` has published them.
pub(crate) fn bsp_controls() -> Option<Controls> {
    let bits = BSP_CONTROLS.load(Ordering::Acquire);
    (bits & PUBLISHED != 0).then(|| Controls::from_bits(bits))
}

/// Configure only the executing CPU: set CR4.SMEP, CR4.SMAP and CR4.UMIP
/// exactly where CPUID reports them and clear EFER.SCE, then read CR4 and EFER
/// back. SMAP is set only while `MIRROR_SMAP_ACTIVE` is, after clearing AC, and
/// AC is read back too. Every other bit keeps its value. Returns the CPUID
/// set. Failures are returned without logging, so an AP can call this before
/// its own stacks are mapped.
pub(crate) fn configure_current() -> Result<Controls, &'static str> {
    if !interrupts::is_initialized() {
        return Err("CPU controls require the IDT");
    }
    if arch::interrupts_enabled() {
        return Err("CPU controls require IF=0");
    }
    let controls = Controls::detect();
    let smap = controls.smap && MIRROR_SMAP_ACTIVE.load(Ordering::Acquire);
    if smap {
        // SAFETY: CPL0, and CPUID reports SMAP, so CLAC is defined whatever
        // CR4 holds. It changes only RFLAGS.AC, which compiled code never
        // relies on.
        unsafe { asm!("clac", options(nomem, nostack)) };
    }
    let cr4 = read_cr4();
    let expected = (cr4 & !(CR4_SMEP | CR4_SMAP | CR4_UMIP)) | controls.enabled_cr4(smap);
    if expected != cr4 {
        // SAFETY: CPL0 with IF clear. CPUID reports each bit being set, so the
        // write cannot fault, and clearing an unreported bit writes the zero it
        // must already hold. Every other bit keeps the value just read. SMEP
        // and SMAP only stop supervisor instruction fetches from, and data
        // accesses with AC clear to, user pages, and UMIP only affects CPL3.
        // The kernel neither runs from nor touches a user page through its
        // user address: Limine's root, which the BSP and an AP may still be
        // using, and Mirror's roots map the kernel supervisor-only, and user
        // memory is reached through the direct map after a page walk.
        unsafe { asm!("mov cr4, {}", in(reg) expected, options(nostack, preserves_flags)) };
    }
    if read_cr4() != expected {
        return Err("CPU retained unexpected CR4 protections");
    }
    if smap && alignment_check_set() {
        return Err("CPU kept RFLAGS.AC set with SMAP on");
    }

    let efer = read_efer();
    if efer & EFER_SCE != 0 {
        let cleared = efer & !EFER_SCE;
        // SAFETY: EFER exists at CPL0 on x86-64, and SCE is always writable.
        // Clearing it makes SYSCALL raise #UD; nothing programs the SYSCALL
        // MSRs, and the kernel never executes SYSCALL or SYSRET. Every other
        // bit keeps the value just read.
        unsafe {
            asm!("wrmsr", in("ecx") EFER, in("eax") cleared as u32,
                in("edx") (cleared >> 32) as u32, options(nostack, preserves_flags));
        }
    }
    if read_efer() != efer & !EFER_SCE {
        return Err("CPU retained EFER.SCE");
    }
    Ok(controls)
}

/// Print one CPU's `MIRROR_CPU_HARDENING` line from its CR4, EFER and CPUID set.
pub(crate) fn report(cpu: usize, cr4: u64, efer: u64, controls: Controls) {
    let bit = |value: u64, mask: u64| u8::from(value & mask != 0);
    serial::println!(
        "MIRROR_CPU_HARDENING cpu={} cr4={:#x} smep={} smap={} umip={} sce={} cpuid_smep={} cpuid_smap={} cpuid_umip={}",
        cpu,
        cr4,
        bit(cr4, CR4_SMEP),
        bit(cr4, CR4_SMAP),
        bit(cr4, CR4_UMIP),
        bit(efer, EFER_SCE),
        u8::from(controls.smep),
        u8::from(controls.smap),
        u8::from(controls.umip)
    );
}

fn alignment_check_set() -> bool {
    let flags: u64;
    // SAFETY: PUSHFQ/POP are balanced on the current, mapped stack.
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags)) };
    flags & RFLAGS_AC != 0
}

pub(crate) fn read_cr4() -> u64 {
    let cr4: u64;
    // SAFETY: The kernel runs at CPL0, where reading CR4 changes nothing.
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    cr4
}

pub(crate) fn read_efer() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: EFER exists at CPL0 on x86-64, and reading it changes nothing.
    unsafe {
        asm!("rdmsr", in("ecx") EFER, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
    }
    (u64::from(high) << 32) | u64::from(low)
}
