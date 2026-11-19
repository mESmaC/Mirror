# 0053: XSAVE and AVX2 user state

Depends on: 0050, 0051. Ledger: I6, S12, S7a, S9.

## Goal

Native threads can use AVX and, where advertised, AVX2 without leaking or losing
register state across syscalls, faults, scheduling, CPU migration or teardown.
CPUs without usable AVX retain the existing x87/SSE2 contract. The kernel and
userspace Rust targets remain unchanged; hard-float userspace is 0028.

## Design

- Use eager standard-format `XSAVE64`/`XRSTOR64`, with exactly x87, SSE and AVX
  enabled (`XCR0=7`). Check CPUID XSAVE/AVX, leaf 0xD's supported mask and AVX
  component (offset 576, length 256), then check the enabled size after setting
  XCR0. Reject an inconsistent layout before any save. AVX2 is independently
  detected through CPUID leaf 7. No XSAVEOPT, compacted format, supervisor state,
  AVX-512 or AMX is enabled.
- Each thread owns a supervisor-only, 64-byte-aligned 832-byte buffer. FXSAVE
  fallback uses its first 512 bytes. The clean image has default controls, empty
  x87 stack and zero register payloads; its XSAVE header explicitly restores all
  three components and has zero compacted/reserved fields. Saved images never
  come from userspace. `FNINIT` before restore retains the existing AMD x87
  pointer precaution.
- The BSP publishes one contract before AP admission. Every AP validates the
  enabled layout and matching AVX2 capability and programs the same XCR0, or is
  rejected before it can run user threads. A fallback BSP keeps OSXSAVE disabled
  on every AP. FSGSBASE stays disabled.
- Save the outgoing user image before restoring the clean kernel image, then
  re-arm CR0.TS. The shared same-context interrupt return continues to preserve
  live registers. All kernel Rust remains soft-float, including handlers that
  execute while TS is clear. No scheduler policy or syscall is changed.
- The boot record is `MIRROR_FPU_READY format=<fxsave64|xsave64>
  bytes=<512|832> xmm=16 eager=1 avx=<0|1> avx2=<0|1> cpuid_avx=<0|1>
  osxsave=<0|1> mask=<0|7>` on one line. Host assertions require the exact
  capability/format combination. Existing clean/control and user-preemption
  checks remain; extended probes add explicit success evidence.

## Tests

1. Pure boot-smoke policy checks reject incomplete features, malformed XSAVE
   layouts and incompatible AP capabilities. Hardware clean-state checks verify
   privileged controls, the selected mask, and all meaningful x87/XMM/YMM bytes.
2. Extend the existing user FPU probes with distinct YMM0-15 canaries and AVX2
   arithmetic when available. Compare after syscalls, yield, sleep, direct and
   deferred page faults, and an interval with kernel-confirmed preemption.
   Independent explicit YMM snapshots avoid sharing the kernel's XSAVE oracle.
3. Fresh process and thread snapshots require clean controls and zero payloads;
   relaunch after dirty-state teardown repeats the check. SMP wait/receive
   canaries cover live extended state on participating CPUs. Before workers
   start, three tagged smoke-only yields force the main thread BSP -> AP -> BSP;
   each next call acknowledges the preceding assembly canary comparison. The
   existing process CPU mask controls placement; scheduler policy is unchanged.
   `MIRROR_SMP_XSTATE_OK cpus=N migrations=2 avx=<0|1>` follows the SMP result;
   a single userspace CPU instead reports zero migrations. The marker is absent
   from default and trace kernels.
4. Keep `UserAvx` as an unsupported-AVX #UD test on default/baseline QEMU. Reject
   `-ExtendedCpu -Fault UserAvx` before boot; extended normal boots exercise
   successful AVX/AVX2. Retain the existing 1-, 2- and 8-CPU extended cases,
   thread teardown faults and the baseline fallback cases in both profiles.
5. Run root formatting and three clippy configurations, targeted FPU/fault/SMP
   cases, then the complete normal `test.ps1`. Check a separate default-flavor
   VMware VM for enabled AVX2 and successful boot when a slot is available;
   Cameron's daily VM is untouched. No hard-float toolchain change or benchmark
   claim is part of this work.

Dependencies: none added.

References: [Intel SDM volume 1, chapter 13](https://cdrdv2.intel.com/v1/dl/getContent/671436),
[Intel instruction reference](https://cdrdv2-public.intel.com/789589/334569-sdm-vol-2d.pdf),
[AMD APM volume 2, section 11.5.10](https://docs.amd.com/api/khub/documents/sD1_QL~h4Afq2_tvzxqqSQ/content).
