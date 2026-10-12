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

Source behavior was inspected at `970e48c`, the local `origin/main` during this review. Branch evidence is distinct:

- [vm-settings](../vm-settings.md) records Workstation Pro 26.0.1 build 25688693, hardware 22, UEFI, ULM and 1024x768. [0013](0013-firmware-and-pci-tolerance.md) records successful 1/4-vCPU boots, not resume; its 42 functions/34 bridges are observations, with the bridge count explicitly informational.
- `package.ps1` copies a default image without provenance. `Assert-MirrorKernelImage` scans the staged ELF, not an older package. [0015a](0015a-disk-image.md) asks whether VMware preserves the descriptor after boot; the inspected completion notes do not record that result.
- At `970e48c`, `ps2::prepare_keyboard` accepts only `0x02`; 0017b's code was inspected at `67cea58` (implementation `a6264e6`, `2f02c70`), with current completion notes at `f373b04`. Its D9 amendment accepts VMware's `0x41`; the notes record ten successful VMware cold boots before rebase, with unchanged 0017b code. It is not merged into the inspected main.
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
| VMR-D6 | Recommend acquire-only use of the existing directory lock for new cases and lane packaging; retained-lock recovery resumes only that exact run. This avoids changing manual holders' protocol. Alternative: general adoption, requiring separately designed handoff tokens and team documentation. | proposed |
| VMR-D7 | Recommend a fixed-root journal: retain the lock for possibly active states, release it on proven suspended failure, and require explicit same-run recovery. This protects the VM without blocking QEMU indefinitely when suspension is proven. Alternative: hold the lock through every failure until manual off recovery. | proposed |
| VMR-D8 | Recommend exact glyph/scene checks including the pointer sprite, with immediate key-up. This detects wrong keys and avoids typematic flakes. Alternative: changed-pixel checks, which cannot distinguish successful status output from an unknown command. | proposed |
| VMR-D9 | Recommend clean-commit `git archive` packaging in an isolated scratch tree with recorded hashes. It excludes ordinary edits to the working checkout at modest cost. Alternatives: sealed Windows sources (sharing handles/ACLs and recovery fixtures, materially larger scope), or a working-tree manifest with dirty packages refused (simpler but susceptible to edits during compilation). The recommendation does not claim hostile isolation. | proposed |
| VMR-D10 | Recommend Codex provision and own one lane VM, coordinated by Claude, at `C:\Users\engin\Desktop\Mirror-vm\lane`, with fixed staging shared across worktrees. This avoids repointing VMs per branch. Alternative: an owner-prepared VM per worktree, with separate ports and recovery state. Cameron must approve the owner/path/profile. | proposed |
| VMR-D11 | Recommend a separately authorized Codex prerequisite investigation before guest-lane implementation: state/listener/serial checks, pixel fidelity, disk preservation and manual recovery. This settles external facts before building against them. Alternative: defer experiments until all lane decisions are confirmed. Provisioning and power cycles are VM operations, not read-only work; this draft authorizes none. | proposed |

## Interfaces

### Entry, sources and implementation boundary

Proposed entry: `scripts/vmrun.ps1 -Vmx <path> -LaneRoot <path> -LockOwner <nick/task> -Case boot|suspend-resume|restart`, or `-Recover <run-id>` instead of `-Case`. New cases never adopt an existing lock, even with the same owner name. Optional `-Vmrun <path>` overrides discovery through the 64-bit VMware Workstation `InstallPath` registry value. Use argument arrays and hidden processes; no credentials, implicit `all`, build or repackaging.

The Rust lane owns deadlines, transitions, oracles, journals and evidence behind injectable command, clock, serial, state/listener and artifact adapters. The host spec also puts the lane-package controller in Rust; `package.ps1 -Lane [-Release] -LaneRoot <path> -LockOwner <nick/task>` is its thin entry. Thus the cargo host tests exercise production controllers with fake command runners and temporary files, without Pester or a second orchestration implementation.

