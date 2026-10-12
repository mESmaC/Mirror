# 0019c: Finite perf input capture

Status: draft. Every decision below is proposed; implementation and VM runs require Cameron's confirmation.
Depends on: confirmed and implemented 0019a, merged 0017a/b/c; before 0030a. The Phase 1 exit additionally requires 0018's host probe and 0019d/0051/0052's load work.
Ledger: S7/S7a/S7b, S10, G7/G7a, G11; proposed RD8 and 0019-D16's amendments to 0018.

## Goal

Collect bounded input-to-next-physical-tile diagnostics without printing perf lines during collection. Make the host's visible-response measurement an explicit prerequisite for the Phase 1 exit.

## Non-goals

No input ABI change, complete-frame/causality claim, scheduler policy, burn implementation, flip calibration, AP clock implementation or other-OS work. A first tile after input delivery can be unrelated to the requested response.

## Decisions

| ID | Decision, reason and alternative | Status |
| --- | --- | --- |
| 0019-D7 | Recommend finite capture with internal event identities, then seal and drain outside measured work. Quota sealing stays quiet until the host finishes and exits the session. Idle-only draining has no guarantee after scheduler changes; immediate serial perturbs the measurement. The kernel endpoint is a diagnostic proxy only. | proposed |
| 0019-D17 | Recommend 200 keys, 256 input records, 40 buffered MEM samples, 60 s setup, 330 s collection and 30 s post-quota finish, with the host bounds below. At 1.5 s per key, 200 keys need at most 300 s; collection has 30 s margin and excludes setup. Alternative: fewer keys in a shorter window, with correspondingly weaker distribution reports. | proposed |
| 0019-D18 | Recommend the 0018-D10 `tools/perf-probe` host RFB/pixel instrument for the Phase 1 visible-response exit. It must land before that exit with the raw, uncalibrated Phase 1 protocol below; deferred flip calibration is needed for 0018's baseline rows. Amend RD8/Phase 1's exit to name it instead of claiming the kernel trace proves p99<30 ms. Alternative: make Phase 1 diagnostic-only and defer the visible-response gate; that changes the roadmap explicitly. | proposed |

## Interfaces

### Collection and identity

`mirror.perf=1` enables capture only with HPET and the interactive physical Input/surface owner selected by 0019a. Smoke and no-clock behavior retain 0019a's skip rules. Setup expires 60 HPET seconds after `shell_ready`. Before arming, clicks only update pointer state. Admit an arming key only before setup expires. Its first byte must be at or after `shell_ready` and, with burn, at or after burn dispatch. Its first-byte timestamp is WINDOW start; collection ends at start plus 330 seconds. Checked arithmetic rejects overflow. No rearming occurs this boot.

Discard partial events whose first byte predates arming, while preserving the pointer baseline. Accept the first 200 key makes, including autorepeat, and intervening button **presses**: one record when any supported button changes from up to down. Releases, motion, ACKs, errors and unknown sequences create no record. After loss, the first fresh pointer state establishes a baseline without fabricating a press. Untimed close-control presses are still records once armed.

Origins remain separate in every report:

| Origin | Start observation |
| --- | --- |
| 1 | Keyboard first byte drained in IRQ context |
| 2 | Relative pointer packet's first byte drained in IRQ context |
| 3 | Vmmouse DATA read producing a press during an IRQ drain |
| 4 | Keyboard first byte drained by polling |
| 5 | Relative pointer first byte drained by polling |
| 6 | Vmmouse DATA read producing a press during deferred/polled draining |

For PS/2, carry the first-byte timestamp and context through prefixes/packets. AUX routing identifies the device; the IRQ vector alone does not. A vmmouse doorbell is not the press. Snapshot each qualifying DATA read rather than borrowing its doorbell's time. Mixed prefix/packet contexts keep the first byte's origin. Do not pool IRQ and polled/deferred distributions.

Give each admitted event sequence 1..256. Carry bounded sidecars through decoder queues and Input Session pending/retry without changing the 32-byte user event. Delivery occurs only after successful copyout **and consumption** by the physical Input owner. Failed/retried receives cannot deliver twice. A successful positive physical present pairs all delivered, unpaired records with its completion snapshot; offscreen app presents, failed/empty presents and undelivered events do not qualify.

Count overlap when admitting a record while another remains unpaired. Preserve the original press identity across same-button coalescing, or cancel it explicitly; never replace it with a newer event's identity. Queue loss, fallback and discard cancel affected identities. Cancellation leaves their records unpaired, increments loss and requests abort sealing (reason 3); later input cannot repair the run. No per-event allocation or perf printing occurs while collecting.

### Sealing, memory and output

Seal after 200 keys when every admitted record is paired, at the active deadline, on session exit/abort, or when admission would exceed capacity. Setup expiry seals an unarmed window. Producers reject new records, pairings and buffered MEM at or after the collection deadline. Scheduler return/idle passes finalize sealing; this is not a hard real-time completion guarantee. Fatal/stopped guests produce incomplete runs.

