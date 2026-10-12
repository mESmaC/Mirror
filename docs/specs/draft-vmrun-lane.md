# Draft: VMware headless check lane

Status: draft (2026-10-12). All lane decisions and interfaces below are proposed, awaiting Cameron's number and confirmation. This document authorizes no implementation or VM operation.
Depends on: 0010, 0011, 0013, 0015a; merged 0017b including the 0017-D9 readback amendment for every guest case. Host-only preparation can precede 0017b. Uses 0018-D10's host library without resuming its benchmarks. Guest restart additionally requires confirmed, merged 0058a.
Ledger: G3a (uses S1a, S1c, S1d, S3, S7, S7a, S7b, S10, S15, P3, P5a, P8, P9; related G11, S16). Roadmap: RD1 remainder, RD7, RD16 and RD24 remain proposed in [roadmap-replan.md](roadmap-replan.md). RD1's remaining decision was due before 0013 ended; that point has passed.

## Goal

An explicitly invoked Mirror check boots a packaged image headless, verifies the correct console commands through VMware VNC, and checks console continuity and responsiveness after host suspend/resume. A guest-issued restart case joins after 0058a. Each run has bounded operations, exclusive ownership and preserved evidence; it produces functional results, not performance measurements.

## Non-goals

Other-OS downloads, setup and benchmarks remain deferred until Cameron requests them. This scope does not complete 0018, add perf markers, implement guest power/clock/Tools support, automate pointer checks, create VMs, edit VM hardware, take snapshots, change host security settings, or replace QEMU tests. Phase 5's 0058/0087c work can add soft-power cases later. Enforcing the team lock in all existing scripts is a separate task.

## Verified starting point

Repository behavior below was inspected at `970e48c`; named symbols identify the source contracts, rather than mutable line numbers. Branch evidence is labeled separately.

- [vm-settings](../vm-settings.md) records 0015a check 9 on Workstation Pro 26.0.1 build 25688693, hardware 22, UEFI, ULM and 1024x768. [0013](0013-firmware-and-pci-tolerance.md) records successful boots at 1 and 4 vCPUs, not suspend/resume. Its observed PCI counts are 42 functions/34 bridges; test 14 treats the bridge count as informational.
- `scripts/package.ps1` builds from the calling checkout and copies the default image/descriptor into `build/dist`; it records no provenance. `scripts/test.ps1::Assert-MirrorKernelImage` scans the staged ELF, not an earlier packaged image.
- `kernel/src/ps2.rs::prepare_keyboard` at this base accepts only readback `0x02`; 0017b records VMware returning `0x41`. Its amendment and fix were inspected at immutable commit `144678b` on the 0017b branch; they are absent from this base. `kernel/src/main.rs` permits the shell with either keyboard or mouse ready, and emits `MIRROR_BOOT_OK` before input-session setup.
- The existing machine lock is the directory `C:\Users\engin\Desktop\Mirror-locks\qemu`, described in `xros_chat/MIRROR-TEAM.md` and `claude/team-process@97cc86f`'s AGENTS/P3 update. Directory creation is atomic; `owner.txt` records nick, task, worktree and start time. Process exit does not remove it; another owner's or apparently stale lock is never reclaimed automatically.
- `bbc88e4` preserves **unreviewed WIP** RFB/frame code and a library in `tools/perf-probe`. Its tests are synthetic, with no VMware capture validation. The 1920x1080 benchmark check lives in its CLI. [0018-D10](0018-performance-baseline-harness.md) already assigns the std-only RFB client to this workspace.
- Installed `vmrun.exe -?`, read on 2026-10-12, reports version 1.17.0.25688693 and lists `start [gui|nogui]`, `stop/reset/suspend [hard|soft]`, and `list` of running VMs. This proves syntax only. Reliable power-state detection, live serial sharing/latency, log generations and resumed guest behavior are unverified prerequisites.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| VMR-D1 | Add explicit `boot`, `suspend-resume` and, later, `restart` cases. Missing prerequisites are `blocked`; an attempted check that fails is `fail`. Never omit a requested case or substitute host reset for guest restart. The lane does not block 0017c's manual checks. | proposed |
| VMR-D2 | Use one dedicated unencrypted wizard/vmcli-created VM, initially the reference 1 GiB/1 vCPU profile, with fixed disk/serial staging outside worktrees. Consume clean, immutable-source, hash-bound default packages. G11 requires a later explicit profile decision. | proposed |
| VMR-D3 | These cases use `vmrun -T ws start <vmx> nogui`, `suspend <vmx> hard`, and state-qualified `stop <vmx> hard`. They send no soft operations. Host command success alone never proves guest success. | proposed |
| VMR-D4 | Review and land the RFB/frame primitives once in `tools/perf-probe`, adding a functional `lane` command and testable Rust orchestration there. Keep `scripts/vmrun.ps1` thin. No duplicated RFB workspace, unrelated WIP cherry-pick or new third-party crate; preserve 0018's benchmark contracts. | proposed |
| VMR-D5 | Use only the selected VM's verified loopback listener: RFB 3.8, security None, Raw and DesktopSize. Before any key, require merged 0017b provenance, its fresh exact readiness marker, and pixel readiness. No pointer events; those await 0017c verification. | proposed |
| VMR-D6 | Acquire or explicitly adopt the existing directory lock exclusively. Bound every operation. Do not retrofit other script entry points or infer ownership from a matching name, age or absent PID. | proposed |
| VMR-D7 | Require proven off at entry and for a passing result. Preserve first failure and evidence; qualify cleanup by observed state. Unresolved state retains ownership and a durable recovery record. Explicit `Recover` only archives that record after proven off. | proposed |
| VMR-D8 | Match exact rendered command/history glyphs, not merely changed pixels. Send each key down/up once, never replay an ambiguous command, and attempt bounded release on error. Default serial remains free of user content. | proposed |