Future scope: `tools/perf-probe`, `scripts/vmrun.ps1`, `scripts/package.ps1`, host checks in `scripts/test.ps1`, and matching AGENTS/P9 documentation. Correct `docs/vm-settings.md`'s creation paragraph: only bridges 4–7 are eight-function root ports; bridge 0 is separate. Add a separate lane profile, preserving the previously confirmed reference block. No kernel/userspace changes, `common.ps1` retrofit or other script lock edits.

### Recommended packaging and fixed VM binding

Lane packaging acquires/releases the machine lock independently of the later VM case and never changes VM staging. Resolve one clean committed tree and export it with `git archive` under `<LaneRoot>/package/<package-id>/source`, outside every checkout, with fresh output directories. Only this controller writes the scratch tree; cooperating agents never edit it. Inventory hashes before/after detect persistent changes; they do not prevent transient tampering by another process. If stronger isolation is wanted, choose D9's sealed alternative and specify sealing, unsealing and crash cleanup in the host spec.

Cargo searches ancestors and Cargo home as well as the project ([Cargo configuration hierarchy](https://doc.rust-lang.org/cargo/reference/config.html#hierarchical-structure)). Reject either `.cargo/config` or `.cargo/config.toml` in every ancestor above the export through the volume root. Use a fresh config-free `CARGO_HOME`, seeded only with required cached dependencies; preserve the exported project's own committed configuration. Clear/reject inherited source, compiler, flags and output overrides against an explicit recorded allowlist. Verify the pinned toolchain/cache first, use locked offline resolution, and copy the already verified Limine archive into the export's private `.tools` before invoking its build script. Do not junction mutable extraction output or download missing inputs.

Build default flavor without agent-demo/perf tokens. Copy the private build's image/descriptor to the calling checkout's `build/dist`; publish `mirror.package.json` there last and atomically: schema, package-id, source commit/tree, clean status, profile/features/toolchain, committed-source inventory and oracle version, plus image/descriptor sizes and SHA-256. Inventory only committed source files, excluding generated `build`, `target`, `.tools` and lane artifacts. All packaging modes invalidate an old record before replacing files; only `-Lane` publishes a new one. A partial/mixed generation fails validation. Approved 0017b implementation must be an ancestor of the recorded commit; restart additionally requires 0058a capability evidence. The later checkout HEAD or staged ELF is not package provenance.

The controller's `finally` removes only its canonically verified scratch directory, with no reparse traversal, retaining logs/manifests outside it. A crash leaves a package-id/owner record: the owner explicitly removes that abandoned scratch tree after establishing the packaging operation has ended; never delete by age or interrupt a live build. There are no ACL seals to undo in the recommended option.

Provisioning records one canonical VMX-to-LaneRoot binding. Preflight requires the VMX's resolved `sata0:0.fileName` to equal `<LaneRoot>/disk/mirror.vmdk`, its extent to be adjacent `mirror.img`, and `serial0.fileName` to equal `<LaneRoot>/serial.log`. Reject LaneRoot inside any Git worktree, beneath Cameron's `C:\Users\engin\Desktop\Mirror`, or through reparse aliases. The supplied root must match the recorded binding; another root cannot bypass its journal. No snapshots/parent chain or additional disk. With proven off state and lock held, stage both hash-verified files through temporaries and publish a generation last. After off, compare both hashes exactly; preserve and fail on mutation, never normalize it silently.

### VM profile and prerequisite investigation

Codex provisions only after D10/D11 are explicitly authorized, using the wizard or `vmcli VM Create ... -c other-64`. The proposed profile requires hardware 22, UEFI/Secure Boot off, 1 GiB/1 vCPU, SATA/e1000, HPET, no USB and reference 3D/RTC/unattended-start settings. Require `pciBridge0.present`, and bridges 4–7 present with `virtualDev=pcieRootPort`, `functions=8`. Record creation method. Add VNC enabled, loopback IP, a reserved port 5901–6001, no password/key and no keyMap override. Initial oracle supports recorded 1024x768; another firmware mode needs a reviewed oracle, not forced settings.

Reject contradictory/duplicate configuration, occupied port, unexpected devices, encrypted/unreadable configuration or escaping paths. Verify the listener belongs to this VM and binds only loopback before connecting. Bound VMX to 1 MiB, manifests/journals to 256 KiB and owner records to 16 KiB; lane-generated records have versioned schemas. No automatic firewall change.

Before guest implementation, the separately authorized investigation must record: reliable off/running/suspended/transition and power-operation-completion indicators (absence from `vmrun list` is insufficient); listener ownership; complete serial generations and, for recommended D5, concurrent reads and bounded delivery; Raw frame fidelity at reference 3D settings; **byte-identical staged image and descriptor after a power cycle**; and the manual suspended-recovery sequence below. Copy/rebuild identity does not establish post-VM identity. Test resume responsiveness separately from clock/Tools behavior. If a prerequisite fails, bring its evidence back for a decision, not an unapproved setting or transport change.

### Exact guest evidence

Under recommended D5, each initial/restarted boot supplies its fresh readiness before input; validate the entire epoch again after off. The alternative policy would instead defer serial verdicts, including shell/console markers, until off, while retaining pixel fences and requiring all epochs to remain available. Record the confirmed policy in the lane profile; no runtime fallback flag.

Use complete schemas from `kernel/src/{main,acpi,apic,smp,timer,pci,drivers,ps2,userspace}.rs`, reviewed against the packaged source. At `67cea58`, require these lines exactly once in order, with `elapsed_ms < 1000` (0017b's VMware threshold, not its QEMU threshold):

```text
MIRROR_PS2_INIT elapsed_ms=<u64> clock=hpet selftest=0
MIRROR_KEYBOARD_READY transport=i8042 set=2 irq=1
MIRROR_MOUSE_READY transport=i8042 packet=3 irq=12
```

Reject every PS2_TIMEOUT, keyboard/mouse skip, malformed/duplicate or unknown member of a checked family, and unexpected fallback/fatal/trace/test output. Apply 0013's boot requirements: BOOT_OK/framebuffer readiness, configured enabled/online CPUs, xAPIC, LAPIC timer, no ACPI skip/table skip, PCI omitted=0 and AHCI/NIC/SVGA records. For the proposed one-vCPU profile, the observed IOAPIC line is `MIRROR_APIC_IOAPIC_ID index=0 firmware=1 hardware=0`, before APIC readiness. Record 42/34 PCI counts and flag differences; do not turn informational observations into universal constants. Require in order:

```text
MIRROR_AHCI_READY sectors=2097152 readonly=1 port=0
MIRROR_AHCI_PROBE_OK fixture=0 reads=1
MIRROR_AHCI_CLEANUP_OK dma_reclaimed=1
```

Rust lane assertions use these VMware values; `common.ps1::Assert-MirrorAhciMarkers` assumes QEMU's 2048-sector disk. NIC catalog presence is not networking functionality.

### Pixel and input contract

Match packaged-source glyphs, geometry and UI tokens, with an explicit oracle version. Shell readiness matches `Shell::paint`'s MIRROR/Native desktop/Esc and workspace welcome glyph regions at their source positions plus centered surface offset. BOOT_OK/static pixels alone are insufficient: `input::Session::new` drains before shell execution. Send Escape once, then match the console header, exact initial two history rows and empty input; `run_console` drains before rendering. Required serial sequence after Escape is:

```text
MIRROR_SHELL_CLEANUP_OK apps_reclaimed=1 outcome=Exit(0)
MIRROR_POINTER_READY x=512 y=384 width=1024 height=768
MIRROR_CONSOLE_READY input=ps2 display=1
```

`framebuffer.rs::{ConsoleGeometry,glyph,draw_console_history,draw_console_input}` and `console.rs::{Console::process,Text::counter,parse}` define the oracle. At 1024x768: history origin (48,136), row spacing 24, prompt text x=72, edit origin (72,664), scale 2/advance 12. Compose the actual pointer afterward using `mirror_ui_tokens::CURSOR` and `Driver::draw_cursor`: 15×19 at (512,384), X writes RGB(3,6,10), O writes text color RGB(236,241,249), and dots preserve the scene. Require that stationary overlay, including over otherwise blank rows; mask nothing. The lane sends no pointer events or allows concurrent interactive input.

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
| Recover | 120 s, no automatic power operation. |

Suspend-resume uses 37 key pairs, the longest present case; allow one extra emergency key-up. Keep one RFB request outstanding across timeout, invalidate coverage/geometry on resize and require complete frames. Reviewed primitives retain caps of 4096 pixels/dimension, 4096 rectangles/update, 128 MiB/message, 1 MiB/string. Malformed input fails.

### Acquire-only lock and same-run recovery

New lane/package invocations atomically create the existing team directory, write the four readable owner fields plus `lane_schema=1` and unique `lane_run=<id>`, and hold an exclusive write/delete-excluding handle on `owner.txt` through execution/finalization. Any existing directory is busy for a new case, including the same nick/task. No tokens, borrowing, age/PID-based takeover or changes to other agents' free-text records. Document that these new commands acquire their own lock; ordinary scripts keep today's manual protocol.

Before staging, durably journal `preparing` with owner/worktree/run, bound VMX/root, paths and last proven state; before every power action, including restart Enter, record its intent. Partial staging is never a valid generation. On error release keys immediately and preserve the first failure. Stop hard once only for a positively running owned VM with no unresolved power operation. Unknown/running/transitioning state retains the journal and machine lock. A timed-out vmrun child may be terminated, never vmware-vmx; that does not prove VM state. In every case, Recover and manual sequence, unresolved power intent means transitioning despite an off/suspended snapshot: release/finalization requires reconciliation through D11's verified completion indicators, preventing a delayed start after lock release.

For a **positively suspended failure**, preserve a `suspended-needs-recovery` journal and evidence, then release the lane-created machine lock; it must not block QEMU while the VM is proven inactive. The journal still forbids starting a new case/restaging that VM. Suspension during the normal successful case keeps ownership until its planned resume.

`-Recover <run-id>` is an explicit continuation of that run, not general adoption. With a retained directory, require exact owner/worktree/run matches against its journal and obtain the same exclusive owner-file handle; an active original invocation or competing Recover prevents acquisition. With no directory, acquire it atomically for the recorded run. Missing/incomplete/mismatched identity goes to manual resolution; never overwrite another owner. Recovery performs no power action: off proof permits idempotent evidence archival/finalization; suspended state remains journaled and releases that recorded run's lock, whether retained or newly acquired; possibly active states retain it. It accepts partial/absent staged files recorded by preparing, and never repairs or deletes failed artifacts silently.

Candidate **manual suspended recovery**, to be verified in D11 and executed by Codex under Claude's coordination after authorization: (1) acquire the team lock atomically for the recorded run, or verify the exact retained run and obtain its exclusive lease; recheck suspended state/binding and capture evidence; (2) `vmrun -T ws start <bound-vmx> nogui` once; (3) after positive running evidence, `vmrun -T ws stop <bound-vmx> hard` once; (4) prove off, release the manual owner-file handle while retaining the directory, then invoke same-run Recover to archive/release it. Use the lifecycle deadlines above. Any ambiguous result stops the sequence and retains the lock; do not repeat start or delete VMware `.lck` files. Workstation's discard-suspended-state UI is an alternative only if separately verified/approved. These are candidate instructions, not claims that recovery has worked.

### Evidence and finalization

Write unique `build/vm-logs/lane/<run-id>` evidence: package/source/staging hashes, VM binding/settings, tool/monitor/oracle versions, policy, steps/deadlines and final power/cleanup state. Preserve old logs and each serial generation before possible replacement; unexplained truncation/missing early lines/unprovable epoch boundaries fail. Revalidate serial after off. Off proof and durable evidence precede a `finalized-off` journal with archive pointer; keep it through directory release so crashes during finalization remain recoverable. A later case checks final evidence and renewed off state before a new generation; retained ownership still requires explicit Recover. No final manifest means incomplete, not pass.

Checkpoint counts: boot four (shell, console, status edit/result); suspend nine (adds echo edit/result, retained history, new status pair); restart nine (adds restart edit, new shell/console, new status pair). Cap 12 normal plus four failure frames; do not save every poll. Cap command streams at 1 MiB each/16 MiB total, serial and VMware logs at 64 MiB each across generations. Overflow preserves the bounded prefix/reason and fails. Cleanup success never replaces the first failure. Label synthetic fixtures separately from VMware captures.

## Acceptance tests

The future host tests exercise the production Rust lane/package controllers with fake adapters and temporary roots; the wrapper invokes those controllers, so no unstated PowerShell framework is needed. Add these commands to test.ps1 and AGENTS when implemented:

```powershell
cargo fmt --all --manifest-path tools/perf-probe/Cargo.toml -- --check
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml
```

1. Fake peers cover occupied/same-owner new-case refusal, concurrent same-run Recover, incomplete identity, staging/finalization crashes, suspended lock release and reacquisition, unresolved start with an off/suspended snapshot, unknown-state retention, wrong binding/listener, missing/duplicate/PS2_TIMEOUT markers, no repeated power/input action, >200 ms key-up, resize and evidence caps. Pixel fixtures include the exact pointer over blank rows; wrong sprite, command, digits, sentinel and stale/partial frames fail. Review WIP RFB code and its malformed/deadline tests.
2. Package tests use a fake build runner with production archive/config/environment/publication logic: original checkout edits cannot affect exported inputs; ancestor/home Cargo configs and overrides are rejected; dirty source, missing cached inputs, attempted downloads, partial copies and stale/hash-mismatched records fail. Verify scratch cleanup/crash leftovers. No hostile-seal claim or fixture in the recommended option. Then run actual lane packaging debug/release sequentially and check source/tree/profile/hashes.
3. After separately authorized D11 experiments, confirmed decisions and merged 0017b, run boot and suspend-resume once each for debug/release. Preserve exact glyph/sentinel/pointer frames, complete epochs, actual settings, off proof and unchanged artifacts. Prove suspended-failure lock release, reacquisition and the candidate manual off recovery before enabling that workflow. Report unresolved prerequisites as blocked. Do not manufacture a captured resize if none occurs.
4. A requested restart without approved 0058a package capability blocks before start. Once its actual contract lands, run one guest restart per profile with a new epoch and no host reset fallback. Run the repository's full test.ps1 before merge per P8, plus AgentDemo where required by touched code; VMware cases remain explicit. Report every failed/blocked case, with no other-OS or benchmark result.

## Dependencies

No new third-party crates/test framework. Existing pinned Rust 1.99.0/std, MSVC/Windows SDK, Git and PowerShell/.NET; Workstation Pro 26.0.1 build 25688693 and vmrun 1.17.0.25688693 (proprietary Broadcom, not redistributed). Tool-version changes require recorded revalidation, not automatic downloads.

## Open questions

- Cameron chooses separately: D2 reference versus G11; D9 isolated archive versus sealed/working-tree packaging; D6 acquire-only versus a future adoption protocol; D5 live versus post-off serial policy; D10 Codex ownership/default root; D11 whether to authorize prerequisite experiments before guest-spec confirmation. Other rows also remain proposed, each with its alternative above.
- The investigation must settle state/listener identity, serial retention/latency, Raw fidelity, image/descriptor preservation and manual resume-stop recovery. A failed fact returns for a new choice, not a silent fallback. No experiments are authorized by this draft alone.
- 0058a must provide the guest restart and package-capability contract. Cameron numbers the host and guest specs after choosing the shared interfaces; ledger/map changes remain Claude's responsibility.
