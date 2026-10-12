# 0019a: Perf markers before the scheduler changes

Status: draft. Only D1 below is confirmed. Nothing here authorizes implementation, VM runs, downloads or other-OS benchmarks before Cameron confirms the proposed choices.
Depends on: 0010, 0015a, 0016a/b, 0017a/b/c merged; RD8 and this draft confirmed before implementation. Runs before 0030a.
Ledger: S7a/S7b, S1c, S6f, C3; related confirmed contract 0018-D9.

## Goal

Measure boot milestones, idle memory and input-to-next-scanout-tile latency before Phase 1 changes scheduling. Provide a bounded BSP userspace load using today's scheduler. Preserve the same kernel/modules for daily and measured boots. This replaces the instrumentation portion of 0019 only after confirmation; RD8's split and the associated contract amendments remain proposed.

## Non-goals

Device/link telemetry (0019-D2 remains confirmed through C3; D8-D10 move to 0019b after 0062), agent benchmarks (0045), TSC calibration (0055a), AP userspace (0051), scheduler policy changes, complete-frame or causal input-to-pixel measurement (0027c), and the kernel console fallback. Flip remains reserved: a dedicated calibration follow-up must settle its smoke placement and implement 0018's 100 fills/500 ms contract before Mirror's calibrated baseline rows can pass.

## Decisions

| ID | Decision, reason and alternative | Status |
| --- | --- | --- |
| 0019-D1 | Perf code is compiled into every flavor; heavier measurements require a valid `mirror.perf=` token. No user content is emitted; input-dependent timings require opt-in. Implements confirmed S7b and 0018-D9. | confirmed |
| 0019-D3 | Gate all perf lines, including milestones, except invalid-token OFF. This makes ordinary-boot hygiene a zero count. Alternative: preserve always-on milestones. This narrows 0018-D9/its always-on marker contract and needs re-confirmation. | proposed |
| 0019-D4 | Use the bounded exact grammar below; invalid configuration disables all perf, while smoke retains strict rejection. Reserve unimplemented items explicitly. Alternative: ignore unknown items, which hides measurement typos. | proposed |
| 0019-D5 | Use actual HPET snapshots, with `since_entry_ns=none` until 0055a. Avoid an unvalidated early TSC conversion across the guarded-stack transition. Alternative: ordered TSC/HPET pairs and checked offset/rate calibration here, with separate VMware validation. Host boot timing remains 0018's gate. | proposed |
| 0019-D6 | Observe the selected interactive process's physical-surface and Input syscalls as below. This needs no new syscall. Alternative: an explicit frame-complete/ready ABI, which would provide stronger semantics but belongs with the compositor contract. | proposed |
| 0019-D7 | Finite numeric capture, then seal and drain; define attribution, loss and proxy limits below. Avoid perf serial within the measured window. Alternative: idle-only draining, which occurs under today's throttle but has no general guarantee after scheduler policy changes. | proposed |
| 0019-D11 | Add in-tree `mirror-perf-protocol`: `no_std`, `forbid(unsafe_code)`, inherited lints, no external dependencies. Own grammar, exact line schemas and bounded capture-state logic with host tests. Alternative: kernel-only logic, harder to test independently. | proposed |
| 0019-D12 | QEMU uses SMBIOS; VMware uses a separate verified perf image with identical kernel/modules and a distinct fixed descriptor CID. Alternative: a second boot-menu entry, adding selection to the timed path. | proposed |
| 0019-D13 | Stage burn: one bounded worker in the current shell process now; 0051 owns extension to every online CPU after 0030a's capacity change. This preserves phase order without inventing AP kernel scheduling. Alternatives: separately design kernel AP load, or reorder phases. Amend roadmap Phase 1/RD8 explicitly. | proposed |
| 0019-D14 | Recommend keeping daily images unpinned. Separately, Cameron may choose RD8's daily 1280x800 pin, then 1920x1080 in 0027a and removal in 0069. That changes confirmed 0015a. Perf's requested 1920x1080 is already confirmed. | proposed |

## Interfaces

### Configuration and output

`mirror.perf=` accepts comma-separated `boot`, `1`, `mem`, `burn`, `flip`, `telemetry`, `agent`. Split command-line tokens on ASCII whitespace; matching is case-sensitive. Exactly one perf token, at most 64 bytes including its prefix, no empty/duplicate items or non-ASCII bytes. `burn` requires `1`; `mem,1` is redundant and invalid. Every valid combination enables milestones. `1` enables memory plus finite input capture; `mem` enables memory alone. Reserved `flip`, `telemetry`, `agent` each emit one SKIPPED with `reason=unavailable`, never successful measurement records.

