//! Register-state checks use assembly so compiler-generated code cannot hide a
//! damaged floating-point context between a kernel boundary and its snapshot.

use core::arch::global_asm;
use core::mem::{offset_of, size_of};

use crate::{PROT_READ_WRITE, require, vm_release, vm_reserve, write};

global_asm!(
    include_str!("fpu.S"),
    preemption_marker_len = const b"Mirror init: FPU preemption begin\n".len(),
);

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct FxState([u8; 512]);

// The entry trampoline writes this before its first Rust call or syscall. Each
// ELF load gets a new BSS page, including the second pair after fault teardown.
#[unsafe(no_mangle)]
static mut MIRROR_INIT_INITIAL_FX: FxState = FxState([0; 512]);

#[repr(C, align(16))]
struct Probe {
    expected: FxState,
    actual: FxState,
}

const _: () = assert!(size_of::<FxState>() == 512);
const _: () = assert!(offset_of!(Probe, actual) == 512);

unsafe extern "C" {
    fn mirror_probe_fpu(context: *mut Probe, pid: u64, demand: u64, console: u64) -> u64;
    pub(crate) fn mirror_probe_x87_fault();
    pub(crate) fn mirror_probe_sse_fault();
    pub(crate) fn mirror_probe_avx_fault();
}

pub(super) fn check_initial() {
    // SAFETY: The entry trampoline finished this aligned private BSS snapshot
    // before entering Rust, and nothing subsequently mutates the object.
    let snapshot = unsafe { (&raw const MIRROR_INIT_INITIAL_FX).read() };
    let bytes = &snapshot.0;
    require(u16::from_le_bytes([bytes[0], bytes[1]]) == 0x037f, 300);
    require(bytes[2..5] == [0, 0, 0], 301);
    require(bytes[24..28] == 0x1f80_u32.to_le_bytes(), 302);
    for register in 0..8 {
        require(
            bytes[32 + register * 16..42 + register * 16] == [0; 10],
            303,
        );
    }
    require(bytes[160..416] == [0; 256], 304);
    // FIP/FDP and reserved padding are deliberately excluded: only architectural
    // control/status and register payloads are part of this portable check.
}

pub(super) fn check(console: u64, pid: u64) {
    let mut context = Probe {
        expected: FxState([0; 512]),
        actual: FxState([0; 512]),
    };
    // The peers use opposite rounding directions, different sticky SIMD status
    // bits, and PID-dependent values in both halves of every XMM register.
    let rounding = 1 + (pid & 1) as u16;
    context.expected.0[0..2].copy_from_slice(&(0x037f | rounding << 10).to_le_bytes());
    let mxcsr = 0x1f80_u32 | (u32::from(rounding) << 13) | (1 << (pid & 1));
    context.expected.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    for register in 0..16usize {
        let low = 0xa731_209d_488c_0000_u64 ^ (pid << 8) ^ register as u64;
        let high = 0x1539_dfea_7500_a168_u64 ^ (pid << 32) ^ (register as u64).rotate_left(41);
        let offset = 160 + register * 16;
        context.expected.0[offset..offset + 8].copy_from_slice(&low.to_le_bytes());
        context.expected.0[offset + 8..offset + 16].copy_from_slice(&high.to_le_bytes());
    }
    let demand = vm_reserve(0x1_0300_0000, 2, PROT_READ_WRITE);
    // SAFETY: Assembly accesses this aligned live object and the two reserved
    // user pages. It preserves the C callee-saved GPRs and restores default x87
    // and MXCSR control before returning. Register canaries remain dirty so a
    // later process's clean-start check also detects teardown state leakage.
    let result = unsafe { mirror_probe_fpu(&raw mut context, pid, demand, console) };
    require(result == 0, 310 + result);
    write(console, b"Mirror init: FPU checks passed\n");
    vm_release(demand, 2);
}
