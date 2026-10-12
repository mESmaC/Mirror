# Draft: VMware headless check lane

Status: draft (2026-10-12). Every lane decision below is proposed; none authorizes implementation or a VM operation. Cameron assigns a number and confirms the decisions before implementation. This draft does not change the ledger or the spec map.
Depends on: 0010 (flavors and serial hygiene), 0011 (host-tool checks), 0013 (VMware firmware and PCI), 0015a (packaged disk). Keyboard checks exercise 0017a/b as they land; absolute-pointer checks wait for 0017c and its VMware checks. Guest restart waits for the proposed 0058a to be confirmed and implemented. Uses the preserved RFB work described below, without depending on completion of 0018's benchmarks.
Ledger: G3a, S1a, S1c, S1d, S3, S7, S7a, S7b, S10, S15, P5a, P8, P9. Related: G11, S16; the vmrun lane and RD16/RD24 in [roadmap-replan.md](roadmap-replan.md) remain proposals.

## Goal

A manually invoked host check boots the packaged Mirror image headless in a dedicated VMware VM, verifies fresh boot evidence and keyboard input through VNC, and checks that the same console session responds after host suspend/resume. It also gains a guest-issued `restart` case when 0058a lands. Each case has bounded waits, an exclusive run guard, and preserved serial, VMware, command and framebuffer evidence. It reports functional outcomes, not performance gate numbers.

## Non-goals

- Downloading, installing, configuring, testing or benchmarking another OS. Cameron deferred other-OS downloads and benchmarks on 2026-10-11 until explicitly requested; this lane does not resume that work.
- Completing 0018, defining performance thresholds, measuring latency/cadence, publishing baseline rows, or requiring 0019 perf markers.
- Implementing guest reboot, ACPI shutdown, TCLO guest services, clock resynchronization, timer rearm or network renewal. These remain with 0058a and the Phase 5 owners, including 0087c.
- Treating `vmrun stop/reset hard` as an orderly guest shutdown/restart, or treating successful host suspension as proof of guest resume support.
- Creating VMs, installing Tools, changing host hypervisor/security settings, editing an existing VM's hardware, taking/reverting snapshots, or replacing QEMU's automated suite. No CI or background VM monitor.

## Existing evidence and prerequisites

These are observations already recorded in the repository, except for the explicitly dated local help check. They are not successful executions of this proposed lane.

