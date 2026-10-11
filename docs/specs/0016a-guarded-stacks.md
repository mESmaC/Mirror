# 0016a: Guarded stacks

Status: done (2026-10-11, branch `claude/0016a-guarded-stacks`: items 1-5 in 9b0bbcf, 76e2606, 4764c45, 4b6ed35 and 36f4631, review fixes in 533f44a). All acceptance tests pass. `scripts/test.ps1` and `-AgentDemo` each ran 122 smoke boots, KernelStackOverflow included: it gives vector 8 with cr2 0xffffd100000427f8, inside the page at `rsp0_guard`, and exit 35. Every CPU line shows `ist=3 mce=1 cpuid_mce=1` at 1, 2, 4 and 8 CPUs. `nm -S` on the release kernel lists neither removed symbol, and .bss is now 26,232 bytes. Test 7 ran in VMware, in a Workstation-created VM with two boots each at 1 and 4 vCPUs: every boot printed one `MIRROR_CPU_STACKS` line per vCPU with `ist=3 mce=1 cpuid_mce=1`, `MIRROR_STACKS_READY`, and the shell started; the keyboard is 0017b's. The Interfaces keep their HEAD 4614249 line numbers. (Confirmed 2026-10-09.)

Implementation notes (2026-10-11):
- `map_guarded` is an `unsafe fn`, because its callers own the mapping preconditions. It limits a stack to 64 pages and fails with the new reason `stack_layout_invalid`. `map_set` maps a set of stacks and `map_sets` a run of sets, each rolling back on failure, and `smp::allocate_stacks` uses `map_sets`. A boot-smoke `stacks::self_test` checks bad placements, zeroing, guard refusal and rollback with exact frame counts.
- The window before `activate_secondary` (item 3): from the IDT load on, only a #DF triple-faults. An NMI or #MC pushes to the unmapped IST2 or IST3 page and raises a #PF, which `secondary_fault` records as vector 14. Item 3's "triple-faults" holds for #DF only.
- `cpu=` is the slot index; with every AP online it runs 0..N-1. `ist=` is measured, as the number of TSS IST1-3 fields whose readback equals the CPU's guarded tops. The scripts require `cpuid_mce=1` on QEMU's CPU models, stricter than `mce` equal to `cpuid_mce`.
- `capture.ps1` also checks the stack markers on trace and default boots. `test.ps1`'s image scan requires `MIRROR_CPU_STACKS` and `MIRROR_STACKS_READY` in trace and default kernels and forbids `MIRROR_IST_OK` there.
- The BSP region packs into 102 pages, and each AP slot takes 36. `MIRROR_FRAME_POOL allocated=` grows by the stack frames; nothing asserts it. `MIRROR_IDT_READY` still prints `double_fault_ist=1`.
Depends on: 0015a, only for the manual VMware check (it boots the disk image); the code and QEMU tests depend on no other spec. In the M0 order it lands after 0013, so that check runs with 0013's firmware and PCI tolerance in place, and before 0016b.
Ledger: A9 (supports), G3a (test platforms), S17 (supersedes D4 for the panic path)