Initialize configuration immediately after disabling interrupts in `_start`, without allocation, printing, TSC reads or `command_line()`'s panic on a missing Limine response. Missing command line means off. After serial initialization/banner, emit one `MIRROR_PERF_ON v=1 mem=<0|1> input=<0|1> burn=<0|1>` for requested configuration, or one `MIRROR_PERF_OFF reason=invalid`. SKIPPED later identifies unavailable measurements. Smoke skips perf arguments only when the complete configuration is valid; otherwise its existing unknown-command-line panic remains. Other boot-policy/fault parsing stays strict.

Names/field order below are exact. Timestamps are decimal u64 nanoseconds or `none`; counters are decimal u64. INPUT, MEM, WINDOW and BURN carry only numbers/`none`, never user data. `MIRROR_PERF_SKIPPED item=<input|mem|burn|flip|telemetry|agent> reason=<unavailable|smoke|no_clock|agent_demo|create_failed>` uses compiled identifiers. Perf state, snapshots and emitters are BSP-only with IF clear; no AP hook.

### Clock and milestones

`MIRROR_PERF_T event=<clock|display|input|userspace|frame|shell_ready> ns=<n|none>` occurs at most once each, in order. Only `clock` adds `since_entry_ns=none`. Its actual post-initialization snapshot is not zero: HPET resets before publication. Without HPET, timestamps are `none`; requested input, memory periodic sampling and burn emit SKIPPED `no_clock`. Skip precedence is smoke, then burn's AgentDemo exclusion, then clock absence. No alternative clock/recovery policy is introduced. Saturated timestamps or counter overflow invalidate measurements.

`display` follows FRAMEBUFFER_READY; `input` follows PS/2/vmmouse setup and IRQ enabling. `userspace` precedes first resume of the chosen interactive shell/agent-demo desktop, excluding smoke init. `frame` is its first positive physical-surface present: **one tile**, not a completed frame. Offscreen/failed/empty presents never qualify. `shell_ready` is its first physical Input receive/poll after `frame` with valid capability, writable buffer and exact length, including EAGAIN. Current clients paint completely before polling; this source invariant is not an ABI completion proof. Missing display/session omits its milestones.

Boot-smoke has no persistent interactive path: shell mode 3 enters non-returning `smoke_test` (`shell.rs:1021,1053`). It emits only clock/display/input milestones and SKIPPED smoke for requested input/mem/burn. Do not change either smoke battery to manufacture normal readiness.

### Memory

`MIRROR_PERF_MEM ns= available_kib= pool_kib=` uses four times recycled-plus-never-allocated frames and four times total allocator frames respectively; unavailable stats produce `none`. Neither quantity means all configured RAM. Sample through the scheduler's allocator borrow at interactive `userspace`, then on the first scheduler pass at least 10 HPET seconds after the previous sample; no catch-up burst. Without HPET emit only the initial sample with `ns=none`, plus the periodic-sampling skip. Delivered ticks are not wall time. Standalone `mem` supplies continuous idle observations. 0018 still aligns minute 10–12 to measured boot end, not HPET zero.

### Finite input capture

Capture exists only for `1`, HPET and the interactive physical Input/surface owner. The absolute deadline is shell_ready plus 120 seconds. Before the first key press, setup clicks are ignored, allowing the host to open Notes. The first key press whose first byte is at or after shell_ready starts collection; with burn, that timestamp must also be at or after worker dispatch. WINDOW start is that key's first-byte timestamp. Exclude other partial events whose first byte predates arming, even if they complete later; still update pointer baseline. Mouse measurements use a known arming key first. No later rearming occurs this boot.

Reserve at most 256 fixed records, each holding sequence, origin, start timestamp and optional present timestamp. Record the first 200 key presses and intervening button edges; key releases and motion alone do not start records. Autorepeat makes count as presses. Origin values are 1=keyboard IRQ, 2=relative-pointer IRQ, 3=vmmouse DATA, 4=polled keyboard, 5=polled relative pointer. For PS/2, carry the first-byte observation through prefix/packet decoding; source routing, not the delivered IRQ vector, identifies the device. A polled first byte gets its actual drain timestamp, never a fabricated IRQ timestamp. Vmmouse timestamps each DATA read that yields an edge, including deferred drains; a doorbell is not the click. Clear attribution and button-history validity on device loss; the first fresh pointer state establishes a baseline without inventing an edge.

Only completed accepted makes qualify; ACKs, errors, unknown sequences and breaks do not. Report distributions separately by origin, never pooling IRQ, polled and DATA-read timing.