- [docs/vm-settings.md](../vm-settings.md), confirmed by 0015a check 9 on 2026-10-11, records Workstation Pro 26.0.1 build 25688693, virtual hardware 22, UEFI, ULM monitor mode and a 1024x768 framebuffer. Its reference profile uses 1 GiB and 1 vCPU, SATA `build/dist/mirror.vmdk`, e1000, explicit HPET, no USB controller, and a file-backed serial port. `package.ps1` stages the default flavor; VMs must not boot the image that tests rewrite under `build/`.
- [0013](0013-firmware-and-pci-tolerance.md) records ten successful VMware boots each at 1 and 4 vCPUs, after removing the firmware/PCI failures found by [0015a](0015a-disk-image.md). That establishes boot evidence for those recorded settings, not suspend/resume or guest restart.
- On 2026-10-12, a read-only invocation of `C:\Program Files\VMware\VMware Workstation\vmrun.exe -?` reported `vmrun version 1.17.0.25688693`. Its installed help lists `start <vmx> [gui|nogui]`, `stop/reset/suspend <vmx> [hard|soft]`, `list`, and `-T ws`. Help output establishes syntax only; no power command was run while drafting.
- [0018-D10 and its VM setup](0018-performance-baseline-harness.md) record VMware's RFB 3.8, security None, Raw and DesktopSize support and the loopback VNC keys. The reference VM document does not yet include those keys. Its 1920x1080 benchmark profile, vTPM and encryption are not prerequisites for this lane.
- Preserved Mirror source: `astra/0018-baseline-harness`, commit `bbc88e4`, `tools/perf-probe/src/{rfb,frame}.rs` and its README. The std-only RFB client bounds input, tracks full-frame coverage, retains partial input on timeout, and invalidates coverage on resize. Its fixtures are synthetic; its README explicitly records no VMware capture validation. The existing performance CLI enforces 1920x1080 and is not a drop-in command for this reference VM.
- `MIRROR_BOOT_OK` precedes `run_shell` in `kernel/src/main.rs`; `kernel/src/input.rs` drains queued events when the input session begins. It is not a shell input-readiness fence. The current console implements `status` and `echo`, but not `restart` (`kernel/src/console.rs`). Default builds deliberately emit no typed-text oracle.
- [0017c](0017c-absolute-pointing.md) still requires VMware verification of VNC absolute coordinates without Tools. Its `restrict_backdoor` setting remains proposed. This lane does not silently enable it or assume the pointer checks passed.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| VMR-D1 | Add an opt-in Mirror-only lane, separate from `test.ps1`'s QEMU boots. Initial cases are boot and host suspend/resume; guest restart joins only after 0058a. A requested case whose prerequisite is missing returns `blocked`, never `pass` or an omitted result. Implements G3a and the proposed Phase 0 lane. | proposed |
| VMR-D2 | Use an explicitly supplied, dedicated, unencrypted test VM prepared from the current reference settings. Initial profile is 1 GiB/1 vCPU, with its packaged default kernel and no `agent-demo` or `mirror.perf=` token. Keep the firmware-selected display mode. G11 coverage is a later explicit profile addition after RD16 is resolved, not an unnoticed switch to 12 vCPUs/20 GiB. | proposed |
| VMR-D3 | Use `vmrun -T ws start <vmx> nogui`, `suspend <vmx> hard`, and bounded `stop <vmx> hard` for host-owned cleanup. Never send soft power operations or substitute host reset for the guest `restart` case. Success requires guest evidence after the host command, not its exit code alone. | proposed |
| VMR-D4 | Add a small `tools/vm-probe/` std-only host workspace, reusing reviewed RFB and PPM primitives from `bbc88e4` with source provenance recorded. Add lane-specific keyboard/frame checks; do not cherry-pick the whole preserved branch, import guest benchmark helpers, or weaken 0018's resolution contract. No new third-party crates. | proposed |
| VMR-D5 | Read/write VNC only on the verified loopback listener of the selected VM. Use RFB 3.8, security None, Raw and DesktopSize. Initial typed checks need keyboard events only. A future absolute-pointer case requires 0017c's fresh readiness marker and recorded VMware check 8; it never falls back to guessed relative moves. | proposed |
| VMR-D6 | Acquire one exclusive host-wide run guard across Mirror checkouts before inspecting mutable run state or touching `build/dist`. Existing build/run/capture/package/test entry points must participate when this lane lands; nested calls retain the same owner. Busy or uncertain ownership fails before VM input. All commands and observations have the finite deadlines below. | proposed |
| VMR-D7 | Operate only a VM proven powered off at entry, preserve an append-only run record, and leave it powered off on success. On failure capture available evidence first, then attempt bounded cleanup of only the VM this run started. An ambiguous power state blocks later runs until explicit recovery; no automatic retry of a possibly executed state-changing operation. | proposed |
| VMR-D8 | The default build's serial contract stays unchanged. Keyboard success needs a fresh change in the console output region after Enter, excluding the edit line and cursor. Readiness is observed before sending any action; each key has one down/up pair, and Enter/restart are never resent after an uncertain outcome. | proposed |

## Interfaces

### Entry, VM preparation and ownership

Proposed entry: `scripts/vmrun.ps1 -Vmx <absolute-path> -Case boot|suspend-resume|restart [-Vmrun <absolute-path>]`. It consumes an already packaged image; it neither builds nor repackages it. Run each case independently from a confirmed powered-off state. No `all` default hides an unavailable restart case.

Resolve `vmrun.exe` from `-Vmrun` or the 64-bit `HKLM\SOFTWARE\VMware, Inc.\VMware Workstation` `InstallPath`, following 0018. Record its full path and version. Invoke processes with argument arrays and hidden windows; never interpolate VM paths into a shell command. No guest credentials or encryption passwords are needed or accepted in this first scope.