## Interfaces

### Entry and implementation boundary

Proposed wrapper: `scripts/vmrun.ps1 -Vmx <absolute-path> -LaneRoot <absolute-path> -LockOwner <nick/task> -Case boot|suspend-resume|restart`, or `-Recover` instead of `-Case`. Optional `-AdoptLock <handoff-token>` explicitly adopts the caller's lock; `-Vmrun <absolute-path>` overrides discovery. No implicit `all` case, build or packaging. Resolve VMware from the 64-bit `HKLM\SOFTWARE\VMware, Inc.\VMware Workstation` `InstallPath`; record executable/version. Use argument arrays and hidden processes, without credentials or shell interpolation.

The `perf-probe lane` state machine owns deadlines, power transitions, no-repeat rules, marker/oracle checks, cleanup, recovery and artifact publication. Injectable command runner, monotonic clock, serial source, state/listener query and artifact store make the same logic runnable with fake peers. Windows adapters return strictly parsed bounded data; PowerShell does not hold an alternative lifecycle state machine.

Future files: `tools/perf-probe/`, `scripts/vmrun.ps1`, `scripts/package.ps1`, host-check integration in `scripts/test.ps1`, and matching `AGENTS.md`/separate lane settings documentation. No kernel or userspace change, `common.ps1` marker retrofit, or cross-script lock retrofit. This revision changes only this draft.

### Package provenance and fixed staging

Proposed `package.ps1 -Lane [-Release]` selects a clean committed tree; ordinary packaging keeps its existing interface. Export only that commit into a private source directory with fresh build/target directories. Seal exported source files and directories against overwrite, deletion, replacement and added inputs for the entire build, allowing writes only to declared output roots and the existing verified tool cache. Proposed Windows sharing handles plus directory ACLs provide this seal; failure to establish it blocks packaging. A read-only file attribute or pre/post Git-status comparison is insufficient. Source path/content inventory must match the selected Git tree. Concurrent edits in the original worktree cannot enter this build; inherited source/output overrides must be rejected or normalized to the recorded build configuration. Require the pinned toolchain, locked crates and verified Limine archive already cached before invoking the snapshot build; enforce offline dependency resolution and block missing inputs instead of downloading them.

Build the snapshot's default flavor, with no `agent-demo` or perf token. After verified copies of both artifacts, atomically publish `build/dist/mirror.package.json` last: schema/generation, commit/tree, clean status, debug/release, exact features, toolchain, source-inventory digest, oracle version and image/descriptor sizes/SHA-256 hashes. Every packaging mode invalidates any prior completion record before replacing artifacts; only `-Lane` publishes a new one. Failure leaves no acceptable mixed generation. This record describes the snapshot actually built, never the later checkout HEAD. Future prerequisite commits must be confirmed and ancestors of the recorded clean source commit; absent or unknown 0017b/0058a capability evidence blocks before power-on. A scan of mutable `build/esp/kernel.elf` is not a substitute.

