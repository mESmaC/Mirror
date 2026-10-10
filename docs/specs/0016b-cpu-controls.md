# 0016b: CPU controls and vulnerability reporting

Status: confirmed (2026-10-09)
Depends on: 0015a, only for the manual VMware check (it boots the disk image); the code and QEMU tests depend on no other spec. In the M0 order it lands after 0013 and 0016a, though it does not need 0016a (see Interfaces), and before 0017a.
Ledger: S9, A9 (supports), A11 (relates), S6a (prepares 0021), G3a (test platforms)

## Goal
Every CPU runs with a known, verified set of low-cost CPU protections, and the kernel reports the CPU's speculative-execution exposure. When this spec is done:
- SMEP, SMAP and UMIP are enabled wherever the CPU reports them. While SMAP is on, the kernel always runs with RFLAGS.AC clear.
- EFER.SCE is explicitly off.
- The kernel reports Meltdown and MDS exposure and applies no mitigation (ledger S9).
- The default QEMU smoke CPU exposes SMEP and SMAP, so the whole fault battery runs with them on.

Serial markers prove each property on every online CPU. These are defence-in-depth layers under the immutable core's runtime-tamper protection (A9). They hold for every ring-3 process, whichever principal owns it (A11). They also fix the CPU-control contract that the SYSCALL path (0021) builds on.

## Non-goals
- KPTI, VERW/MDS buffer clearing, IBRS/IBPB and retpolines. Ledger S9 makes a KPTI/MDS mitigation spec a prerequisite before any third-party native code runs.
- KASLR, an NX direct map, and auditing or removing the Limine lower-half mappings copied into the kernel root.
- Guarded stacks and the NMI/#MC IST stacks (0016a). Catch-all IDT stubs and the other interrupt-robustness work (0055).
- The SYSCALL entry (0021), XSAVE (0053), CET shadow stacks and IBT, PKS.

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0016-D1 | Enable SMEP and SMAP on every CPU whose CPUID reports them, and fail any AP whose support differs from the BSP's. A CPU without them still boots and reports `smep=0 smap=0`. Rationale: hypervisors give every vCPU the same CPUID, the entry stub's CLAC keys off one global flag (item 2), and Intel Broadwell and AMD Zen onward have both. | confirmed (2026-10-09) |
| 0016-D2 | Enable UMIP when reported, so user SGDT/SIDT/SLDT/SMSW/STR raise #GP(0). Rationale: it costs nothing now, and a later compatibility layer that needs these instructions can emulate them in the #GP path, as Linux does. | confirmed (2026-10-09) |
| 0016-D3 | Force EFER.SCE=0 on every CPU, so a user `syscall` raises #UD and is contained as an ordinary user fault. 0021 sets SCE only together with STAR/LSTAR/SFMASK. Rationale: no code programs the SYSCALL MSRs, so an inherited SCE=1 would enter CPL0 at whatever LSTAR the firmware left. | confirmed (2026-10-09) |
| 0016-D5 | v1 speculative-execution policy, per ledger S9: detect and report only. The kernel reads the CPU vendor and IA32_ARCH_CAPABILITIES (RDCL_NO, MDS_NO), logs the exposure, and applies no KPTI or VERW. A KPTI/MDS spec is a prerequisite before any third-party native code runs. Rationale: only Mirror-built native code runs inside Mirror in v1 (external agents connect through the Agent API), and KPTI adds a CR3 switch to every entry and exit. | confirmed (2026-10-09, ledger S9) |
| 0016-D6 | The default QEMU smoke CPU becomes `qemu64,+smep,+smap` in run.ps1 and capture.ps1. A new run.ps1 switch, `-BaselineCpu`, keeps plain `qemu64` for one Normal boot per build profile. Rationale: the whole fault battery then runs with the protections VMware gives on current hosts. (Amended 2026-10-09: VirtualBox dropped, G3a amended.) | confirmed (2026-10-09) |

Decision in 0016a that also governs this spec:
- 0016-D7: automated acceptance runs in QEMU; VMware is checked by hand (ledger G3a, amended 2026-10-09: VirtualBox dropped).

