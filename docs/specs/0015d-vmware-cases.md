# 0015d: VMware headless functional cases

Status: draft (2026-10-12); split from reviewed `draft-vmrun-lane.md` at `8cd8eb0`. Implementation and VM operations are separate work.

Depends on: [0015c](0015c-host-lane.md), 0010, 0011, 0013, 0015a and merged 0017b/0017c. D10 provisioning and D11's separately authorized platform investigation must pass before VMware implementation. Guest restart additionally requires confirmed, merged 0058a. Uses 0018-D10's library without resuming benchmarks.

Ledger: G3a, S1a, S1c, S1d, S3, S7, S7a, S7b, S10, S15, P3, P5a, P8, P9; related G11 and S16. Claude owns the corresponding ledger, roadmap and spec-map update.

## Goal

An opt-in check boots a packaged Mirror image headless, verifies exact console commands through VMware VNC, and checks continuity after host suspend/resume. A guest restart case joins after 0058a. Results are functional outcomes with bounded operations and preserved evidence, not performance measurements.

0015c can land with fake adapters before this spec's investigation. 0015d owns the real VMware command/state/listener/serial adapters, VMX profile validation, glyph/scene oracles and case activation. D11 first proves the platform contract manually under the existing team rules; actual guest cases and VM recovery are then enabled together. Package recovery belongs to 0015c and is independent of D11.

## Non-goals

Other-OS downloads, setup and benchmarks remain deferred until Cameron requests them. No completion of 0018, new guest markers, guest power/clock/Tools implementation, pointer gestures or click tests, snapshots, host security changes, CI or replacement of QEMU tests. The only pointer input is 0015d's bounded setup priming move. The runner does not create/reconfigure VMs; D10 provisioning is separate owner work in 0015d, coordinated by Claude under the team lock. Phase 5 can add soft-power cases later. Cross-script lock enforcement and hostile-process source isolation are outside the confirmed scope.

The shared controllers, packaging, journals, lock protocol and recovery algorithms are defined only in 0015c. This spec consumes them without duplicating or relaxing them.

## Verified starting point

Source and oracle behavior below was checked at merged main `838c3b6`. Unmerged diagnostic and decision-documentation evidence is identified separately:

- [vm-settings](../vm-settings.md) records Workstation Pro 26.0.1 build 25688693, hardware 22, UEFI, ULM and 1024x768. [0013](0013-firmware-and-pci-tolerance.md) records successful 1/4-vCPU boots, not resume; its 42 functions/34 bridges are observations, with the bridge count explicitly informational.
- Merged 0017b ends at `14cb7b2`, with completion notes at `6d50e7d`; `ps2::prepare_keyboard` accepts `0x02 | 0x41` (`838c3b6:kernel/src/ps2.rs:483-516`). Merged 0017c ends at `3c87bcb`, with notes at `a803e9a`. Guest packages require both code tips as ancestors.
- Installed help read on 2026-10-12 reports `vmrun` 1.17.0.25688693 and start/stop/suspend syntax. Help proves neither reliable power-state detection nor live serial, resume, recovery or disk-preservation behavior.
- Claude's 2026-10-12 diagnostic, recorded in `0017c-absolute-pointing.md` at the unmerged `claude/0017c-restrict-backdoor` commit `358d170`, found that the first VNC move after power-on delivered no AUX byte and queued no packet; the second delivered only its own packet. That branch also records Cameron's confirmed 0017-D19. Its eventual merged documentation revision must be recorded before lane implementation; `358d170` is evidence, not a merged package capability. This draft claims no new runtime result.