The owner prepares the dedicated VM once with disk `<LaneRoot>/disk/mirror.vmdk` and serial `<LaneRoot>/serial.log`. Under the machine lock and positive off evidence, the lane copies the selected worktree's package pair to those fixed paths through temporary files, validates both hashes, and publishes a staging generation last. The descriptor's relative `mirror.img` extent permits this unchanged (`tools/mirror-image/src/vmdk.rs::descriptor`). No snapshots, parent chain or extra data disk. After proven off, compare both staged artifacts byte-for-byte by hash; unexpected mutation fails and preserves evidence, without normalization or silent restoration. Recovery state lives under this fixed root so another worktree cannot bypass it; package output in another checkout cannot rewrite the VM's disk.

### VM profile, ownership and external prerequisites

Create the VM with Workstation's wizard or `vmcli VM Create ... -c other-64`, then apply the reference settings in a separately documented lane profile. Preflight requires hardware 22, UEFI/Secure Boot off, 1 GiB/1 vCPU, SATA, e1000, HPET, no USB, reference 3D/RTC/unattended-start keys, `pciBridge0.present`, and `pciBridge4` through `pciBridge7` present with `virtualDev=pcieRootPort` and `functions=8`. Do **not** require those latter fields on `pciBridge0`: the installed Workstation-created VM has them only on 4–7. Record creation method and validated settings. Keep the firmware-selected mode; the initial reviewed oracle supports the recorded 1024x768 mode. Another mode needs a reviewed oracle, not a forced resolution change.

Add `RemoteDisplay.vnc.enabled=TRUE`, `.ip=127.0.0.1`, a reserved `.port` in 5901–6001, and leave `.password`, `.key` and `.keyMap` absent. Reject encrypted/unreadable/duplicate or contradictory configuration, occupied port, unexpected devices, escaping paths and reparse points in lane/evidence roots. Bound VMX input to 1 MiB, manifests/recovery records to 256 KiB, and owner records to 16 KiB; identity records use versioned schemas with unknown/duplicate fields rejected. Verify listener address and owning VM process after start before connecting; no automatic firewall change. No existing user's VM is repurposed by the lane.

Create the team lock directory atomically, then write its owner record. An existing directory is busy unless its owner issues an explicit handoff token after stopping its prior work. Both acquisition and adoption hold the same exclusive write/delete-excluding lease on `owner.txt` throughout execution and finalization. Adoption verifies nick/task, canonical worktree and token through that handle, consumes the single-use token and durably records the active run-id/state before work begins. A competing creator/adopter cannot obtain the lease; a stale invocation cannot reuse a consumed token after finalization. Every later adoption requires a fresh explicit handoff after the owner stops its other work. Legacy records need explicit owner handoff, never silent adoption. This is one protocol inside the existing directory, not a second machine lock. Only a lane-created directory can be removed by the lane after proven off; a caller-owned directory remains for its owner. Never reclaim by age, kill another VM, or delete VMware `.lck` files.

Before lifecycle execution, establish and document installed-platform indicators that distinguish off, running, suspended and transitioning states, and identify the listener owner. `vmrun list` alone is not off evidence. Also establish concurrent reads of the file-backed serial port, bounded delivery latency and fresh-generation tracking. The QEMU reader in `scripts/capture.ps1` proves none of those VMware properties. If live serial cannot supply the keyboard gate, typed cases remain blocked; checking all markers only after shutdown would violate VMR-D5. Changing serial transport requires a separate confirmed choice. These are implementation gates, not assumptions filled in by retries.

### Guest evidence and pixel oracles

Every initial or restarted boot has its own captured serial epoch. Before its first key require exactly one full line `MIRROR_KEYBOARD_READY transport=i8042 set=2 irq=1`, with no keyboard-skipped record. Validate exactly-once/full-field boot records and the 0013 profile checks: `MIRROR_BOOT_OK`, framebuffer readiness, enabled/online CPUs equal configuration, xAPIC, LAPIC timer, no ACPI skip/table skip, PCI `omitted=0`, AHCI/NIC/SVGA catalog records. The observed one-vCPU IOAPIC line is `MIRROR_APIC_IOAPIC_ID index=0 firmware=1 hardware=0`, preceding APIC readiness. Record 42 functions/34 bridges and flag deviations for review; these were informational in 0013, not universal rejection constants. Enforce actual creation keys and device records independently.