## Interfaces
Line references are at HEAD 4614249; earlier M0 specs shift some of them. Work order: items 1-5. Each item is its own commit and runs the targeted `run.ps1` cases for what it touches. The full `scripts/test.ps1` runs in debug and release when the spec is complete (ledger P8). This spec does not need 0016a, which the README orders first. Both add `SmokeMode` fatal modes, run.ps1 fatal-set entries and `Slot` fields, so whichever lands second merges with the first. 0011 lands earlier, so the new `unsafe` blocks in init carry `// SAFETY:` comments.

1. **CPU controls.** A new `kernel/src/hardening.rs` provides `configure_current() -> Result<Controls, &'static str>`. It is called on the BSP after `fpu::init` (main.rs:55) and on each AP after `fpu::init_secondary` (smp.rs:418).
   - When the maximum basic CPUID leaf is at least 7 (checked as at interrupts.rs:235), it sets CR4.SMEP (bit 20, CPUID 7.0 EBX[7]), CR4.SMAP (bit 21, 7.0 EBX[20]) and CR4.UMIP (bit 11, 7.0 ECX[2]). It clears EFER.SCE (bit 0) and verifies everything by readback.
   - Nothing sets these bits today. CR4 is written only at fpu.rs:153 and by the PGE toggle at paging.rs:1650-1663. EFER is written only at fpu.rs:127-129 (clears FFXSR) and paging.rs:1514-1518 and 1609-1611 (set NXE). All of these read-modify-write sites preserve the new bits.
   - An AP whose `Controls` differ from the BSP's fails startup with a new `smp::fail` reason (smp.rs:485-489). APs record their CR4 and EFER in their `Slot` (smp.rs:47-66; 0016a's CR4 field is reused if it exists), and the BSP prints them.
2. **AC on entry.** `mirror_exception_common` executes `clac` next to `cld` (interrupts.S:119-120) when the global byte `MIRROR_SMAP_ACTIVE` is set. Every entry stub jumps there (interrupts.S:7-14). The BSP sets that byte before enabling SMAP; CLAC is legal on any CPU whose CPUID reports SMAP, whatever CR4 holds. No code patching is involved.
   - The trusted kernel continuation already runs with AC clear (interrupts.rs:674).
   - User AC is still preserved across returns (interrupts.rs:41).
   - The user copies go through the direct map after a page walk (paging.rs:1259-1328). So do ELF loading, demand-zero VM pages and window pixel buffers (userspace.rs:2355-2363, vm.rs:437-440, windows.rs:55-90). They therefore need no STAC windows.
3. **Vulnerability report (BSP).** The report reads the vendor, CPUID 1 ECX[31] (hypervisor), and IA32_ARCH_CAPABILITIES (MSR 0x10A) bit 0 RDCL_NO and bit 5 MDS_NO. It reads the MSR only when CPUID 7.0 EDX[29] is set.
   - AMD counts as not affected by Meltdown or MDS. Intel without the MSR counts as affected. Any other vendor is `unknown`.
   - The report changes nothing.