Expose a kernel-only sealed state `{armed, start, end, reason}`, finish outcome and drain-complete flag to 0019d. Its failure path may request session-abort sealing before cleanup. Sealing freezes pending records as unpaired without itself incrementing loss, freezes output and blocks future collection. Retain records through normal session teardown until drained; no old identity may leak into a new session.

Initial/pre-arming MEM emits normally. During collection buffer 0019a's 10-second samples separately: at most 34 in 330 seconds, capacity 40. Buffer exhaustion increments overflow and seals invalid. Quota seal freezes INPUT/MEM/WINDOW and pauses MEM sampling. Defer all perf output and keep 0019d's load active until the host confirms the final pixels and sends ordinary Escape, producing normal Input-session exit within 30 seconds of seal. This also applies without burn. Escape then creates no new record. The kernel observes exit, not pixels; host evidence supplies that distinction.

At timely exit, stop/quiesce burn before draining retained data outside exited process contexts. A finish timeout, abnormal exit or nonquota abort allows immediate stop/drain and fails the gate. Enforce finish deadlines before dispatch/at scheduler passes; delayed handling cannot count a late exit as timely. Frozen quota WINDOW remains a proxy result on timeout, never proof of host success; the host must reject absent/late successful shell cleanup, and burn completion is zero. Drain at most four records per scheduler pass outside IRQ/decoder/present handlers; never require idle. Emit INPUT in sequence order, buffered MEM in sample order, then exactly one WINDOW. Resume ordinary MEM afterwards, without catch-up samples.

Exact field order, using decimal u64 values or `none` and no user content:

```
MIRROR_PERF_INPUT seq= origin= start_ns= present_ns=
MIRROR_PERF_WINDOW start_ns= end_ns= keys= buttons= records= paired= overlap= loss= overflow= unpaired= reason=
```

Unpaired `present_ns` is `none`. Reasons are 1=quota, 2=deadline, 3=session exit/abort, 4=capacity. Unarmed start is `none`, with zero keys/buttons/records. End is the logical seal/deadline timestamp, not the later drain time. Each armed record satisfies `WINDOW start <= start <= present <= WINDOW end`, omitting present when unpaired. Completed-event admission determines sequence; first-byte starts may reorder across devices. MEM timestamps are nondecreasing. Require `records=keys+buttons=paired+unpaired`, exact sequence coverage, and origin-derived key/button counts. Strict 200-key diagnostics require quota completion and zero overlap/loss/overflow/unpaired. Truncated/malformed serial invalidates the run.

### Host checks and visible response

Extend 0019a's proposed `capture.ps1 -PerfCapture -Perf <items>` default-flavor mode with `-PerfScenario Markers|Keys|Click|RelativeClick|Overlap|Deadline|Exit`. Input scenarios exclude AgentDemo; Markers retains 0019a's rules. PerfCapture excludes Hygiene. SMBIOS delivery follows 0019a's proposed 0015a amendment. Preserve trace captures and their budgets. Add `-NoHpet`; RelativeClick also requires `-RelativePointer`. Reject incompatible options before building.

Keys opens/focuses Notes before arming, parks the pointer outside the probe, and alternates `a`/Backspace for 200 keys. Every next key waits for the preceding expected text pixels and then 250 ms. Key down, release, pixel confirmation and gap together have a 1.5-second bound; release immediately after down, within 200 ms even on errors. A timeout fails the capture without injecting another measured key. After the final confirmed response, send Escape immediately. Both Escape and successful shell cleanup must occur within 30 host-QPC seconds of the final measured key-down, which precedes quota seal; do not start that host deadline at the later pixel observation. This conservative check applies even without BURN and rejects late completion hidden by a frozen WINDOW. These bounds allow throttle stalls without typematic repeats. Full runs have a 480-second outer timeout, including setup and drain. Raise capture's accepted maximum from 300 to 600 seconds; this does not extend guest deadlines or the ordinary default timeout.

QEMU uses QMP input and expected screenshot rectangles as functional assertions, never gate timings. The Phase 1 exit uses the same 200-key protocol through the single std-only `tools/perf-probe` RFB implementation. Host QPC starts at key-down write and ends at the first matching response pixels, with masked pointer/irrelevant pixels and recorded rectangles/tolerance. Preserve one pending incremental request; timeout releases input and aborts/reconnects without stacking requests. Record actual resolution/profile/monitor mode. Only release VMware results can qualify. Report raw host latency including delivery/display overhead, with no flip subtraction or dependency on deferred calibration. Use nearest-rank p99: the 198th sorted sample of exactly 200 successful samples; any timeout invalidates the gate. Compare it with the roadmap's proposed 30 ms bound. This is a separately labeled Phase 1 protocol, not an 0018 baseline row: its three sessions, warmups and random-gap method remain unchanged. Kernel timestamps never substitute for a missing host frame.

