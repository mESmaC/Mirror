# Draft: VMware headless check lane

Status: draft (2026-10-12). Every recommendation below is **proposed**, not approval to implement or operate a VM. Cameron chooses the alternatives separately and assigns spec numbers.
Depends on: 0010, 0011, 0013, 0015a; merged 0017b including 0017-D9's readback amendment for guest cases. Host-only work can precede 0017b. Uses 0018-D10's library without resuming benchmarks. Guest restart also requires confirmed, merged 0058a.
Ledger: G3a (uses S1a, S1c, S1d, S3, S7, S7a, S7b, S10, S15, P3, P5a, P8, P9; related G11, S16). [Roadmap](roadmap-replan.md): RD1 remainder, RD7, RD16 and RD24 remain proposed; RD1's remaining decision was due before the now-completed 0013 ended.

## Goal

An opt-in check boots a packaged Mirror image headless, verifies exact console commands through VMware VNC, and checks continuity after host suspend/resume. A guest restart case joins after 0058a. Results are functional outcomes with bounded operations and preserved evidence, not performance measurements.

This remains an approval proposal until Claude remaps it. Before implementation, split it into numbered **host library/packaging** and **VMware cases** specs, retaining these decision IDs: the former can land before 0017b; the latter depends on platform experiments. Keeping the shared contract here avoids premature numbering or duplicated interfaces.

## Non-goals

Other-OS downloads, setup and benchmarks remain deferred until Cameron requests them. No completion of 0018, new guest markers, guest power/clock/Tools implementation, pointer automation, snapshots, host security changes, CI or replacement of QEMU tests. The runner does not create/reconfigure VMs; proposed manual provisioning is a separate owner action below. Phase 5 can add soft-power cases later. Cross-script lock enforcement and hostile-process source isolation are outside the recommended scope.

## Verified starting point

Source behavior was inspected at `970e48c`; the current local `origin/main`, `58461e0`, changes none of the cited code. Branch evidence is distinct:

- [vm-settings](../vm-settings.md) records Workstation Pro 26.0.1 build 25688693, hardware 22, UEFI, ULM and 1024x768. [0013](0013-firmware-and-pci-tolerance.md) records successful 1/4-vCPU boots, not resume; its 42 functions/34 bridges are observations, with the bridge count explicitly informational.
- `package.ps1` copies a default image without provenance. `Assert-MirrorKernelImage` scans the staged ELF, not an older package. [0015a](0015a-disk-image.md) asks whether VMware preserves the descriptor after boot; the inspected completion notes do not record that result.
- At `970e48c`, `ps2::prepare_keyboard` accepts only `0x02`; current 0017b code is `14cb7b2`, unchanged from inspected `67cea58`, with completion notes at `ae9aa3e`. Its D9 amendment accepts VMware's `0x41`; the notes record ten successful VMware cold boots before rebase. Pre-merge branch references are evidence, not the future merged capability identifiers.
- The existing lock is `C:\Users\engin\Desktop\Mirror-locks\qemu`, with human-readable nick/task/worktree/start-time ownership (`xros_chat/MIRROR-TEAM.md`; `claude/team-process@97cc86f`, AGENTS/P3). It survives process exit; apparent staleness never authorizes takeover.
- `bbc88e4` preserves **unreviewed WIP** RFB/frame primitives and a library in `tools/perf-probe`, with synthetic tests and no VMware capture validation. [0018-D10](0018-performance-baseline-harness.md) assigns RFB to this workspace; its benchmark CLI's 1920x1080 rule is separate.
- Installed help read on 2026-10-12 reports `vmrun` 1.17.0.25688693 and start/stop/suspend syntax. Help proves neither reliable power-state detection nor live serial, resume, recovery or disk-preservation behavior.

## Decisions

Each row states the recommendation, reason and a real alternative; implementation follows only the chosen, confirmed options.