Use complete marker schemas from `kernel/src/{main,apic,smp,timer,pci,drivers}.rs`, rejecting duplicates, malformed or unknown members of checked families and unexpected fallback/fatal/trace/test output. VMware AHCI requires, in order:

```text
MIRROR_AHCI_READY sectors=2097152 readonly=1 port=0
MIRROR_AHCI_PROBE_OK fixture=0 reads=1
MIRROR_AHCI_CLEANUP_OK dma_reclaimed=1
```

Implement these assertions in the Rust lane; `common.ps1::Assert-MirrorAhciMarkers` hardcodes QEMU's 2048-sector disk and is not reusable unchanged. The e1000 catalog entry establishes device presence, not working networking.

Readiness and output fixtures are versioned against the packaged source's glyph tables, geometry and UI tokens. `userspace/desktop/src/shell.rs::Shell::paint` renders `MIRROR`, `Native desktop`, `Esc returns to console` and the two workspace welcome lines; match these complete glyph regions at their source-defined positions, with the centered surface offset, to distinguish the shell from a static boot screen. After one Escape, require fresh `MIRROR_SHELL_CLEANUP_OK`, `MIRROR_CONSOLE_READY input=ps2 display=1`, and the console header, its exact two initial history rows and empty edit row. Source ordering matters: `input::Session::new` drains before the shell runs; `run_console` drains before `Driver::render_console`. BOOT_OK and a stable screenshot alone are insufficient.

Console matching uses `kernel/src/framebuffer.rs::{ConsoleGeometry,glyph,draw_console_history,draw_console_input}` and `console.rs::{Console::process,Text::counter,parse}`. At 1024x768, history begins at (48,136), row spacing is 24 pixels, submitted prompt text starts at x=72, and edit text starts at (72,664), with scale 2 and 12-pixel character advance. Compare exact glyph/background colors from the recorded UI tokens, including blank suffix and caret; no OCR, guessed tolerance or current-unrelated-build template. Submitted text remains visible as a prompt history row.

For `status`, before Enter require exactly that edit text and expected caret. After one Enter require empty input, unchanged previous history, and exactly three appended rows: prompt `status`, `Timer ticks: <u64>`, `Keyboard IRQs: <u64>`. Match the prefixes and decimal glyphs (1–20 digits, bounded u64, no leading zero except zero), not predicted counter values. For `echo <sentinel>`, require the exact edit text, then its prompt plus exact sentinel output. Use 16 lowercase hexadecimal characters from the unique run identity. Reject extra rows, unknown-command/input-loss notices, stale history, missing/doubled keys and partial frames. Default serial must not contain the sentinel or other typed content.

### Cases, budgets and recovery

| Case | One-shot sequence after validated staging/off state |
| --- | --- |
| `boot` | Start; validate fresh boot and shell; Escape; console readiness; exact `status`; capture evidence; stop hard once; prove off and validate artifacts. |
| `suspend-resume` | Boot sequence through `status`; exact echo sentinel; preserve history and serial segment; close VNC; suspend hard once; prove suspended; dwell 2 s; start nogui once; reconnect; require identical retained history/sentinel, same boot epoch and no fatal marker; exact new `status`; stop and prove off. |
| `restart` | Only after 0058a's confirmed implementation/diagnostic contract is incorporated: checked boot/console/status; exact `restart` edit row; one Enter; distinct fresh boot epoch and keyboard gate; shell/console readiness and new exact `status`; stop and prove off. No host reset or repeated Enter/start rescue. |

The suspend sequence has 2 → 5 → 7 → 10 history rows, below the current capacity of 12. No clock resynchronization, timer rearm or Tools resume behavior is inferred. After resume the original fresh boot's keyboard gate remains applicable only with proven continuity; a fresh reboot fails this case.

All waits use total monotonic deadlines, including retries; progress does not reset them. Proposed caps:

| Operation | Cap |
| --- | --- |
| Active case, including preflight/staging | 900 s total; busy ownership fails immediately. |
| Package validation/staging; each state/listener phase | 120 s; 15 s respectively, with at most 32 queries per phase. |
| Each lifecycle command; each guest-ready phase | 60 s; 120 s respectively. RFB connection attempt ≤3 s, capped by remaining time. |
| Each typed action including its before/after frames | 30 s; each event write ≤100 ms; ≤32 key pairs per action, ≤64 per case. |
| Evidence and cleanup after active-case termination | 30 s for immediate key release and evidence, ≤60 s for a permitted stop, ≤15 s off verification; 105 s total. |
| Recover | 120 s total, no power operation. |

The longest present key sequence is suspend-resume: Escape + two `status`/Enter actions + `echo `, 16-character sentinel and Enter = 37 pairs. Reserve one additional key-up attempt outside the pair count for error cleanup. Release a held key before failing; an ambiguous input outcome never triggers a resend. Keep one RFB update request outstanding; timeout does not permit a second request. Resize invalidates all pixel coverage and geometry. Retain the WIP bounds (4096 pixels per dimension, 4096 rectangles/update, 128 MiB/message, 1 MiB/string), subject to review; obtain a fresh complete frame before comparison. Malformed input fails closed.

Immediately after ownership is established and before staging, journal a durable fixed-root `preparing` record with owner/run identity, VMX, source/staging paths and last proven state. Partial staging is recorded as incomplete, not as a valid disk generation. Before each power operation, including the guest-restart Enter, durably update that record with the intended transition and last proven state. A crash leaves an incomplete run that blocks starts/restaging from every worktree. On failure retain the first error, immediately attempt key release, then capture bounded evidence. If the owned VM is positively running with no unresolved operation, stop hard once and verify off. If suspended, unknown or transitioning, do not repeat start or race an in-flight command; retain the recovery record and directory lock for the owner. A timed-out `vmrun` process may be terminated, never its VM process; that does not establish VM state. Do not promise a hard stop can clear a suspended state. Supervised resume/stop or discarding suspended state requires verified operator instructions outside automatic recovery.

`-Recover` uses the same explicit ownership/adoption rules with a fresh handoff, positively verifies off and journal/VMX/staging identity, and preserves remaining evidence. It handles `preparing` with partial or absent staged files, an active operation, and `finalized-off` with an archive pointer; it never requires an incomplete copy to match a completed generation. Archive operation evidence once and record completion idempotently; repeated recovery revalidates off and never deletes history. Missing or incomplete ownership identity requires explicit manual resolution, never guessed stale-lock reclamation. It never starts the VM or silently repairs a changed image. Suspended/unknown state remains blocked. QEMU scripts do not read these recovery records; nevertheless the existing shared lock continues to block heavy runs while this Mirror VM might be active. Fixed staging protects unrelated worktree package output. Once safe, remove only a lane-created lock; caller-owned locks remain with their owner.

### Evidence

Use a new `build/vm-logs/lane/<run-id>/` per invocation. Record package provenance, staging hashes, validated VM settings, executable/monitor versions, oracle version, deadlines, step results and final power/cleanup state. Preserve old logs before start and serial file identity/offsets across transitions; save each generation before a possible replacement. Unexplained truncation, missing early lines or an unprovable epoch boundary fails. Revalidate all serial evidence after off. After off proof and durable evidence publication, finalization writes a durable `finalized-off` journal state with its evidence/archive pointer before archiving operation details. Keep that journal through ownership release (or return to the caller), so a crash after archive but before release remains recoverable. A finalized journal may remain as history; the next invocation verifies the final manifest and renewed off state before beginning a new generation. If ownership still names the interrupted run, explicit recovery is required. Journal transitions are atomic and preserve the first failure; incomplete preparation and interrupted finalization never count as a passing run.

Save named checkpoint PPMs: boot's shell, console, status-edit and status-result (four); suspend adds echo-edit, echo-result, retained-history and another status pair (nine total); restart adds restart-edit, new shell/console and another status pair (nine). Allow at most 12 normal checkpoints plus four reserved failure/cleanup frames (16 total); polling frames are not all saved. Other caps: 1 MiB per command-output stream, 16 MiB command output per run, 64 MiB aggregate per serial/VMware log category across generations. Overflow fails while retaining the prefix and truncation reason. Publish the final manifest atomically as pass/fail/blocked; no final record means incomplete. Never erase first-failure evidence after successful cleanup. Synthetic fixtures and captured VMware evidence are labeled separately; no secrets or arbitrary guest content are collected.