Each qualifying event gets its own record. Carry its bounded internal identity/timestamp through decoder queues and Input Session pending/retry; do not change the 32-byte user event. Arm completion only after successful copyout **and consumption** by the selected physical Input owner (`userspace.rs:2607-2624`). A positive physical present pairs all delivered, unpaired records with that completion snapshot, never records still queued for delivery. Increment `overlap` for every start while another record is pending. Cancel sidecars on queue loss, session discard, fallback or exit; coalescing must preserve an unchanged edge's identity or cancel it explicitly. Never substitute a newer event's sequence. Cancellation leaves an unpaired record and increments loss. Sequences are 1–256, never wrap.

The first tile after delivery can still be unrelated to the intended response. This diagnostic proves neither completed paint nor visible causality and cannot pass G7/G11/0018 response gates. Those require the host changed-frame/probe oracle or future 0027 correlation.

Stop admitting keys after 200. Seal when all 200 have paired, at the absolute deadline, on session exit, or when another qualifying event would exceed capacity. Producers reject new input records, pairings and buffered memory samples at or after the deadline. Deadline checks run at every interactive scheduler return/idle pass; final sealing/draining happens on the first such pass, not promised as a hard real-time bound. A stopped guest or fatal panic yields an incomplete run, never fabricated completion. Sealing marks pending records unpaired and prevents any further collection. Device queue loss increments `loss` and invalidates the run; buffer exhaustion increments `overflow` and seals. Do not silently discard/coalesce trace records.

Before arming, emit the initial and periodic MEM samples normally. Buffer memory only from the first arming key until sealing, never during the potentially slow userspace-to-shell_ready startup. While collecting, perf sites allocate nothing and print nothing. Memory sampling continues into a separate 16-record buffer (at most 13 samples during the at-most-120-second collection). Buffer exhaustion invalidates the run. After sealing, drain at most four records per scheduler pass, outside IRQ/decoder/present handling; do not require idle. Records are immutable during draining. Emit:

- `MIRROR_PERF_INPUT seq= origin= start_ns= present_ns=`; unpaired presents are `none`.
- Buffered MEM records in sample order.
- `MIRROR_PERF_WINDOW start_ns= end_ns= keys= buttons= records= paired= overlap= loss= overflow= unpaired= reason=` after buffered lines; reason is 1=quota, 2=deadline, 3=session exit, 4=capacity. Start is `none` without an arming key.

Drain before normal session teardown; later input cannot mutate records. Memory then emits normally. Diagnostic runs use default flavor with no lifecycle transitions during collection; existing lifecycle/trace output is not suppressed. The host requires exact sequences 1..records, records=keys+buttons=paired+unpaired, matching origin counts and exactly one terminal WINDOW. Sequences follow completed-event admission; first-byte INPUT starts may go backwards across devices. Validate each INPUT as WINDOW start <= start <= present <= WINDOW end (omit present for unpaired records), nondecreasing MEM timestamps and WINDOW start <= end when armed. An unarmed WINDOW has start=none and zero keys/buttons/records. Strict 200-key diagnostics require 200 paired keys, zero overlap/loss/overflow/unpaired and quota completion. Requested burn additionally requires workers=completed=1 and every INPUT start and paired present timestamp inside its dispatch-to-exit interval; startup MEM samples may precede that interval. Missing/malformed lines invalidate the run: serial's bounded polls can drop bytes. Click runs report incomplete quotas honestly.

### BSP burn

Shell supports burn in default/trace after initial painting. AgentDemo skips burn; input/memory still work. Private startup flag `0x100` is valid only with base mode 2; reject other combinations/unknown bits. No syscall layout, module, capacity or policy change. Both images contain identical code.

The shell allocates 16 KiB writable stack between two unmapped 4 KiB guards. Its main thread's first successful THREAD_CREATE designates the worker TID in opted-in mode; instrumentation follows dispatch, exit and join without restricting other processes. The worker performs bounded arithmetic batches, reading SYS_CLOCK_GET=35 between batches. It exits after 120 seconds from its first clock read, or the shell's atomic stop request; no kernel seal notification. It publishes an atomic done flag before THREAD_EXIT; the shell checks each loop, joins, then releases the full guarded reservation. The join wrapper checks RAX for syscall success and RDX for worker status, including the blocked-join return; the existing generic RAX-only wrapper is insufficient.

Normal shell exit must request stop, join/reap the designated TID and release its reservation **before SYS_EXIT**, replacing the immediate exit path at `shell.rs:1034-1035`. Clock errors exit unsuccessfully. Forced/fault process destruction cancels threads and reclaims the aborted session; it never counts as a successful join or completed measurement. Creation failure rolls back, emits SKIPPED create_failed and invalidates burn measurement.

`MIRROR_PERF_BURN start_ns= end_ns= workers= completed=` records actual first dispatch and exit, or `none` if absent; workers=1 if created, completed=1 only after zero-status exit/join. Defer during capture. Both BURN and WINDOW are required; absent exit/join is incomplete.

