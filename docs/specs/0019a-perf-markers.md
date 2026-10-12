# 0019a: Perf configuration, milestones and memory

Status: draft. Only inherited D1 is confirmed. New choices need Cameron's confirmation before implementation or VM experiments.
Depends on: 0010, 0015a, 0016a/b and merged 0017a/b/c; confirmation of the split and affected contracts. Runs before 0019c, 0019d and 0030a.
Ledger: S7a/S7b, S1c, S6f, S10, G7, C3; related confirmed contract 0018-D9.

## Goal

Provide opt-in boot milestones and idle-memory samples using the same kernel and modules as daily boots. Add a tested protocol crate, image/token delivery and exact log checks. Input capture moves to [0019c](0019c-perf-input.md); BSP burn moves to [0019d](0019d-perf-burn.md). These proposals replace part of 0019 after confirmation.

## Non-goals

Input collection, burn execution, scheduler changes, host tooling, AP timing, TSC calibration, agent benchmarks and telemetry. 0019-D2 stays confirmed in 0019; D8-D10 belong to 0019b after 0062. Flip is reserved until D19's follow-up. No other-OS work.

## Decisions

| ID | Decision, reason and alternative | Status |
| --- | --- | --- |
| 0019-D1 | Perf markers are compiled into every build. The heavier ones (memory samples, input trace, display calibration, agent bench) are switched on by `mirror.perf=` tokens, so daily and measured boots run the same kernel binary. No marker carries user content. Timing markers that depend on input appear only with a valid `mirror.perf=` token. (Reworded 2026-10-09 with ledger S7b: the last sentence replaces "In default builds, these opt-in markers are the only serial output that may depend on user input".) | confirmed (ledger S7b; 0018-D9) |
| 0019-D3 | Gate all perf lines, including milestones, except invalid-token OFF. Ordinary boots then have a zero-count oracle. Alternative: retain 0018's always-on milestones. This narrows confirmed 0018-D9 and needs re-confirmation. | proposed |
| 0019-D4 | Use the bounded grammar below. Invalid configuration disables perf; smoke still rejects it. Alternative: ignore unknown items, hiding measurement typos. | proposed |
| 0019-D5 | Actual HPET snapshots only; `since_entry_ns=none` until 0055a. TSC rate is uncalibrated and RDTSC may trap under ULM. Alternative: checked TSC/HPET calibration here, with additional VMware validation. | proposed |
| 0019-D6 | Observe the interactive process's physical-surface and Input syscalls. `frame` means one tile, not complete paint. Alternative: a frame-complete/ready ABI, deferred to the compositor contract. | proposed |
| 0019-D11 | New native `mirror-perf-protocol`: `no_std`, `forbid(unsafe_code)`, inherited lints, no dependencies. Own grammar, exact schemas and host tests. Alternative: kernel-only logic, harder to test. | proposed |
| 0019-D12 | QEMU perf boots use SMBIOS; VMware uses a separate perf image with identical kernel/modules and CID `50455246`. This avoids rebuilding QEMU images for each token and gives VMware unattended, reproducible token delivery. It explicitly amends 0015a's rule that every capture uses embedded config without SMBIOS. Tokenless captures retain that rule. Alternative: private QEMU perf images too, or timed boot-menu selection. | proposed |
| 0019-D14 | Daily images remain unpinned, preserving S1c and confirmed 0015a. Cameron declined RD8's temporary daily pin on 2026-10-12; no pin choice remains open here. Perf's requested 1920x1080 stays confirmed. | settled by confirmed RD8 (2026-10-12) |
| 0019-D15 | Divide the Phase 1 work in order: 0019a configuration, milestones, memory and delivery; 0019c finite input; 0019d BSP burn; all before 0030a. This bounds each implementation. RD8 already places 0019's Phase 1 part before 0019b's Phase 6 C3 telemetry, and RD24 confirmed these letters on 2026-10-12. Only the three-way division and its order remain proposed. Alternative: one broad Phase 1 spec. | proposed |
| 0019-D16 | Re-confirm all 0018 amendments together: gated milestones; added clock/frame and actual HPET/unknown entry-offset semantics; INPUT irq/irq_ns replaced by numeric origin/start and delivery-fenced first-tile semantics; ON/OFF/SKIPPED/WINDOW/BURN schemas; bounded collection; a separate `perf-probe input --protocol phase1-keys` mode owned by 0019c's implementer; explicit Mirror-row tokens and `perf.ps1 -Gate all` repackaging between them; one unmeasured absolute priming move in future Mirror setup (0019c); and delayed flip. Preserve baseline thresholds, warmups, counts and pacing. Mirror input rows use `boot`, not finite capture; 0019c's separate Keys and Click runs, including vmmouse origins, answer 0019 open question 7. Alternative: preserve the old contract and redesign these slices. | proposed |
| 0019-D19 | Recommend a small calibration slice owned by 0027c's implementer in Phase 4, before 0018's calibrated Mirror rows, because 0027c supplies the frame-time hooks. It must implement 100 fills at 500 ms intervals and explicitly choose smoke placement. Alternative: a separately numbered pre-0030a calibration slice assigned by Claude. Until that confirmed follow-up lands, flip stays unavailable and calibrated Mirror rows cannot pass. | proposed |