The owner prepares the dedicated VM while it is off, preserving every [reference setting](../vm-settings.md). Add only the VNC keys from 0018: `RemoteDisplay.vnc.enabled = "TRUE"`, `RemoteDisplay.vnc.ip = "127.0.0.1"`, and a reserved port in 5901-6001; no `.password` or `.key`, and an unset `.keyMap` (US). Record the configured port, VMX identity, disk paths and serial path. Do not change 3D acceleration, RTC, virtual hardware or the display mode to make a failed check pass.

Preflight validates canonical VMX/image/serial paths, settings, artifact sizes/hashes, flavor evidence and ownership before starting anything. The disk must resolve to the selected checkout's `build/dist/mirror.vmdk` and adjacent `mirror.img`, with no snapshot chain or additional writable data disk. Refuse active or suspended VMs, an existing recovery record, an occupied VNC port, encrypted/unreadable configuration, and evidence paths that escape their approved roots or traverse reparse points. Absence from `vmrun list` alone cannot prove powered off: suspension also removes a VM from the running list. Verification of VMware's stopped/suspended state indicators is an implementation prerequisite, not an assumption.

After start, verify the listener's local address and owning VM process before connecting or injecting keys. A wildcard/LAN listener or unverifiable owner fails the lane. No firewall rule is added automatically. The run guard covers this VM and the shared build artifacts; it does not authorize stopping another VM or evicting another agent's run. A held guard is released after normal cleanup; process exit releases its handle, but an unresolved-VM recovery record continues to block another run. Never delete VMware `.lck` directories as recovery.

VMR-D6 extends today's manually coordinated one-script-at-a-time rule into an enforced guard across existing script entry points. That additional implementation scope needs Cameron's explicit decision; it is not already required by a confirmed automated-lock design.

### Bounded observations

All budgets below are proposals under VMR-D6. Use host monotonic deadlines for the complete operation, including retries; receiving a byte or a marker does not restart a deadline.

| Operation | Bound and failure behavior |
| --- | --- |
| Run-guard acquisition and preflight | Fail immediately on an occupied guard; 15 s for state/listener queries. |
| Each `vmrun` lifecycle command | 60 s; capture exit code/stdout/stderr. On timeout terminate only the owned command process, not `vmware-vmx`; regard the VM operation as unresolved until a bounded state query proves otherwise. |
| Boot or resumed guest readiness | 120 s total; RFB attempts each at most 3 s and capped by the remaining budget. Reconnect only an observation connection; never repeat start/suspend/restart as a retry. |
| Typed console check | 15 s for a complete baseline and 15 s after Enter; key down/up writes each at most 100 ms, bounded key count 32. Always attempt release on the error path, then fail on ambiguity. |
| Suspend dwell | 2 s after positive host suspended-state evidence; no guest timer-progress requirement while suspended. |
| Failure cleanup | 30 s for bounded evidence capture, then at most 60 s for a permitted hard stop and 15 s to verify off. If another power operation may still be in flight, record unresolved state instead of racing it with a second command. |

Keep one RFB update request outstanding. A DesktopSize record invalidates old pixels and coordinate assumptions; obtain complete coverage and recompute geometry before comparison. Do not issue a new request merely because a read timed out while the previous request remains pending. RFB bounds stay at 4096 pixels per dimension, 4096 rectangles per update, 128 MiB per message and 1 MiB per ignored string. Unexpected messages/encodings, out-of-bounds geometry, truncation or malformed markers fail the case.

### Guest checks