Packaging, existing team-lock and unreviewed `tools/perf-probe` WIP evidence are recorded in [0015c](0015c-host-lane.md#verified-starting-point).

## Decisions

Cameron confirmed the reviewed `8cd8eb0` recommendations on 2026-10-12, recorded in [decisions.md](decisions.md)'s decision-page entry. Each VMR ID has one canonical owner across these two specs. Alternatives below are retained as unchosen rationale, not selectable runtime policies; changing one requires a re-confirmed decision.

| ID | Recommendation, rationale and alternative | Status |
| --- | --- | --- |
| VMR-D2 | Adopted recommendation: the dedicated unencrypted reference profile: 1 GiB/1 vCPU, default flavor and firmware-selected display mode; the lane VM follows 0017-D19 and sets `monitor_control.restrict_backdoor = "TRUE"`. It has existing boot evidence. Alternative: resolve RD16 first and use G11; that expands resource/profile and oracle validation. | confirmed (2026-10-12) |
| VMR-D3 | Adopted recommendation: start nogui, suspend hard and state-qualified stop hard for these cases. They need no assumed guest Tools contract. Alternative: soft operations now, which requires the unverified Phase 5 integration. Never substitute host reset for guest restart. | confirmed (2026-10-12) |
| VMR-D5 | Adopted recommendation: live exact keyboard readiness **and** pixel readiness before input, giving early, attributable failures. Alternative: pixels before input and complete serial validation after off; this avoids a live-read dependency but needs proven preservation of every required serial epoch. These are explicit policy choices, never automatic fallback. | confirmed (2026-10-12) |
| VMR-D8 | Adopted recommendation: exact glyph/scene checks including the pointer sprite, with immediate key-up. This detects wrong keys and avoids typematic flakes. Alternative: changed-pixel checks, which cannot distinguish successful status output from an unknown command. | confirmed (2026-10-12) |
| VMR-D10 | Adopted recommendation: Codex provision, own and exclusively operate lane cases, coordinated by Claude, at `C:\Users\engin\Desktop\Mirror-vm\lane`, with fixed staging. This makes same-operator recovery independent of worktrees. Alternative: per-agent VMs, ports and recovery state. This operator/path/profile is confirmed on 2026-10-12; coordinate its provisioning with Claude. | confirmed (2026-10-12) |
| VMR-D11 | Adopted recommendation: a separately authorized Codex investigation after D10 provisioning and before guest-lane implementation: state/command completion, listener/serial, RFB/pointer, artifact preservation and recovery. This settles external facts first. Alternative: defer experiments until all lane choices are confirmed. Provisioning and power cycles remain separate coordinated VM work under the shared lock; this documentation change performs none. | confirmed (2026-10-12) |
| Priming | One unmeasured buttons-up center move per fresh boot epoch before Escape, with the confirmed live readiness fence and limits below. Alternative: no pointer event. Never prime resume or a same-epoch reconnect. | confirmed (2026-10-12) |

Canonical rows VMR-D1, D4, D6, D6 exception, D7 and D9 live in [0015c](0015c-host-lane.md#decisions). Its operation/count/size caps apply unchanged. This spec owns the priming contract below; the two spec numbers implement D1.

## Interfaces

Use [0015c's entry points and host contracts](0015c-host-lane.md#interfaces): no build/repackage during a VM case; fixed canonical VMX/LaneRoot/disk/serial binding; fresh package manifest plus valid package-final record; off-only staging; shared acquire/release/detach protocol; one-process Recover/PowerOff; named replacement authorization; bounded evidence and durable finalization. Enforce every host cap and preserve the first failure. No separate recovery controller or alternate lock path is introduced.

Implementation scope: real VMware adapters and oracles in `tools/perf-probe`, activation of the thin `scripts/vmrun.ps1` entry, VMware fixtures/cases, and matching AGENTS/P9 and `docs/vm-settings.md` documentation. Correct the creation paragraph: only bridges 4–7 are eight-function root ports; bridge 0 is separate. Add a separate lane profile, preserving the reference block. No kernel/userspace changes, `common.ps1` retrofit or other script lock edits.

### VM profile and prerequisite investigation

D10 confirms Codex as the provisioning owner; coordinate this separate work with Claude, using the wizard or `vmcli VM Create ... -c other-64`. D11's investigation scope is confirmed separately from implementation and requires that provisioned VM. The confirmed profile requires hardware 22, UEFI/Secure Boot off, 1 GiB/1 vCPU, SATA/e1000, HPET, no USB and reference 3D/RTC/unattended-start settings. Require exactly `monitor_control.restrict_backdoor = "TRUE"` under 0017-D19; preflight rejects an absent, false or conflicting value. D11's vmmouse checks use that setting. Adoption is confirmed; acceptance under ULM and working kernel vmmouse are not a CPL3-enforcement probe. Require `pciBridge0.present`, and bridges 4–7 present with `virtualDev=pcieRootPort`, `functions=8`. Record creation method. Add VNC enabled, loopback IP, a reserved port 5901–6001, no password/key and no keyMap override. Initial oracle supports recorded 1024x768; another mode needs a reviewed oracle.

Reject contradictory/duplicate configuration, occupied port, unexpected devices, encrypted/unreadable configuration or escaping paths. Verify the listener belongs to this VM and binds only loopback before connecting. Bound VMX to 1 MiB, manifests/journals to 256 KiB and owner records to 16 KiB; lane-generated records have versioned schemas. No automatic firewall change.

Before guest implementation, D11 must verify the following under the team lock; failed prerequisites block the cases until a reviewed choice resolves them:

- Reliable off/running/suspended/transition and completed-power-operation indicators; absence from `vmrun list`, elapsed time and repeated snapshots alone are insufficient. Verify crash/timeout recovery and command-job lifetime as specified below.
- Listener ownership, complete serial epochs and (confirmed D5) live reads with bounded delivery; Raw fidelity at reference 3D settings; repeated incremental and non-incremental requests on one connection on static and changing screens. An unchanged screen may leave an incremental request pending ([RFB §7.5.3](https://www.rfc-editor.org/rfc/rfc6143.html#section-7.5.3)); test its response after a controlled change, without issuing a second pending request.
- Verify the setup priming contract below and that the absolute pointer stays at its expected position across VNC connection, suspend and resume. Unexpected pointer movement requires a reviewed oracle rule, never a mask. Repeated-update stalls require a reviewed/confirmed reconnect policy and revised connect budget, not an automatic fallback.
- Byte-identical staged image/descriptor after a power cycle, plus both running and suspended PowerOff recovery paths. Rebuild identity proves neither. Resume responsiveness establishes no guest clock/Tools support.

### Exact guest evidence

Under confirmed D5, each initial/restarted boot supplies its fresh readiness before input; validate the entire epoch again after off. The unchosen alternative policy would instead defer serial verdicts, including shell/console markers, until off, while retaining pixel fences and requiring all epochs to remain available. Record the confirmed policy in the lane profile; no runtime fallback flag.

The supported first oracle requires merged 0017c and absolute vmmouse; PS/2-only packages are outside this lane. Use schemas at `838c3b6` from `kernel/src/{main,acpi,apic,smp,timer,clock,pci,drivers,ps2,userspace,vmmouse}.rs`. Require these lines exactly once in order, with `elapsed_ms < 1000` (0017b's VMware threshold):

```text
MIRROR_PS2_INIT elapsed_ms=<u64> clock=hpet selftest=0
MIRROR_KEYBOARD_READY transport=i8042 set=2 irq=1
MIRROR_MOUSE_READY transport=i8042 packet=3 irq=12
MIRROR_VMMOUSE_READY mode=absolute
```

Reject VMMOUSE_SKIPPED/FALLBACK across the entire epoch, including after resume. Init SKIPPED/READY are at `838c3b6:kernel/src/vmmouse.rs:289-297`, called after MOUSE_READY at `main.rs:334,344`; runtime FALLBACK is at `vmmouse.rs:272`, called from `ps2.rs:358`. The unexpected-fallback rule below applies to the whole epoch. The centered POINTER_READY precedes consumption of absolute position events (`main.rs:579-607`, `pointer.rs:26-31`); D11 must verify later pixel stability. Unrecognized package/oracle combinations block.

Reject every PS2_TIMEOUT, keyboard/mouse skip, malformed/duplicate or unknown member of a checked family, and unexpected fallback/fatal/trace/test output. Apply 0013's boot requirements: BOOT_OK/framebuffer readiness, configured enabled/online CPUs, xAPIC, LAPIC timer, no ACPI skip/table skip, PCI omitted=0 and AHCI/NIC/SVGA records. For the confirmed one-vCPU profile, the observed IOAPIC line is `MIRROR_APIC_IOAPIC_ID index=0 firmware=1 hardware=0`, before APIC readiness. Record 42/34 PCI counts and flag differences; do not turn informational observations into universal constants. Require in order:

```text
MIRROR_AHCI_READY sectors=2097152 readonly=1 port=0
MIRROR_AHCI_PROBE_OK fixture=0 reads=1
MIRROR_AHCI_CLEANUP_OK dma_reclaimed=1
```

Rust lane assertions use these VMware values; `common.ps1::Assert-MirrorAhciMarkers` assumes QEMU's 2048-sector disk. NIC catalog presence is not networking functionality.

### Pixel and input contract

Match packaged-source glyphs, geometry and UI tokens. Shell readiness matches `Shell::paint`'s MIRROR/Native desktop/Esc and welcome glyph regions plus centered surface offset. BOOT_OK/static pixels alone are insufficient: `input::Session::new` drains before shell execution. Under confirmed D5, wait for live absolute-vmmouse readiness and rendered shell readiness. The unchosen post-off alternative would wait for rendered shell readiness using the ancestry-validated vmmouse package; verify READY and no SKIPPED/FALLBACK from the preserved epoch after off before any pass verdict; it is not enabled. Once the confirmed policy's pre-input readiness fence is met, send exactly one RFB PointerEvent with buttons=0 at framebuffer (512,384), before Escape. Its write has a 100 ms cap within the existing readiness/case deadline; failure aborts without resending. Record it as setup priming, with no expected response or extra frame request: the host may drop it or deliver the same center position. Repeat once on a new guest-restart epoch, never on suspend/resume or a same-epoch reconnect. Send Escape once, then match the console header, initial two history rows and empty input; `run_console` drains before rendering. Require these serial lines in this order, not necessarily adjacent; other records must match their schemas:

```text
MIRROR_SHELL_CLEANUP_OK apps_reclaimed=1 outcome=Exit(0)
MIRROR_SMP_IPI_OK phase=after_desktop cpus=1 rounds=4
MIRROR_POINTER_READY x=512 y=384 width=1024 height=768
MIRROR_CONSOLE_READY input=ps2 display=1
```

`framebuffer.rs::{ConsoleGeometry,glyph,draw_console_history,draw_console_input}` and `console.rs::{Console::process,Text::counter,parse}` define the oracle. At 1024x768: history origin (48,136), row spacing 24, prompt text x=72, edit origin (72,664), scale 2/advance 12. Compose the pointer using `mirror_ui_tokens::CURSOR` and `Driver::draw_cursor`: 15×19 at (512,384), X writes RGB(3,6,10), O writes text color RGB(236,241,249), and dots preserve the scene. Require that stationary overlay, including over blank rows; mask nothing. After setup priming the lane sends no pointer events. Concurrent interactive input is prohibited.

Before Enter require exact `status`, blank suffix and caret. After one Enter require empty input, unchanged prior history and exactly three appended rows: prompt `status`, `Timer ticks: <u64>`, `Keyboard IRQs: <u64>`. Validate decimal glyphs, 1–20 digits/u64 range/no leading zero except zero; do not predict counters. Echo uses `echo ` plus a 16-character lowercase hex run sentinel: match its edit text, prompt and exact output, retained after resume. Reject unknown commands, wrong/doubled/missing letters, extra rows, input-loss notices, stale/partial frames and sentinel leakage to serial. No OCR or guessed tolerance.

Send key-up immediately after each successful key-down write, without an intervening wait/frame/query. From starting down to completing up allow at most **200 ms**, with each write at most 100 ms; a longer gap is an ambiguous failure and gets no resend. Always attempt immediate bounded release on error. This stays below 0018's documented 500 ms VMware typematic delay without relying on benchmark input timing.

### Cases

| Case | Sequence after validated off state/staging |
| --- | --- |
| boot | Start once; boot/shell fences; Escape; console fences; exact status; evidence; stop hard once; prove off and check artifacts. |
| suspend-resume | Boot through status; exact echo; preserve history/serial; close VNC; suspend hard once; prove suspended; dwell 2 s; start nogui once; reconnect; same history/sentinel/boot epoch and no fatal marker; new exact status; stop/off checks. |
| restart | After 0058a's contract is confirmed and incorporated: checked boot/status; exact restart edit; Enter once; distinct boot epoch and readiness; console/status again; stop/off checks. No reset/repeated Enter/start rescue. |

The suspend history grows 2→5→7→10 rows, below capacity 12. Continuity preserves the original boot gate after resume; a reboot fails this case. No clock, timer-rearm or Tools support is inferred.

Apply all [0015c operation/transport bounds](0015c-host-lane.md#operation-and-transport-bounds), including the 900 s active case, separate 105 s failure cleanup, 120/300 s recovery caps, eight connection attempts and one pending RFB request. Its evidence/finalization section owns checkpoint counts, storage caps and the durable verdict. No progress, reconnect or repeated polling resets a deadline.

### Recovery prerequisite and activation

D11 proves the [0015c recovery contract](0015c-host-lane.md#acquire-only-lock-and-same-run-recovery) against real VMware before enabling either VM cases or VM recovery. In particular, prove worker/service separation, terminal completion beyond matching snapshots, every interrupted-intent injection and its bounded observation, suspended serial preservation before start, both PowerOff paths, and suspended release/fresh acquisition. A quiet observation window alone is insufficient. Running/unknown/unresolved state retains the shared lock; planned suspend/resume holds it across the dwell. The owner retains a manual stop/release route under the current team protocol during D11. If any prerequisite fails, actual VM cases and controller recovery stay disabled together and the evidence goes to Cameron.

## Acceptance tests

1. Extend the production Rust controller host tests from 0015c with fake VMware adapters and exact oracle fixtures: required D19 configuration; capability/marker schemas and IPI order; repeated RFB updates, >200 ms key-up, resize and caps. Priming fixtures cover dropped/delivered center moves, D5's confirmed live fence and rejection of an unselected fallback policy, one buttons-up packet per boot epoch, none on resume/reconnect, and write failure without retry. Reject PS/2-only packages. Exact pointer/glyph/sentinel fixtures reject movement, stale frames and malformed input. Reject VMMOUSE_FALLBACK anywhere in a complete epoch, including after resume. Use 0015c's fmt/clippy/host-test commands and retain its lifecycle/lock/package fixtures.

2. After D10 provisioning under its confirmed scope, first perform D11 manually under the current team rules: verify real job membership, completion/cancellation evidence and interrupted-intent injections, running and suspended PowerOff procedures, serial preservation, and suspended release/fresh reacquisition. The owner retains a manual stop/release route throughout the experiment; no unproved controller recovery is used. Check both RFB request types repeatedly on static/changing screens and vmmouse stability with D19 set. Only after these prerequisites pass may guest cases and their recovery be enabled together. Then run boot and suspend-resume once each for debug/release with ancestry-validated 0017b/0017c packages. Preserve exact frames, complete epochs, actual settings, off proof and unchanged artifacts. Failed prerequisites keep guest cases blocked; reconnect changes need confirmation. Do not manufacture captured resize evidence.

3. A requested restart without approved 0058a package capability blocks before start. Once its actual contract lands, run one guest restart per profile with a new epoch and no host reset fallback. Run the repository's full test.ps1 before merge per P8, plus AgentDemo where required by touched code; VMware cases remain explicit. Report every failed/blocked case, with no other-OS or benchmark result.

## Dependencies

No new third-party crates/test framework. Existing pinned Rust 1.99.0/std, MSVC/Windows SDK, Git and PowerShell/.NET; Workstation Pro 26.0.1 build 25688693 and vmrun 1.17.0.25688693 (proprietary Broadcom, not redistributed). Tool-version changes require recorded revalidation, not automatic downloads.

## Open prerequisites

- Claude owns the ledger/map confirmation record. Record the eventual merged D19 documentation revision before lane implementation; do not treat unmerged diagnostic evidence as a package capability.

- D11 must settle pending-operation completion/job membership, state/listener identity, serial epochs, repeated RFB requests, vmmouse stability, artifact preservation and both PowerOff paths. A failed fact returns for a choice, not a silent fallback. Coordinate the confirmed D10/D11 work separately with Claude under the team run lock; this spec records no completed experiment.

- 0058a must provide the confirmed, merged guest restart and package-capability contract. Boot and suspend/resume do not wait for restart, but an unsupported restart request blocks before start.