## Interfaces

### Configuration

Split command-line tokens on ASCII whitespace. Accept exactly one case-sensitive `mirror.perf=` token, at most 64 bytes including prefix. Items are `boot`, `1`, `mem`, `burn`, `flip`, `telemetry`, `agent`. Reject non-ASCII bytes, empty/duplicate items, `mem,1`, and burn without 1. Allow redundant boot with any other item, including boot,1. Item order has no effect.

Every valid token enables milestones. mem enables memory; 1 requests memory and input. Input and burn remain unavailable until 0019c and 0019d respectively. Flip, telemetry and agent are reserved. Configuration initializes immediately after disabling interrupts in _start, without allocation, printing, TSC reads or a panic on a missing Limine response. Missing command line means off.

After serial initialization and the banner, emit ON or invalid OFF **before MIRROR_TRACE_ON**. A tokenless boot emits neither. Smoke skips a perf argument only if the whole configuration is valid; otherwise its existing strict parsing still panics. Other boot-policy/fault parsing stays strict.

### Records and ordering

Names, field order and identifier values are closed-world. Counters are decimal u64; timestamps are decimal u64 nanoseconds or none. Saturation/overflow invalidates measurement. MEM contains only numbers/none.

| Record | Exact fields and occurrence |
| --- | --- |
| `MIRROR_PERF_ON` | `v=1 mem=<0\|1> input=<0\|1> burn=<0\|1>` once; flags describe requests, not availability. |
| `MIRROR_PERF_OFF` | `reason=invalid` once; no other perf records. |
| `MIRROR_PERF_T` | `event=<clock\|display\|input\|userspace\|frame\|shell_ready> ns=<n\|none>`, each reached event once in this order. Only clock adds `since_entry_ns=none`. |
| `MIRROR_PERF_MEM` | `ns= available_kib= pool_kib=` at interactive userspace start and later sample points. |
| `MIRROR_PERF_SKIPPED` | `item=<input\|mem\|burn\|flip\|telemetry\|agent> reason=<smoke\|agent_demo\|no_clock\|no_session\|unavailable>` once per skipped requested item. 0019d adds the runtime burn reason `create_failed`. |

After input initialization and selection of the interactive-session path, before either interactive userspace start or MIRROR_DESKTOP_SKIPPED, emit required skips in order input, mem, burn, flip, telemetry, agent. Precedence is smoke, then agent_demo for input and burn, no_clock, no_session, and finally unavailable. AgentDemo still supports milestones and memory; it allocates no input capture state and emits no INPUT, WINDOW or BURN. No_session means the framebuffer is absent or neither PS/2 device is ready. Reserved flip/telemetry/agent use unavailable. A fatal scenario before this decision point emits only the reached prefix. A later attempted burn launch may emit create_failed as 0019d defines; it never repeats an earlier burn skip.

Clock snapshots after HPET initialization; its time is not forced to zero. Without HPET it still prints `ns=none since_entry_ns=none`; later timestamps are none. Display follows FRAMEBUFFER_READY. Input follows PS/2/vmmouse setup and IRQ enabling even without a framebuffer; in that case display is omitted, and input still prints. Userspace precedes the first resume of the selected interactive shell or AgentDemo desktop, excluding smoke init.

Frame is that process's first positive physical-surface present. Empty, failed and offscreen presents do not count. Shell_ready is its first physical Input receive/poll after frame with valid capability, exact length and writable buffer, including EAGAIN. Today's clients paint before polling; that source invariant is not a frame-completion ABI.