1. **Boot.** Start once. Require a fresh serial boot epoch with exactly one `MIRROR_BOOT_OK`, `MIRROR_FRAMEBUFFER_READY`, and the expected keyboard/device readiness. Validate the existing 0013/0015a requirements against the selected profile: online CPUs equal the configured count, no APIC/ACPI fallback, AHCI sector-0 probe success, the AHCI catalog record and e1000 `8086:100f` record. Use the existing marker field contracts, including duplicate/unknown-family rejection, without imposing QEMU-specific PCI counts on VMware. Reject panic/exception/user-fault, test-battery and `MIRROR_TRACE_ON` output.
2. **Enter the console.** Wait for a complete shell frame identified by a small, versioned pixel predicate from the current renderer, using framebuffer dimensions and the centered desktop extent; a stable firmware/boot screen is insufficient. Send Escape once and wait for fresh `MIRROR_SHELL_CLEANUP_OK` and `MIRROR_CONSOLE_READY`. Missing readiness is failure, not permission to keep sending Escape. This intentionally ends the shell session, as today's UI does.
3. **Typed check.** At the empty console edit line, send `status` as separate key-down/up pairs. Obtain a complete frame after typing and before Enter. Send Enter once and require a later complete frame whose output rows change, with the edit line and cursor excluded. Keep both frames and the exact comparison region. This proves a submitted console command produced visible output; it does not measure latency or establish wall-clock correctness. Freeze the renderer-specific readiness/output predicates and verify them against negative fixtures before using them as oracles.
4. **Host suspend/resume.** After boot and a successful typed check, submit a bounded run-specific `echo` sentinel and preserve the visible output frame. Close VNC, suspend once with `hard`, verify actual suspended state, then `start ... nogui` once. Reconnect with a fresh RFB client; require retained sentinel/history, no new boot epoch, no fatal marker, and another successful `status` output change. A fresh reboot that happens to answer input fails continuity. Host suspend success without responsive resumed input fails the guest check. Clock resync, elapsed-time accuracy, timer rearm and Tools `OS_Resume` handling are explicitly untested here.
5. **Guest restart, after 0058a.** Require its confirmed command/diagnostic contract and a build containing that implementation; otherwise return `blocked` before power-on. From a checked console, type `restart` and press Enter once. Require a distinct fresh boot epoch, revalidate boot evidence, reacquire shell/console readiness and repeat `status`. Do not use `vmrun reset`, another Enter or a new start command to rescue a failure. Adopt 0058a's actual marker contract when it exists; this draft invents no successful guest-reset marker.

A serial epoch is tied to this invocation, not to an old occurrence in a reused file. Before starting, copy existing serial/VMware logs to the new run directory without deleting them. During the case, track serial file identity and offsets and preserve each observed generation. Capture the pre-transition segment before suspend/restart because file-backed serial can be reopened or replaced. Unexplained shrink/rotation, missing first lines or an unprovable restart boundary fails evidence validation; do not concatenate old and new text and accept the first matching marker.

### Artifacts and files

Each invocation creates a new `build/vm-logs/lane/<run-id>/` directory; never overwrite a previous run. Preserve a manifest with source commit, dirty state, kernel profile/features, image and descriptor hashes, VMX hash and validated settings, VMware version, monitor-mode line, host times, per-step result and cleanup state. Capture bounded command stdout/stderr, serial generations, VMware log segments, RFB version/security/size metadata, before/after PPMs and the first failing frame when obtainable. Do not claim synthetic RFB fixtures are VMware recordings.

Proposed per-run caps: 1 MiB per command-output stream, 64 MiB each for captured serial and VMware logs, and 16 frames. Overflow fails the run while retaining the bounded prefix and explicit truncation status. A final manifest is atomically published as `pass`, `fail`, or `blocked`; a missing final record means incomplete, never success. Preserve the first failure even if cleanup succeeds. Recheck packaged artifacts after verified power-off and report any unexpected mutation; never silently rebuild over it. Do not save credentials or full unreviewed VM configuration in evidence.

Implementation would touch `scripts/vmrun.ps1`, a shared run-guard helper and participating script entry points; `tools/vm-probe/`; host-check integration in `scripts/test.ps1`; and matching command/testing/settings documentation in `AGENTS.md` and `docs/vm-settings.md` (P9). It adds no kernel syscall, guest marker, power implementation or driver. This draft changes only this file.

## Acceptance tests