## Goal
No kernel stack on any CPU can overflow silently into neighbouring kernel data, and NMI and #MC never run on the stack they interrupted. When this spec is done:
- The BSP's scheduler stack, its RSP0 trap stack and its IST stacks sit in guarded mappings, as the AP stacks already do. After early boot the BSP no longer runs on Limine's stack.
- Every CPU has guarded IST1 (#DF), IST2 (NMI) and IST3 (#MC) stacks, and sets CR4.MCE wherever the CPU reports MCE.
- A boot-smoke probe overflows the RSP0 stack and proves that the overflow lands on #DF via IST1.

Serial markers prove each property on every online CPU. This is defence in depth under the immutable core's runtime-tamper protection (A9). The CPU controls and the vulnerability report are 0016b, which is independent of this spec.

## Non-goals
- SMEP, SMAP, UMIP, EFER.SCE and the CPU vulnerability report (0016b).
- Non-fatal NMI handling, which waits for the first feature that needs a CPU to continue after an NMI, such as a watchdog (D4). The panic-time NMI stop of the other CPUs (ledger S17) is implemented in 0051. MCA bank decoding.
- Catch-all IDT stubs, EOI of unexpected vectors, not blaming user processes for EXT=1 faults (interrupts.rs:901), and LINT1 NMI on the BSP only (APs program LINT1 from the MADT today, apic.rs:595-599). These belong to 0055.
- A #DB IST (0021). Per-thread kernel stacks (0050): 0050 allocates them after the root is sealed, when `map_kernel_page` refuses new leaves (paging.rs:606-608), so it needs a shootdown-aware guarded mapping path. `stacks.rs` gives it only the layout rules.
- Changing Limine's stack request (boot.rs:52). It stays as it is, and its region stays reserved and idle (see Open questions).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0016-D4 | NMI and #MC get dedicated IST stacks on every CPU (IST2 and IST3) and stay fatal: diagnose and halt, as today. CR4.MCE is set when reported, so a machine check reaches the handler instead of shutting the CPU down. Rationale: no current feature uses NMIs, so non-fatal handling waits for one (watchdog, crash IPI). (Amended 2026-10-09: superseded for the panic path by ledger S17. On panic the panicking CPU stops the others with an NMI IPI, the crash-IPI case this decision deferred; 0051 implements it, including the NMI handler's panic-stop path. This spec's NMI and #MC handling is unchanged.) | confirmed (2026-10-09) |
| 0016-D7 | Automated acceptance runs in QEMU. VMware is checked by hand. (Amended 2026-10-09: VirtualBox dropped, G3a amended.) | confirmed (ledger G3a) |

## Interfaces
Line references are at HEAD 4614249; earlier M0 specs shift some of them. Work order: items 1-5. Each item is its own commit and runs the targeted `run.ps1` cases for what it touches. The full `scripts/test.ps1` runs in debug and release when the spec is complete (ledger P8).

1. **Guarded stack helper.** A new `kernel/src/stacks.rs` provides `map_guarded(tables, frames, bottom, pages) -> Result<u64, &'static str>`, which returns the stack top.
   - It maps zeroed frames and rolls back its own pages on failure, as smp.rs:375-389 does. It applies the guard check from smp.rs:347-355: the page below and the page above must be unmapped, both before and after mapping.
   - Every stack has its own unmapped page below and above, so neighbouring stacks are separated by two unmapped pages, as in the AP layout (smp.rs:186-191). A fault frame pushed just below a guard page therefore faults too. One page per side is enough because the x86_64-unknown-none target emits inline stack probes for frames larger than a page.
   - `smp::allocate_stacks` (smp.rs:339-391) switches to it.
2. **BSP stacks.** A BSP region at the start of the stack arena (smp.rs:29), ahead of the AP slots, holds a 256 KiB scheduler stack replacing Limine's stack (boot.rs:52), a 64 KiB RSP0 stack replacing the static `KERNEL_STACK` (userspace.rs:58-59, removed), and IST1 #DF, IST2 NMI and IST3 #MC stacks of 16 KiB each.
   - The region is mapped right after `heap::init` (main.rs:121). That is before `share_with_secondaries` (paging.rs:122-126) seals kernel mappings, after which `map_kernel_page` refuses new leaves (paging.rs:606-608), and before any process root copies the upper half (paging.rs:861-866).
   - `_start` then loads RSP with the scheduler stack top and calls a `-> !` continuation, using the pattern of smp.rs:433-436. The continuation receives the state built so far (boot info, `PageTables`, `FrameAllocator`) and runs the rest of `_start`.
   - Until this switch, BSP IST1-3 all point at the existing static 16 KiB stack (interrupts.rs:174, installed at 298-306). The switch rewrites the BSP TSS IST fields and reads them back.
   - The three RSP0 installs (userspace.rs:698-701, 1164-1167, 1910-1912) use `stacks::bsp_rsp0_top()`.
3. **AP stacks.** Each AP arena slot gains guarded 16 KiB IST1-3 stacks.
   - `interrupts::init_secondary` takes their tops the way it takes RSP0 today (interrupts.rs:428-467) and verifies all three by readback (as at interrupts.rs:521-524).
   - The static `SECONDARY_DOUBLE_FAULT_STACKS` (interrupts.rs:186-187, 1008 KiB of .bss) is removed.
   - The arena is mapped only in Mirror's root, and an AP runs on Limine's root until `paging::activate_secondary` (smp.rs:415-429). A #DF, NMI or #MC in that window therefore triple-faults (QEMU exits under `-no-reboot`) instead of being recorded, as a #DF there does today. LINT1 stays masked until `apic::init_secondary` (smp.rs:459). The SAFETY comment at smp.rs:413-414 is updated.
   - The layout asserts in the smp self-test (smp.rs:1224-1230) are updated.
4. **IDT and CR4.MCE.** Vector 2 uses IST2 and vector 18 uses IST3; today only vector 8 has an IST (interrupts.rs:310-315). Dispatch stays fatal for 2, 8 and 18 (interrupts.rs:899-904, 944-948).
   - `interrupts::init` and `interrupts::init_secondary` set CR4.MCE (bit 6) after the IDT is loaded, when CPUID 1 EDX[7] is set, and verify it by readback. On an AP a mismatch makes `init_secondary` return false, which fails startup with the existing reason 2 (smp.rs:415-417); on the BSP it panics.
   - CR4 is otherwise written only at fpu.rs:153 and by the PGE toggle at paging.rs:1650-1663. Both read-modify-write CR4 and preserve the bit.
   - APs record CR4 in a new `Slot` field next to `actual_stack` and `actual_root` (smp.rs:47-66), and the BSP prints it.
5. **Markers, probes and scripts.** These markers print in every build:
   - `MIRROR_CPU_STACKS cpu=<n> ist=3 mce=<b> cpuid_mce=<b>`, one line per online CPU, printed by the BSP after SMP bring-up (AP lines from their `Slot`).
   - `MIRROR_STACKS_READY scheduler=256 rsp0=64 ist=3 rsp0_guard=<hex>`, once.

   These probes and markers exist in boot-smoke builds only:
   - `MIRROR_IST_OK nmi=2 mc=3`. On the BSP, the probe runs `int 2` and `int 18` at CPL0 against an armed expectation (modelled on the int3 test, interrupts.rs:923-926). The handler checks that its frame lies inside the matching IST stack, then returns.
   - A new fatal mode, `kernel-stack-overflow`, in `SmokeMode` (boot.rs:152-223), handled next to the existing ones (main.rs:393-401). It prints `MIRROR_TEST_EXPECT_FATAL vector=8`, sets RSP to the RSP0 stack top minus 2 KiB, then repeats `sub rsp, 4096` and `mov qword ptr [rsp], 0`. The write that reaches the guard page faults with RSP inside that page, so pushing the #PF frame faults again and the CPU raises #DF on IST1.

   Scripts and docs:
   - run.ps1 gets `-Fault KernelStackOverflow` and adds it to its fatal set (run.ps1:490) with vector 8 and `error=0x0`, which its fatal-diagnostic regex (run.ps1:846-852) already expects. It checks that cr2 lies in the page at `rsp0_guard`. On every smoke boot it checks the `MIRROR_CPU_STACKS` lines, `MIRROR_STACKS_READY` and `MIRROR_IST_OK`.
   - test.ps1 adds KernelStackOverflow to the main battery (test.ps1:103).
   - readme.md line 7 says every CPU has guarded stacks.

## Acceptance tests
1. `scripts/test.ps1` passes in debug and release with KernelStackOverflow included.
2. Every smoke boot prints one `MIRROR_CPU_STACKS` line per online CPU, cpu=0..N-1: CpuCount lines on APIC runs, and one line on `-LegacyPic` runs, where SMP is skipped (run.ps1:60-72). Every line shows `ist=3`, and its `mce` equals its `cpuid_mce`.
3. Every smoke boot prints `MIRROR_IST_OK nmi=2 mc=3` and exactly one `MIRROR_STACKS_READY` line in the format above.
4. KernelStackOverflow prints `MIRROR_TEST_EXPECT_FATAL vector=8`, then `MIRROR_EXCEPTION vector=8 error=0x0` with cr2 inside the 4 KiB page at `rsp0_guard`. QEMU exits with 35 (isa-debug-exit value 0x11, arch.rs:141-148), and no `MIRROR_BOOT_OK` appears.
5. The 2-, 4- and 8-CPU runs bring every AP online with the new slot layout. `MIRROR_SMP_GUARD_OK` and the SmpUnmap and SmpTlbTimeout modes still pass.
6. Measurement: `nm -S target/x86_64-unknown-none/release/mirror-kernel` lists no `SECONDARY_DOUBLE_FAULT_STACKS` or `userspace::KERNEL_STACK` symbol. Today they occupy 0xfc000 and 0x10000 bytes of .bss.
7. Manual check, after 0015a: boot the disk image in VMware Workstation with the reference settings in `docs/vm-settings.md` (0015-D11), serial output to a file. The shell works, and each vCPU prints one `MIRROR_CPU_STACKS` line with `ist=3` and `mce` equal to `cpuid_mce`.

## Dependencies
None. No crates. The pinned QEMU 11.1.0 (.tools/qemu/VERSION) and OVMF suffice. Test 6 uses GNU nm (binutils, GPL-3.0-or-later), a host-only tool already installed with MinGW; it is neither shipped nor linked.

## Open questions
1. Does Limine's stack-size request (boot.rs:52) also size the AP bootstrap stacks? If it does not, a later change could shrink the request to 64 KiB, since only early boot uses it after this spec, and save 192 KiB on the BSP. This spec leaves the request unchanged.