Smoke shell mode 3 enters the non-returning battery. Smoke emits only reached clock/display/input milestones and skips operational input/mem/burn. Missing framebuffer or interactive session omits its later milestones. Keep existing smoke batteries intact.

### Memory and future scheduling

Sample through the scheduler's allocator borrow at interactive userspace start. Available_kib is four times recycled plus never-allocated frames; pool_kib is four times total allocator frames. Failed stats produce none for both. These are allocator counts, not all configured RAM.

With HPET, sample on the first scheduler pass at least 10 actual seconds after the previous sample. No catch-up burst. Without HPET, a session that starts retains initial MEM with ns=none and skips periodic memory with no_clock. If no session starts, emit no MEM at all, including when no_clock wins skip precedence. Never use delivered ticks as wall time. Standalone mem supports continuous idle observations. Sampling ends with the session; the later kernel console is unsampled. 0018's minute 10–12 still starts at host-observed boot completion. 0019c owns buffering during collection.

Current hooks/snapshots require BSP with IF clear. 0030a must preserve hooks while moving scheduler state. Before 0051 dispatches measured processes on APs, it must make stamps/state AP-safe or keep those processes and their hooks on the BSP. 0055a owns calibrated cross-CPU time semantics; extending burn alone is insufficient.

### Delivery and checks

`run.ps1 -Perf <items>` and `capture.ps1 -Perf <items>` validate before build/boot and inject SMBIOS. Add `capture.ps1 -PerfCapture -Perf <items> -PerfScenario Markers`: build default flavor, use pixel readiness, and reject trace/init markers. It is mutually exclusive with Hygiene, which stays tokenless. Ordinary captures remain trace flavor. Add capture -NoHpet using run.ps1's QEMU hpet=off setting. NoBuild still requires the matching flavor.

Markers never injects an arming key and ends the QEMU run within its 60-second cap. Where input capture is available, it ends before the setup deadline and validates a prefix with input still pending. With AgentDemo, input is skipped, so no setup timer or pending capture exists. Both forms require no INPUT or WINDOW and report no input measurement success. Full input scenarios instead require terminal accounting.

`package.ps1 -Perf <items>` validates the same required item list before building, privately copies staging under build/dist/perf, writes exactly that token and the perf resolution to Limine config, builds/verifies the image, and compares kernel/module hashes with daily staging. A bare -Perf is invalid. Request 1920x1080 and record actual GOP dimensions; fallback cannot pass resolution-specific acceptance. Add `mirror-image disk --perf` (once, no value, disk-only) for CID 50455246; default stays 4d495252. Validate the complete descriptor schema.

After D16 confirmation, 0018's Mirror boot and key/click rows use a `boot` image, and idle-memory rows use `mem`. They never use `1` or `burn`: no finite input window can seal or drain in the middle of their warmups, repeated sessions or click-only run. 0019c's dedicated diagnostics use `1`; its loaded Phase 1 run uses `1,burn` after 0019d. Each row records its token and perf-image hash. The separately authorized 0018 calibration uses `flip` only after D19's follow-up implements it; agent rows retain their separately confirmed future contract. These assignments amend 0018's kernel diagnostic expectation, not its baseline method.

The future 0018 owner changes `perf.ps1 -Gate all` to power off and repackage with the required item list between boot, idle and input groups: `boot`, then `mem`, then `boot`. Never overwrite an attached image while its VM is running. Each group uses the matching token image and powered-off perf-base snapshot prepared and verified through 0018's existing CID procedure; reversion must not silently restore another group's image. Record each image hash and token in its rows. This sequencing and 0019c's unmeasured priming amend 0018 setup; they add no warmups or measured samples. Implementing 0018 remains deferred until assigned.

common.ps1 adds `Assert-MirrorPerfMarkers -Items`, parameterized by flavor, clock availability, session availability and expected fatal endpoint. Reject unknown names/fields/values, duplicates, unexpected records, bad event/skip order and missing required records. Check no_session precedence and its absence of MEM and later milestones; no_clock alone still permits initial MEM when a session exists. Check milestone-time monotonicity and memory bounds/spacing; do not require global timestamp ordering across buffered families. 0019c adds recomputed WINDOW accounting and teardown output order. `Assert-MirrorNoPerf` requires zero MIRROR_PERF_ lines in **every tokenless smoke/capture log**, including AgentDemo and hygiene.