| ID | Recommendation, rationale and alternative | Status |
| --- | --- | --- |
| VMR-D1 | Recommend opt-in boot and suspend/resume, with restart added after 0058a, split into host and guest specs. This keeps host work useful while guest prerequisites settle. Alternative: one combined implementation, with all work waiting for VMware verification. | proposed |
| VMR-D2 | Recommend the dedicated unencrypted reference profile: 1 GiB/1 vCPU, default flavor and firmware-selected display mode. It has existing boot evidence. Alternative: resolve RD16 first and use G11; that expands resource/profile and oracle validation. | proposed |
| VMR-D3 | Recommend start nogui, suspend hard and state-qualified stop hard for these cases. They need no assumed guest Tools contract. Alternative: soft operations now, which requires the unverified Phase 5 integration. Never substitute host reset for guest restart. | proposed |
| VMR-D4 | Recommend one std-only `tools/perf-probe` library plus a functional lane command and testable Rust orchestration; PowerShell is a thin adapter. This follows 0018-D10 and permits fake host tests. Alternative: PowerShell orchestration with a separately specified test harness; no duplicated RFB client. | proposed |
| VMR-D5 | Recommend live exact keyboard readiness **and** pixel readiness before input, giving early, attributable failures. Alternative: pixels before input and complete serial validation after off; this avoids a live-read dependency but needs proven preservation of every required serial epoch. These are explicit policy choices, never automatic fallback. | proposed |
| VMR-D6 | Recommend acquire-only use of the existing directory lock for new cases and lane packaging; recovery continues only the recorded run. This avoids general adoption. A missing operator needs Cameron's named replacement authorization, recorded without impersonation. Alternative: general handoff tokens and a broader team protocol. | proposed |
| VMR-D7 | Recommend a fixed-root journal, suspended-failure lock release and explicit one-process recovery, with opt-in PowerOff. A running/uncertain crash keeps the machine lock until recovery; that can block other agents. Alternative: retain the lock for suspended failures too and require a separately designed operator procedure. | proposed |
| VMR-D8 | Recommend exact glyph/scene checks including the pointer sprite, with immediate key-up. This detects wrong keys and avoids typematic flakes. Alternative: changed-pixel checks, which cannot distinguish successful status output from an unknown command. | proposed |
| VMR-D9 | Recommend clean-commit `git archive` packaging with recorded hashes and the operator's config-free cached Cargo home in offline mode. This avoids checkout edits and undocumented cache copying, but shares mutable cache state. Alternatives: documented offline vendoring into a fresh home (more setup), sealed Windows sources (larger scope), or a working-tree manifest (weaker provenance). No hostile-isolation claim. | proposed |
| VMR-D10 | Recommend Codex provision, own and exclusively operate lane cases, coordinated by Claude, at `C:\Users\engin\Desktop\Mirror-vm\lane`, with fixed staging. This makes same-operator recovery independent of worktrees. Alternative: per-agent VMs, ports and recovery state. Cameron must approve the operator/path/profile and D6's exceptional replacement rule. | proposed |
| VMR-D11 | Recommend a separately authorized Codex investigation after D10 provisioning and before guest-lane implementation: state/command completion, listener/serial, RFB/pointer, artifact preservation and recovery. This settles external facts first. Alternative: defer experiments until all lane choices are confirmed. Provisioning and power cycles are VM operations; this draft authorizes none. | proposed |

## Interfaces

### Entry, sources and implementation boundary

Proposed entry: `scripts/vmrun.ps1 -Vmx <path> -LaneRoot <path> -LockOwner <nick/task> -Case boot|suspend-resume|restart`. Recovery uses `-Recover <run-id> [-PowerOff]` instead of `-Case`, resolves VMX from the canonical LaneRoot record and rejects a conflicting supplied VMX. New cases never adopt an existing lock. Optional `-Vmrun <path>` overrides discovery through the 64-bit VMware Workstation `InstallPath` registry value. Use argument arrays and hidden processes; no credentials, implicit `all`, build or repackaging.

The Rust lane owns deadlines, transitions, oracles, journals and evidence behind injectable command, clock, serial, state/listener and artifact adapters. The host spec also puts the lane-package controller in Rust; `package.ps1 -Lane [-Release] -LaneRoot <path> -LockOwner <nick/task>` is its thin entry. Thus the cargo host tests exercise production controllers with fake command runners and temporary files, without Pester or a second orchestration implementation.