4. **Markers and probes.** These markers print in every build:
   - `MIRROR_CPU_HARDENING cpu=<n> cr4=<hex> smep=<b> smap=<b> umip=<b> sce=0 cpuid_smep=<b> cpuid_smap=<b> cpuid_umip=<b>`, one line per online CPU.
   - `MIRROR_CPU_VULN vendor=<intel|amd|other> hypervisor=<b> arch_caps=<b> meltdown=<affected|not_affected|unknown> mds=<same> mitigation=none`.

   These probes and markers exist in boot-smoke builds only:
   - `MIRROR_ENTRY_AC_OK user_ac=<n>`. While SMAP is active, dispatch asserts that live RFLAGS.AC is 0, and it counts entries whose saved user RFLAGS had AC set. To produce such entries, init's register-preservation probe (userspace/init/src/main.rs:958-990) also sets AC before `int 0x80`, checks it after the return, and clears it with DF before Rust resumes.
   - New fatal modes in `SmokeMode` (boot.rs:146-217), handled next to the existing ones (main.rs:391-399). Each prints `MIRROR_TEST_EXPECT_FATAL vector=14` first. `kernel-smep` calls into an RX user page holding `ret`; `kernel-smap` reads an RW user page. Each page is mapped in a fresh `UserAddressSpace` (paging.rs:844), which is activated. A probe that returns panics.
   - New user modes: `user-syscall` (probe 21, runs `syscall`, expects #UD) and `user-umip` (probe 22, runs `sgdt`, expects #GP(0)). They go in userspace.rs:2247-2279 and userspace/init/src/main.rs:1278-1281, and init's probe bound (userspace/init/src/main.rs:1097) rises to 22.
5. **Scripts and docs.**
   - run.ps1 gets `-Fault` values KernelSmep, KernelSmap, UserSyscall and UserUmip, and the `-BaselineCpu` switch, which it rejects together with `-ExtendedCpu`. It adds the kernel modes to its fatal set (run.ps1:472) and checks the per-CPU markers. Its fatal-diagnostic regex (run.ps1:790-796), which hardcodes `error=0x0` today, accepts a per-mode error code. Its CPU model (run.ps1:318) and capture.ps1's (capture.ps1:272) follow D6.
   - test.ps1 adds KernelSmep, KernelSmap and UserSyscall to the main battery (test.ps1:34), and one `-BaselineCpu` Normal boot per build profile.
   - AGENTS.md: the one-boot command line names `-BaselineCpu`, and the host-side fault line says the default smoke CPU is `qemu64,+smep,+smap`.

## Acceptance tests
1. `scripts/test.ps1` passes in debug and release with every new mode included, and the debug and release captures pass on `qemu64,+smep,+smap`.
2. Every smoke boot prints one `MIRROR_CPU_HARDENING` line per online CPU, cpu=0..N-1, all with `sce=0`: CpuCount lines on APIC runs, and one line on `-LegacyPic` runs, where SMP is skipped (run.ps1:58-70). On each line, smep, smap and umip equal their `cpuid_` values. On the D6 default model and on `-ExtendedCpu`, every line shows `smep=1 smap=1`. The `-BaselineCpu` run shows `smep=0 smap=0` and passes.
3. KernelSmep and KernelSmap each print `MIRROR_TEST_EXPECT_FATAL vector=14`, then `MIRROR_EXCEPTION vector=14` with `error=0x11` (present, instruction fetch) and `error=0x1` (present, read) respectively. Each has cr2 equal to the probe address and exits 35 (arch.rs:141-148).
4. UserSyscall gives `MIRROR_USER_FAULT_CONTAINED vector=6 error=0x0` and recovers normally (exit 33). UserUmip gives `vector=13 error=0x0`. It joins the `-ExtendedCpu` battery (test.ps1:71) if Open question 1 finds that `max` reports `cpuid_umip=1`; otherwise it stays out of the automated suite.
5. Every smoke boot prints exactly one `MIRROR_CPU_VULN` line in the format above. When SMAP is active, it also prints `MIRROR_ENTRY_AC_OK` with `user_ac` of 1 or more.
6. Manual check, after 0015a: boot the disk image in VMware Workstation with the reference settings in `docs/vm-settings.md` (0015-D11), serial output to a file. The shell works, and each vCPU prints one `MIRROR_CPU_HARDENING` line whose smep, smap and umip equal its `cpuid_` values. Record those lines and the `MIRROR_CPU_VULN` line as input for the S9 KPTI/MDS spec. Record the vmware.log `Monitor Mode:` line beside the `MIRROR_CPU_HARDENING` lines, since Host VBS mode hides some CPU features: VMware's 26H1 list of Host VBS mode limitations names RTM/HLE, PKU, PMCs and VT-x and says nothing about SMEP, SMAP or UMIP. The development host runs that mode (`Monitor Mode: ULM`, scratch power-on 2026-10-10). On a Broadwell/Zen or newer host, `cpuid_smep=0` is recorded as a hypervisor finding.

## Dependencies
None. No crates. The pinned QEMU 11.1.0 (.tools/qemu/VERSION) and OVMF suffice: QEMU TCG emulates SMEP, SMAP, CLAC and STAC.

## Open questions
1. Does QEMU 11.1.0's TCG `-cpu max` report UMIP? The first `-ExtendedCpu` boot's `cpuid_umip` answers this, and acceptance test 4 follows the answer.