`run.ps1 -Smoke -Fault PerfInvalid` injects raw mirror.perf=bogus, rejects combination with public -Perf, and adds no kernel SmokeMode. Expect one OFF, exact existing unknown smoke-test command line panic, and exit 35. Reject that scenario's evidence elsewhere.

P9 implementation handoff: the commit adding these interfaces also updates AGENTS.md's Build/Test and When to test sections for `run.ps1 -Perf`, `capture.ps1 -Perf/-PerfCapture/-PerfScenario Markers/-NoHpet`, default-flavor perf captures, `package.ps1 -Perf <items>`, `mirror-image disk --perf`, protocol checks and added suite caps below. Add the PERF family to Serial markers' closed-world common.ps1 list (AGENTS.md:65 at `8e43e55`). Fault injection must identify PerfInvalid as raw host-token injection rather than a kernel SmokeMode or QEMU option. Claude coordinates the AGENTS edit; this proposal does not edit that hot file.

## Acceptance tests

1. Add protocol host tests to test.ps1: grammar boundaries, exact schema round trips, malformed/unknown/duplicate fields, skip precedence, milestone order, no clock, overflow, memory arithmetic/spacing. PowerShell fixtures invoke actual common.ps1 functions with valid/corrupt logs in Windows PowerShell 5.1, the supported inbox host. Include no display, neither input device ready, no_clock plus no_session, and runtime-skip ordering fixtures.
2. Add three smoke cases per debug/release profile: `-Smoke -Perf boot,1,burn`; `-Smoke -Fault PageFault -Perf boot`; `-Smoke -Fault PerfInvalid`. AgentDemo suite passes its switch to each; smoke skip wins. Keep existing diagnostics/image-integrity checks. Add protocol host tests and zero-count checks to both full suites.
3. Add default PerfCapture Markers runs per profile: `-Perf mem` and `-Perf 1,burn -NoHpet`, each capped at 60 seconds. First waits for three MEM samples. The no-HPET ordinary run skips input, mem and burn with no_clock, retains initial MEM none, and emits no periodic sample over 12 seconds. The same AgentDemo run uses agent_demo for input and burn and no_clock for mem, still emitting records in input, mem, burn order; it also retains initial MEM none. Add `-AgentDemo -Perf 1,burn` with HPET: input and burn skip agent_demo, while initial and periodic MEM remain enabled. In both AgentDemo cases require no INPUT, WINDOW or BURN. Capture caps across profiles total four minutes plus two for AgentDemo; the extra smoke boots keep their existing 30-second caps. These are limits, not claimed runtimes. RS6 retires AgentDemo cases when 0030a's implementation retires that build.
4. Once per relevant implementation change, also run trace `capture.ps1 -Perf boot`, pre-build malformed-argument fixtures, and debug/release package default/perf hash/descriptor checks. Run mirror-image workspace fmt/clippy/host tests. Long input/click/burn runs belong to 0019c/d, outside ordinary full suites.
5. Targeted checks and both full suites pass before implementation completion. Docs-only proposal edits need no runtime tests. VMware measurements require a separately authorized run; no other-OS acceptance here.

## Dependencies

No external crate, tool or download. The native protocol crate has no dependencies. Immutable evidence at 58461e0: kernel/src/main.rs:52-59,196,221,399,409; clock.rs:244-274,314-334; userspace.rs:1969-1978,2066-2107,2796-2807; userspace/desktop/src/shell.rs:1020,1028-1048,1052. These establish initialization, HPET reset, physical presents, client order and smoke exclusion. 0017c dependency evidence is 3c87bcb, not its orphan pre-rebase commit. Claude remaps citations before approved merge.

## Open questions

Confirm D3-D6, D11-D12, D15-D16, D19 and the 0019c/d choices. D14 is settled by RD8; RD24's letters are confirmed, but D15's three-way division and order are not. After confirmation, Claude updates ledger/map/status, affected 0018/0019/0015a contracts, the proposed division in Phase 1's map, its item 1 and exit, and the 0030a/0051/0055a migration handoffs. D18's VM choice explicitly amends RD16's rationale and the exit's G11 binding. D19's follow-up needs a number/owner assignment before implementation and before calibrated Mirror rows. No confirmed row is silently replaced.