It shares the shell's 32-of-100-tick budget and two-tick quanta. It measures contention/throttling within that process, not CPU saturation or an independent background service. Existing throttling can still create idle periods. QEMU and VMware results here are reports, not the proposed p99<30ms gate. 0051 must extend functional burn to every online userspace CPU; the end-of-Phase-1 gate follows that work and scheduler policy changes. Kernel AP loops remain untouched.

### Delivery and handoffs

`run.ps1 -Perf <items>` and `capture.ps1 -Perf <items>` validate grammar and use SMBIOS. `package.ps1 -Perf <items>` privately copies staging under `build/dist/perf`, changes Limine configuration, requests 1920x1080, verifies the image and hashes kernel/modules against daily staging. Add `mirror-image disk --perf` (once, no value, disk-only) selecting CID `50455246`; default remains `4d495252`. Check the full descriptor schema/CID deterministically. Record actual GOP dimensions; fallback modes do not satisfy resolution-specific runs. Include this tools workspace's fmt/clippy/host tests.

Public malformed `-Perf` fails before build/boot. For guest rejection coverage, `run.ps1 -Smoke -Fault PerfInvalid` injects raw `mirror.perf=bogus` through SMBIOS; require `-Smoke` and reject combination with `-Perf`. This host-only injection adds no kernel SmokeMode case: strict parsing must still panic. Update run/common/test assertions for one OFF, exact `unknown smoke-test command line` panic and exit 35, and reject this scenario's evidence elsewhere.

After confirmation, update 0018's always-on milestones, input schema/proxy limits and deferred flip dependency; retire superseded 0019 text without changing confirmed D1/D2. Claude's roadmap handoff must amend Phase 1 item 1/exit and RD8 for staged burn; ownership is a requested coordination handoff, not approval. Update proposed 0030a to migrate finite collection/sealing/draining and memory hooks, replacing its idle-only migration promise. Update README, ledger Pending, scripts and AGENTS with the confirmed contract. D14's daily-pin amendment is separate.

## Source map

Baseline is immutable `58461e0`: `main.rs:52,132,167,196,363,382,399` show entry, guarded-stack continuation, HPET and smoke/interactive ordering; `clock.rs:244-274,314-334` show reset-before-publication and BSP/IF-clear snapshots. `userspace.rs:375-386,1969-1978,2066-2107,3371-3404,3441-3485` show thread rotation, shell role, scheduling, thread operations and user-tick preemption. `userspace.rs:2796-2807` separates offscreen from physical presents; `desktop/src/shell.rs:1028-1048` paints before polling. `smp.rs:568-578` keeps APs idle. Paths are under kernel/src or userspace as appropriate.

Dependency evidence, not merged-source claims: 0017c at `7ca4070`, `kernel/src/ps2.rs:779-805`, `mouse.rs:298-330`, `vmmouse.rs:219-257` supplies bounded/deferred DATA drains and loss/coalescing. Proposed 0030a at `94461d0`, spec lines 10,13,33,55, stays BSP-only and promises perf continuity. Rebase hook locations after confirmed Phase 0 work lands.

## Acceptance tests

1. Host tests prove exact grammar, closed schemas, numeric-only data, clock absence, overlap, loss, capacity, deadline/session sealing, immutable drain order and terminal accounting. Synthetic first-byte/prefix and vmmouse deferred-drain cases prove attribution without real input contents. Interleave keyboard completion with a partial pointer packet: preserve truthful start-time reordering, exclude pre-arming partial events and retain the pointer baseline.
2. Smoke proves operational skips, init exclusion and fault composition; synthetic helpers cover burn rollback, successful/failed and blocked join status, exit-before-join prevention, forced-abort rejection and clock absence without a 120-second workload or battery changes. PerfInvalid proves guest OFF/panic; malformed public `-Perf` proves pre-build rejection. Fixtures never run in default.
3. Debug/release default/trace captures prove milestone order, physical-only endpoints, 200 paced presses with/without burn, deliberate overlap/loss rejection, memory spacing and deferred output. Perf cases use an explicit 240-second outer timeout, including worker exit/join/drain; timeout fails incomplete. Ordinary capture budgets stay unchanged. Tokenless default hygiene requires zero perf lines; AgentDemo proves burn skip.
4. Run targeted checks and both full suites required by AGENTS before implementation completion. VMware acceptance records actual resolution, profile, commit, monitor mode and raw records; remains pending until separately authorized. No other-OS acceptance is required here.

## Dependencies and open questions

No new external crate, tool or download. Confirm D3-D7/D11-D14, the 120-second/200-key/256-record bounds, the proxy endpoint and revised input origins, reserved flip owner, and the roadmap/0030a handoffs before implementation. Choosing an alternative requires updating this contract and affected confirmed entries first.