1. **Host-only checks.** Formatting, clippy with warnings denied, and host tests for the new workspace use the pinned toolchain, `--locked`, and `x86_64-pc-windows-msvc`. Reused RFB tests cover handshake, malformed/truncated/oversized data, total deadlines, resize coverage and exact key bytes. New fixtures cover console readiness versus firmware/static boot images, cursor/edit-line-only changes, complete versus partial frames, and fresh versus stale serial epochs. Synthetic and captured fixtures have distinct provenance.
2. **Orchestration failures.** With fake command/RFB/serial peers, prove: busy guard issues no command; running/suspended/unknown entry state is rejected; another VM's listener is rejected; a command timeout cannot trigger repeated start/suspend/restart; missing or duplicate evidence fails; resize cannot reuse old pixels; lost key-up fails; cleanup never targets an unrelated process; and artifacts survive failure. A second invocation cannot erase an incomplete run or bypass its recovery record. No VM is needed for these cases.
3. **Actual VMware boot.** On the prepared reference VM, run the boot case once for debug and once for release, each from a fresh `package.ps1` artifact while the VM is off. Record the actual Workstation/monitor/display values. Boot, console `status`, final hard-stop and artifact checks pass with no user-content serial output or smoke/trace markers. Save a real, minimal RFB handshake/resize capture if a resize occurs; do not manufacture one to satisfy the test.
4. **Actual host suspend/resume.** Run once per profile. Positive host-state evidence, retained console sentinel/history and new post-resume command output are all required. Record unsupported/error outcomes as failures or blocked external prerequisites, not successful guest power support. Preserve evidence for the owning guest spec if the VM resumes but input or rendering does not.
5. **Restart dependency and later run.** A build without confirmed 0058a is rejected before start. After that spec lands, one invocation per profile must show exactly one requested guest restart, a new boot epoch and a successful post-restart typed check, without a host reset fallback.
6. **Integration.** Script entry points honor the same guard and the P9 documentation matches their behavior. Run the repository's full `test.ps1` before merge per P8, plus `-AgentDemo` only if its code is touched; the VMware lane itself remains explicitly invoked. Final results name the cases actually run and every blocked case. No other-OS artifact or benchmark result is produced.

## Dependencies

- New third-party crates: none proposed. The new host tool uses Rust `std`, pinned Rust 1.99.0, and the existing MSVC linker/Windows SDK. Review the reused Mirror source at `bbc88e4`; do not import its unrelated benchmark work.
- Existing external runtime tool, not linked or redistributed: VMware Workstation Pro 26.0.1 build 25688693 (proprietary, Broadcom), including verified installed `vmrun` 1.17.0.25688693. A different build requires rechecking syntax/state evidence and recording the change; do not silently download an upgrade.
- Existing Windows PowerShell/.NET facilities provide process execution, file sharing, hashes and the run guard. No new image library, Python package, guest Tools installation or guest OS image.

## Open questions

1. Cameron: confirm/number the lane and VMR-D1-D8. Does initial acceptance use the existing 1 GiB/1 vCPU reference VM as proposed, or should RD16's G11 profile be resolved first? The roadmap's G11 convention is not silently adopted here.
2. Cameron: confirm the small separate `vm-probe` workspace versus putting a distinct functional-check command beside the preserved `perf-probe` primitives. Either choice must leave 0018's benchmark resolution and deferred scope intact.
3. External prerequisite: which installed VMware state indicators reliably distinguish powered off, suspended, running and a transition, and identify the owner of the VNC listener? `vmrun list` alone is insufficient. Establish this read-only first; if it cannot be established, keep lifecycle execution blocked rather than infer state from port availability.
4. External checks: does the default Mirror build resume with retained console state and functioning PS/2 input under this host's ULM mode; do VNC Raw frames represent the guest correctly with the reference 3D setting; and how do serial/VMware log files change across suspend/start and guest restart? None is established by installed help or the synthetic RFB tests.
5. 0058a must supply the guest restart contract before that case is implemented. Phase 5's orderly power and `OS_Resume` contracts stay deferred even if host suspend/resume passes. Absolute-pointer automation likewise waits for 0017c's recorded VMware verification.