Future scope: `tools/perf-probe`, `scripts/vmrun.ps1`, `scripts/package.ps1`, host checks in `scripts/test.ps1`, and matching AGENTS/P9 and team recovery-rule documentation. Correct `docs/vm-settings.md`'s creation paragraph: only bridges 4–7 are eight-function root ports; bridge 0 is separate. Add a separate lane profile, preserving the reference block. No kernel/userspace changes, `common.ps1` retrofit or other script lock edits.

### Recommended packaging and fixed VM binding

Lane packaging acquires/releases the machine lock independently of the later VM case and never changes VM staging. Resolve one clean committed tree and export it with `git archive` under `<LaneRoot>/package/<package-id>/source`, outside every checkout, with fresh output directories. Only this controller writes the scratch tree; cooperating agents never edit it. Inventory hashes before/after detect persistent changes; they do not prevent transient tampering by another process. If stronger isolation is wanted, choose D9's sealed alternative and specify sealing, unsealing and crash cleanup in the host spec.

Cargo searches ancestors and Cargo home as well as the project ([configuration hierarchy](https://doc.rust-lang.org/cargo/reference/config.html#hierarchical-structure)). Reject `.cargo/config` and `.cargo/config.toml` in every ancestor above the export through the volume root. Resolve and record the operator's existing cached `CARGO_HOME`; reject its `config` and `config.toml`, preserve the exported project's configuration, and set `CARGO_NET_OFFLINE=true` with locked resolution ([environment variables](https://doc.rust-lang.org/cargo/reference/environment-variables.html)). This uses Cargo's own cache handling, not copied internal layouts; missing cached inputs fail. Clear/reject inherited source, compiler, flags and output overrides against a recorded allowlist. Verify the pinned toolchain, and copy the verified cached Limine archive into the export's private `.tools`; no shared extraction junction or download.

Build default flavor without agent-demo/perf tokens. Copy the private build's image/descriptor to the calling checkout's `build/dist`; publish `mirror.package.json` there last and atomically: schema, package-id, source commit/tree, clean status, profile/features/toolchain, committed-source inventory, capability/oracle versions and image/descriptor sizes/SHA-256. Inventory only committed source files, excluding generated `build`, `target`, `.tools` and lane artifacts. All packaging modes invalidate an old record before replacing files; only `-Lane` publishes one. Partial/mixed generations fail. The host spec records approved **merged main commits** for 0017b, optional 0017c and later 0058a; validate their ancestry in the package commit, not historical pre-rebase IDs. Guest cases require 0017b; restart also requires 0058a. The later checkout HEAD or staged ELF is not provenance.

The controller's `finally` removes only its canonically verified scratch directory, with no reparse traversal, retaining logs/manifests outside it. A crash leaves a package-id/owner record: the owner explicitly removes that abandoned scratch tree after establishing the packaging operation has ended; never delete by age or interrupt a live build. There are no ACL seals to undo in the recommended option.

Provisioning records one canonical VMX-to-LaneRoot binding. Preflight requires resolved `sata0:0.fileName` = `<LaneRoot>/disk/mirror.vmdk`, its adjacent extent `mirror.img`, and `serial0.fileName` = `<LaneRoot>/serial.log`. Reject LaneRoot inside any Git worktree, beneath Cameron's `C:\Users\engin\Desktop\Mirror`, or through reparse aliases. Compare canonical Windows path components, not string prefixes: `Mirror-vm` is not beneath `Mirror`. The root must match the binding; another root cannot bypass its journal. No snapshots/parent chain or extra disk. With proven off state and lock held, stage the **calling checkout's `build/dist` image/descriptor and `mirror.package.json`** through hash-verified temporaries and publish a generation last. After off, compare both hashes exactly; preserve and fail on mutation.

### VM profile and prerequisite investigation

Codex provisions after explicit D10 authorization, using the wizard or `vmcli VM Create ... -c other-64`. D11 additionally requires its own authorization and that provisioned VM. The proposed profile requires hardware 22, UEFI/Secure Boot off, 1 GiB/1 vCPU, SATA/e1000, HPET, no USB and reference 3D/RTC/unattended-start settings. Require `pciBridge0.present`, and bridges 4–7 present with `virtualDev=pcieRootPort`, `functions=8`. Record creation method. Add VNC enabled, loopback IP, a reserved port 5901–6001, no password/key and no keyMap override. Initial oracle supports recorded 1024x768; another mode needs a reviewed oracle.

Reject contradictory/duplicate configuration, occupied port, unexpected devices, encrypted/unreadable configuration or escaping paths. Verify the listener belongs to this VM and binds only loopback before connecting. Bound VMX to 1 MiB, manifests/journals to 256 KiB and owner records to 16 KiB; lane-generated records have versioned schemas. No automatic firewall change.

Before guest implementation, D11 must verify the following under the team lock; failed prerequisites block the cases until a reviewed choice resolves them:

- Reliable off/running/suspended/transition and completed-power-operation indicators; absence from `vmrun list`, elapsed time and repeated snapshots alone are insufficient. Verify crash/timeout recovery and command-job lifetime as specified below.
- Listener ownership, complete serial epochs and (recommended D5) live reads with bounded delivery; Raw fidelity at reference 3D settings; repeated incremental and non-incremental requests on one connection on static and changing screens. An unchanged screen may leave an incremental request pending ([RFB §7.5.3](https://www.rfc-editor.org/rfc/rfc6143.html#section-7.5.3)); test its response after a controlled change, without issuing a second pending request.
- If 0017c is packaged, the absolute pointer stays at its initial position across VNC connection, suspend and resume. Pointer movement requires a reviewed oracle rule, never a mask. Repeated-update stalls require a reviewed/confirmed reconnect policy and revised connect budget, not an automatic fallback.
- Byte-identical staged image/descriptor after a power cycle, plus both running and suspended PowerOff recovery paths. Rebuild identity proves neither. Resume responsiveness establishes no guest clock/Tools support.

### Exact guest evidence

Under recommended D5, each initial/restarted boot supplies its fresh readiness before input; validate the entire epoch again after off. The alternative policy would instead defer serial verdicts, including shell/console markers, until off, while retaining pixel fences and requiring all epochs to remain available. Record the confirmed policy in the lane profile; no runtime fallback flag.

The first oracle targets the PS/2-only kernel inspected at `14cb7b2`; a package containing merged 0017c selects an explicit vmmouse variant. Use schemas from `kernel/src/{main,acpi,apic,smp,timer,clock,pci,drivers,ps2,userspace}.rs` plus `vmmouse.rs` for that variant. Require these lines exactly once in order, with `elapsed_ms < 1000` (0017b's VMware threshold):

```text
MIRROR_PS2_INIT elapsed_ms=<u64> clock=hpet selftest=0
MIRROR_KEYBOARD_READY transport=i8042 set=2 irq=1
MIRROR_MOUSE_READY transport=i8042 packet=3 irq=12
```

The vmmouse variant additionally requires `MIRROR_VMMOUSE_READY mode=absolute` after MOUSE_READY, rejecting VMMOUSE_SKIPPED/FALLBACK; source `7ca4070:kernel/src/vmmouse.rs`, `init`. Its centered POINTER_READY still precedes consumption of absolute position events; D11 must verify the later pixel stability. Unrecognized package/oracle combinations block.

Reject every PS2_TIMEOUT, keyboard/mouse skip, malformed/duplicate or unknown member of a checked family, and unexpected fallback/fatal/trace/test output. Apply 0013's boot requirements: BOOT_OK/framebuffer readiness, configured enabled/online CPUs, xAPIC, LAPIC timer, no ACPI skip/table skip, PCI omitted=0 and AHCI/NIC/SVGA records. For the proposed one-vCPU profile, the observed IOAPIC line is `MIRROR_APIC_IOAPIC_ID index=0 firmware=1 hardware=0`, before APIC readiness. Record 42/34 PCI counts and flag differences; do not turn informational observations into universal constants. Require in order:

```text
MIRROR_AHCI_READY sectors=2097152 readonly=1 port=0
MIRROR_AHCI_PROBE_OK fixture=0 reads=1
MIRROR_AHCI_CLEANUP_OK dma_reclaimed=1
```

Rust lane assertions use these VMware values; `common.ps1::Assert-MirrorAhciMarkers` assumes QEMU's 2048-sector disk. NIC catalog presence is not networking functionality.

### Pixel and input contract

Match packaged-source glyphs, geometry and UI tokens. Shell readiness matches `Shell::paint`'s MIRROR/Native desktop/Esc and welcome glyph regions plus centered surface offset. BOOT_OK/static pixels alone are insufficient: `input::Session::new` drains before shell execution. Send Escape once, then match the console header, initial two history rows and empty input; `run_console` drains before rendering. Require these serial lines in this order, not necessarily adjacent; other records must match their schemas:

```text
MIRROR_SHELL_CLEANUP_OK apps_reclaimed=1 outcome=Exit(0)
MIRROR_SMP_IPI_OK phase=after_desktop cpus=1 rounds=4
MIRROR_POINTER_READY x=512 y=384 width=1024 height=768
MIRROR_CONSOLE_READY input=ps2 display=1
```

`framebuffer.rs::{ConsoleGeometry,glyph,draw_console_history,draw_console_input}` and `console.rs::{Console::process,Text::counter,parse}` define the oracle. At 1024x768: history origin (48,136), row spacing 24, prompt text x=72, edit origin (72,664), scale 2/advance 12. Compose the pointer using `mirror_ui_tokens::CURSOR` and `Driver::draw_cursor`: 15×19 at (512,384), X writes RGB(3,6,10), O writes text color RGB(236,241,249), and dots preserve the scene. Require that stationary overlay, including over blank rows; mask nothing. The lane sends no pointer events. Concurrent interactive input is prohibited.

Before Enter require exact `status`, blank suffix and caret. After one Enter require empty input, unchanged prior history and exactly three appended rows: prompt `status`, `Timer ticks: <u64>`, `Keyboard IRQs: <u64>`. Validate decimal glyphs, 1–20 digits/u64 range/no leading zero except zero; do not predict counters. Echo uses `echo ` plus a 16-character lowercase hex run sentinel: match its edit text, prompt and exact output, retained after resume. Reject unknown commands, wrong/doubled/missing letters, extra rows, input-loss notices, stale/partial frames and sentinel leakage to serial. No OCR or guessed tolerance.

Send key-up immediately after each successful key-down write, without an intervening wait/frame/query. From starting down to completing up allow at most **200 ms**, with each write at most 100 ms; a longer gap is an ambiguous failure and gets no resend. Always attempt immediate bounded release on error. This stays below 0018's documented 500 ms VMware typematic delay without relying on benchmark input timing.

### Cases and bounds

| Case | Sequence after validated off state/staging |
| --- | --- |
| boot | Start once; boot/shell fences; Escape; console fences; exact status; evidence; stop hard once; prove off and check artifacts. |
| suspend-resume | Boot through status; exact echo; preserve history/serial; close VNC; suspend hard once; prove suspended; dwell 2 s; start nogui once; reconnect; same history/sentinel/boot epoch and no fatal marker; new exact status; stop/off checks. |
| restart | After 0058a's contract is confirmed and incorporated: checked boot/status; exact restart edit; Enter once; distinct boot epoch and readiness; console/status again; stop/off checks. No reset/repeated Enter/start rescue. |

The suspend history grows 2→5→7→10 rows, below capacity 12. Continuity preserves the original boot gate after resume; a reboot fails this case. No clock, timer-rearm or Tools support is inferred.

All operation deadlines are monotonic totals, capped by remaining case time; progress never resets them.

| Operation | Proposed cap |
| --- | --- |
| Active case including staging | 900 s; busy lock fails immediately. |
| Package validation/staging; state/listener phase | 120 s; 15 s, at most 32 queries. |
| Lifecycle command; guest readiness | 60 s; 120 s. RFB connect attempt ≤3 s. |
| Typed action including frames | 30 s, ≤32 pairs/action, ≤64/case; immediate ≤200 ms down/up contract above. |
| Failure release/evidence, stop, off verification | 30 s + 60 s + 15 s = 105 s maximum after active case. |
| Recover; Recover -PowerOff | 120 s without power actions; 300 s with explicit PowerOff (≤60 s prior-intent reconciliation, three 15 s state phases, two 60 s lifecycle actions, 30 s evidence = 255 s). |

Suspend-resume uses 37 key pairs, the longest case; allow one emergency key-up. Keep one RFB request outstanding across timeout, invalidate coverage/geometry on resize and require complete frames. Readiness polling may request non-incremental full frames when no request is pending. Cap connection attempts at eight/case, ≤3 s each within the case deadline. This is not a reconnect-per-checkpoint policy. Review the WIP primitives' caps of 4096 pixels/dimension, 4096 rectangles/update, 128 MiB/message and 1 MiB/string; malformed input fails.

### Acquire-only lock and same-run recovery

New lane/package invocations atomically create the team directory, retain the four readable owner fields plus `lane_schema=1`/`lane_run=<id>`, and hold an exclusive write/delete-excluding handle on `owner.txt` throughout execution/finalization. Existing directories are busy for new cases, including the same nick/task. Ordinary scripts keep the manual protocol; no age/PID-based takeover or rewriting another agent's free-text record.

Before staging, durably journal `preparing` under LaneRoot with operator/run, original worktree (provenance only), bound VMX/root, paths and last proven state. Record each power intent and dispatch time before dispatch, including restart Enter; partial staging is invalid. On error, release keys and preserve the first failure. A positively running VM may receive one cleanup stop only after previous power intents are reconciled.

One long-lived Rust controller owns the lease and every vmrun child. Assign each child before execution to a Windows [kill-on-close job](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects) whose sole controlling handle belongs to the controller; disallow breakaway. D11 must verify that all command workers are covered while VMware's service/vmware-vmx remain outside the job. Timeout/cancellation kills command workers, never vmware-vmx. If this separation cannot be demonstrated, guest cases are blocked.

For an unresolved prior intent, reconciliation first establishes that no old command worker remains. It then waits out the 60 s intent budget (a full 60 s if elapsed time cannot be trusted across restart) and reads D11's verified completion/state indicators twice within the state-phase cap. **Time and matching snapshots are not proof by themselves:** D11 must establish that an accepted request cannot complete later. Until that rule succeeds, treat the VM as transitioning even when off/suspended is observed; retain the lock and ask for a reviewed operator action. Apply this rule to failures and every recovery path.

Codex runs recovery from its own current checkout using the canonical LaneRoot record; it never needs the original worktree. `-Recover <run-id>` verifies the recorded operator/run and binding, obtains the exclusive handle on a retained lane lock or atomically reacquires a missing lock. Concurrent/active holders fail. Missing or mismatched identity blocks, rather than borrowing another lock. Plain Recover only reconciles and finalizes a proven-off run, or journals proven suspension and releases; running/unknown states remain locked with instructions to invoke PowerOff. Partial/absent staged files are evidence, never repaired silently.

Explicit **`-Recover <run-id> -PowerOff`** holds that same lease in one process through reconciliation, power actions and finalization. Once state is known: off needs no action; running gets `vmrun -T ws stop <bound-vmx> hard` once; suspended gets `start <bound-vmx> nogui` once, then after proven running/completion, `stop <bound-vmx> hard` once. Journal each action, prove off, preserve the failed run's verdict, then finalize/release. No repeat of an ambiguous action, reset, `.lck` deletion or separate shell handoff. D11 must verify both paths and interrupted-command reconciliation before enabling them; otherwise recovery remains blocked, with evidence for Cameron.

If the recorded operator is unavailable, follow the team's stale-lock rule and ask in chat. Only Cameron's explicit authorization naming the run/root, original operator, replacement operator and recovery scope permits replacement. After verifying that run and obtaining its exclusive lease, append the authorization text/reference and both identities to LaneRoot before acting; never impersonate the old operator or write its worktree. This proposed D6 exception requires confirmation and team-rule documentation, not general adoption. No authorization means no takeover.

Release the lane-created directory only at these points: packaging ended with no live build; durable `finalized-off` (pass or fail); a reconciled suspended failure; or those same off/suspended outcomes in Recover. Suspended release first preserves `suspended-needs-recovery`; its journal prevents restaging/new cases. Running, unknown or unresolved states retain the lock; planned successful suspend/resume retains it across the dwell. Release must preserve run identity and cannot remove a successor's lock.

### Evidence and finalization

Full evidence may live in the operator's current `build/vm-logs/lane/<run-id>`: source/package/staging hashes, settings, tool/oracle/policy versions, steps/deadlines and final cleanup state. Preserve serial epochs before replacement; truncation/missing lines/unprovable boundaries fail. Revalidate after off. Before release, atomically persist `<LaneRoot>/runs/<run-id>/final.json` containing verdict/first failure, capability/package/staging hashes, VM binding, reconciled intents, off proof and evidence locations/hashes; the root journal points to it as `finalized-off`. All recovery-critical state lives in LaneRoot. Later cases read only this durable record plus fresh off proof, never a prior worktree. Deleted bulk evidence is reported as unavailable without discarding the retained verdict/proof. Missing final records remain incomplete and require Recover.

Checkpoint counts: boot four (shell, console, status edit/result); suspend nine (adds echo edit/result, retained history, new status pair); restart nine (adds restart edit, new shell/console, new status pair). Cap 12 normal plus four failure frames; do not save every poll. Cap command streams at 1 MiB each/16 MiB total, serial and VMware logs at 64 MiB each across generations. Overflow preserves the bounded prefix/reason and fails. Cleanup success never replaces the first failure. Label synthetic fixtures separately from VMware captures.

## Acceptance tests

The future host tests exercise the production Rust lane/package controllers with fake adapters and temporary roots; the wrapper invokes those controllers, so no unstated PowerShell framework is needed. Add these commands to test.ps1 and AGENTS when implemented:

```powershell
cargo fmt --all --manifest-path tools/perf-probe/Cargo.toml -- --check
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml
```

1. Fake adapters cover acquire-only/concurrent Recover, authorized replacement versus impersonation, recovery from a different own checkout, deleted old bulk evidence, every release point and staging/finalization crashes. Exercise plain Recover and PowerOff for running/suspended/unresolved intents, including off snapshots before a delayed start; no proof means no action/release. A synthetic Windows child tests the production job adapter: controller exit kills its command workers. Also test binding/path components, capability/marker variants and IPI order, repeated RFB updates, >200 ms key-up, resize and caps. Exact pointer/glyph/sentinel fixtures reject movement, stale frames and malformed input; review WIP RFB code.
2. Fake package tests check archive isolation, configuration/override rejection, offline environment, dirty source, partial copies, stale/hash mismatches and scratch cleanup/crash leftovers. Separately run real `cargo metadata --locked --offline --format-version 1` for the root and mirror-image manifests: the config-free warm Cargo home succeeds; a disposable empty home fails without downloads. Then run actual debug/release lane packaging sequentially and verify source/tree/profile/capabilities/hashes. No undocumented cache-copying or hostile-seal fixture.
3. After separately authorized D10/D11, confirmed decisions and merged 0017b, run boot and suspend-resume once each for debug/release. Preserve exact frames, complete epochs, actual settings, off proof and unchanged artifacts. Verify real job membership, interrupted-intent reconciliation, running and suspended PowerOff, and suspended release/reacquisition before enabling recovery. Check both RFB request types repeatedly on static/changing screens and vmmouse pointer stability when packaged. Failed prerequisites remain blocked; reconnect changes need confirmation. Do not manufacture captured resize evidence.
4. A requested restart without approved 0058a package capability blocks before start. Once its actual contract lands, run one guest restart per profile with a new epoch and no host reset fallback. Run the repository's full test.ps1 before merge per P8, plus AgentDemo where required by touched code; VMware cases remain explicit. Report every failed/blocked case, with no other-OS or benchmark result.

## Dependencies

No new third-party crates/test framework. Existing pinned Rust 1.99.0/std, MSVC/Windows SDK, Git and PowerShell/.NET; Workstation Pro 26.0.1 build 25688693 and vmrun 1.17.0.25688693 (proprietary Broadcom, not redistributed). Tool-version changes require recorded revalidation, not automatic downloads.

## Open questions

- Cameron chooses separately: D2 reference versus G11; D9 archive/shared offline cache versus vendored/sealed/working-tree alternatives; D6 acquire-only plus named replacement exception versus general adoption; D5 live versus post-off serial; D10 Codex-only operation/default root; D11 prerequisite experiments after provisioning. All other rows also remain proposed.
- D11 must settle pending-operation completion/job membership, state/listener identity, serial epochs, repeated RFB requests, vmmouse stability, artifact preservation and both PowerOff paths. A failed fact returns for a choice, not a silent fallback. No experiments are authorized by this draft alone.
- 0058a must provide the guest restart and package-capability contract. Cameron numbers the host and guest specs after choosing the shared interfaces; ledger/map changes remain Claude's responsibility.