Click cases allow app lifecycle transitions. Set up one Notes window, arm with one visible key, close it, launch Notes once, then close that window. Each press waits for its expected pixels before the next action. Capture both launch and close presses; only the launch is the host click-response sample. Escape afterwards exits the shell and is itself a final unpaired key. Require two exact `MIRROR_APP_STARTED pid=N kind=0` lines, and for each PID `MIRROR_APP_RECLAIMED pid=N reason=closed status=0 frames_reclaimed=1` followed by `MIRROR_APP_CLOSED pid=N surfaces_reclaimed=1`. Require final `MIRROR_SHELL_CLEANUP_OK apps_reclaimed=1 outcome=Exit(0)`. These are ordered subsequences, not adjacency assertions. Extra launches, faults or alternate cleanup outcomes fail. Existing lifecycle output is permitted and its byte count reported; it is not suppressed or called perf-free timing.

Keys/Exit without burn instead leave their single Notes window open until Escape: one STARTED, then RECLAIMED with `reason=session_ended status=0 frames_reclaimed=1`, CLOSED and successful shell cleanup, with the same exact PID/field rules. With burn, 0019d additionally permits exactly one distinct hidden kind=1 PID and defines its lifecycle subsequence; all other launches remain forbidden. STARTED is never a substitute for rendered-pixel readiness.

Extend `Assert-MirrorPerfMarkers -Items` in `common.ps1` with closed-world INPUT/WINDOW names, full fields/order, bounded counters, scenario-specific origin/lifecycle rules and recomputed accounting. Repeated INPUT/MEM are bounded sequences, not duplicate singleton markers. Keep 0019a's `Assert-MirrorNoPerf` on every tokenless smoke/capture.

Markers is the explicit prefix-only exception: it never arms input and stops before the setup deadline after its required observations. Its checker allows input still pending, requires no INPUT/WINDOW, and cannot produce an input-performance verdict. This is distinct from a truncated measurement run, which fails.

## Acceptance tests

1. `mirror-perf-protocol` host tests cover all six origins, interleaved prefix/packet timing, coalescing/cancellation, failed copyout/retry, no-clock, overlap/loss, capacity, setup/collection/finish deadlines and immutable drain/accounting. Labels identify synthetic data. Include pre-arming partial events, and quota seal before the last expected pixels: no drain/load stop until timely normal exit; late/abnormal exit fails independently of unchanged WINDOW.
2. Keep 0019a's concrete smoke/PerfInvalid cases. `run.ps1 -Smoke -Perf 1` emits input/mem smoke skips and no INPUT/WINDOW; no workload runs inside the smoke battery.
3. In each debug/release default build, short `-PerfCapture -Perf 1` cases: `-PerfScenario Exit` types one visible key then Escape (keys=2, paired=1, unpaired=1, reason=3); Click and RelativeClick follow the lifecycle above (keys=2, buttons=3, paired=4, unpaired=1, reason=3); `-PerfScenario Markers -NoHpet` requires input/mem no-clock skips, initial MEM with ns=none, and no INPUT/WINDOW. Each has a 90-second outer cap. Absolute presses have origins 3/6 and relative presses 2/5, reported separately; synthetic tests force each branch.
4. Run full Keys captures in debug/release before this implementation completes and after input/perf hook changes, separately from ordinary `test.ps1`. Overlap batches makes and requires nonzero overlap; Deadline arms then waits for reason=2. Those targeted cases retain the 480-second outer cap. The routine non-AgentDemo suite adds the four short cases per profile (maximum 12 minutes plus build overhead); it does not silently add four full 200-key burn runs. 0019d owns burn combinations. Full suites and other AGENTS checks still apply.

## Dependencies

No external crate or tool. Extend 0019a's in-tree protocol crate. The 0018 host probe is a required Phase 1 exit dependency, not presumed to exist today. Do not create a second RFB workspace. Confirm D7/D17/D18 and 0019a's split/0018 amendments before implementation.

Source evidence: at `58461e0`, `kernel/src/userspace.rs:2607-2624,2796-2807` distinguishes consumed Input and physical versus app presents. At merged dependency `3c87bcb`, those paths are `2609-2626,2798-2810`; `kernel/src/mouse.rs:298-330,393-425` and `ps2.rs:777-804` expose coalescing, IRQ and deferred drains. `clock.rs:314-334` requires BSP/IF-clear snapshots. `userspace::ShellRuntime::{launch,reclaim,close}` supplies the lifecycle schemas; `userspace/desktop/src/shell.rs:1080-1081` exits before another paint on Escape. These are immutable source anchors, not future line locations.

## Open questions

Confirm the numeric bounds and Phase 1 host oracle. Claude's roadmap/ledger handoff must replace the old kernel-trace exit claim and list the host probe prerequisite. 0030a must preserve collection/seal/drain across scheduler changes. Before 0051 moves measured paths to APs, it must make perf state/timestamps AP-safe or explicitly keep the measured owner on the BSP; 0055a owns the calibrated cross-CPU time source. Neither later spec may call today's BSP-only snapshot from an AP. VMware evidence and all other-OS work remain separately authorized.