## Acceptance tests

1. **Library and fake orchestration.** Run the concrete commands below; add them to `test.ps1` and the matching AGENTS testing guidance when implemented. Tests exercise the actual Rust state machine with fake command/clock/serial/listener/artifact adapters: both lock modes and competing adoption; stale-token replay after finalization; crashes during staging before first power, after archival and before ownership release; repeated recovery of each journal state; malformed owner and state records; wrong VM/listener; missing live keyboard gate; no duplicate power/input action after timeout; wrong/duplicate/rotated markers; resize coverage; key-up failure; suspended/unknown cleanup; cross-worktree recovery; and evidence/cap failures. Unknown command, doubled/missing letters, incorrect digits/sentinel, cursor-only changes and stale/partial images must all fail. Review the reused WIP primitives and retain malformed-protocol/deadline tests.

```powershell
cargo fmt --all --manifest-path tools/perf-probe/Cargo.toml -- --check
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml
```

2. **Packaging fixtures.** The host test command includes a Windows fake-compiler fixture exercising the production package adapter in temporary roots: while it waits, overwrite/delete/rename/add-module/add-build.rs attempts against sealed source must fail; edits in the original checkout must not affect exported input. Missing seal, dirty initial source, missing cached inputs, attempted network dependency resolution, inherited overrides, failed first/second copy, stale/mixed manifest and hash mutation must block publication/use. Normal fixtures require no VM or OS build. Then run actual `package.ps1 -Lane` and `-Lane -Release` sequentially under the team lock, verifying manifest, clean source/tree identity and both hashes; run the repository's targeted packaging checks.
3. **Platform gates and VMware.** Only after 0017b merges and the state/listener/live-serial prerequisites are demonstrated, run boot and suspend-resume once each for debug and release on the dedicated profile. Save exact status/sentinel frames, fresh serial generations, actual display/monitor values, final off proof and unchanged artifact hashes. Verify no typed-content, trace or smoke leakage. A real RFB resize fixture is optional if no resize occurs; do not manufacture capture provenance. No live serial feasibility means blocked before input.
4. **Restart and recovery.** A package lacking confirmed 0058a capability is blocked before start. After its contract lands, run restart once per profile with exactly one guest request and a new epoch. Fake tests prove recovery cannot clear suspended/unknown state, adopt another owner's lock or delete history; a supervised actual failure/recovery check must preserve evidence and prove off before releasing lane ownership.
5. **Integration.** Run the full `test.ps1` before merge per P8, plus `-AgentDemo` if required by the code touched. The VMware cases remain opt-in. Report every attempted and blocked case; no other-OS artifact or benchmark result is produced.

## Dependencies

No new third-party crates or test framework. Rust `std`, pinned Rust 1.99.0 and existing MSVC/Windows SDK; existing Git and Windows PowerShell/.NET for source export, Windows adapters and hashes. Existing VMware Workstation Pro 26.0.1 build 25688693/`vmrun` 1.17.0.25688693 is proprietary Broadcom software, not linked or redistributed. Different tool builds require recorded revalidation, not automatic downloads.

## Open questions

1. Cameron: number and confirm VMR-D1–D8, including the clean snapshot packaging mode and initial reference profile versus RD16's G11 profile. RD1's remaining placement decision is overdue; this lane does not change the ledger or order itself.
2. External gates: establish reliable power-state/listener identity and concurrent file-serial delivery/generation behavior on the installed VMware build. If the required live marker cannot be obtained, bring back a transport decision; do not weaken the input gate.
3. Establish Raw pixel fidelity with reference 3D settings and retained console/input behavior after suspend/start. Failure is evidence for the guest owner, not permission to change VM settings. The actual artifact-mutation policy remains byte-exact; unexpected VMware rewrites require a new decision.
4. 0058a must supply the restart command, diagnostic and package-capability contract before that case is implemented. Phase 5 soft power, clock/timer resume and 0017c pointer automation remain separate work.
