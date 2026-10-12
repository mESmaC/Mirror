# 0019d: A separate BSP burn process

Status: draft; every choice here is proposed. This document does not authorize implementation or VM runs.
Depends on: confirmed and implemented 0019a and 0019c; merged 0017c. Implement before 0030a.
Ledger: S7a/S7b, S4, S10, G7; proposed amendments to roadmap Phase 1 item 1 and its exit. RD8 and RD24 are confirmed.

## Goal

Measure input response while another userspace process competes for the BSP. Reuse the app ELF and current scheduler, without spending the shell's CPU allowance. Keep the workload alive through the host's final pixel observation. Report its actual lifetime and complete resource reclamation.

## Non-goals

CPU saturation, AP scheduling, a new syscall, a new module, telemetry, scheduler-policy changes, and user thread creation/join. The earlier in-shell worker, guarded worker allocation and join-wrapper proposal in 0019a@93aee7c is superseded if D13 below is confirmed. Kernel tile timings remain diagnostics; 0019c owns the visible-response oracle and its proposed Phase 1 gate.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| 0019-D13 | Recommend one private app-module process with its own existing Application policy, launched and stopped by the kernel as below. It costs one app slot but measures cross-process contention before and after 0030a. Stage per-online-CPU load and AP-safe instrumentation with 0051, then repeat after 0052. Amend roadmap Phase 1 item 1 and its exit, which currently describe a spinning thread inside an existing module on every CPU. RD8 settled the Phase 1/telemetry split and no daily pin; it does not decide this burn design. | proposed |

Alternatives are choices for Cameron, not additional implementations:

| Choice | Measurement and cost |
| --- | --- |
| Separate process, recommended | Own 32-of-100-tick allowance. Measures competition between shell, Notes and burn. 0030a changes dispatch/wakeup while retaining that policy. One of two app slots and its resources remain occupied. |
| Worker inside shell | Shares the shell's allowance; this is not an independent budget. The resulting throttle stalls remain after 0030a. The Phase 1 latency exit is blocked on 0052's policy change. Requires the previous guarded allocation, observable failure protocol and correct RAX/RDX join handling. |
| Give an in-shell worker its own budget | Introduces per-thread policy/accounting absent today. This is scheduler work, so defer to a separately confirmed 0052 design rather than hide it in instrumentation. |
| Kernel AP spin loops | Loads other CPUs but bypasses userspace scheduling. Requires a separate interrupt/rendezvous-safe AP design and AP-safe time/stop handling; it cannot stand in for the Phase 1 userspace load. |
| Move burn after 0051 | Simplifies the final workload but loses the pre-0030a loaded baseline. Keep 0019a/c first and explicitly revise phase order if chosen. |

## Interfaces

### Launch and workload

`burn` still requires `1`. Operational burn runs only in a normal default/trace shell with HPET. Use 0019a's skip precedence: smoke, agent_demo, no_clock, no_session, unavailable. These initial skips occur at 0019a's pre-session decision point, create no child and require no BURN record. Requested ON bits never change retroactively.

Add only the runtime skip `MIRROR_PERF_SKIPPED item=burn reason=create_failed`. After the one failed launch attempt and verified rollback, print it immediately, after shell_ready but before any arming or INPUT output. Seal the still-unarmed capture with abort reason 5; never admit an arming key after that failure. Its BURN reason=6 record follows WINDOW in the drain. This is a distinct runtime position in the checker, not a second pre-session skip. Every attempted launch requires exactly one terminal BURN record, even when creation fails.

At `shell_ready`, mark one launch pending. At the next `run_shell` scheduler boundary, before processing new application-launch requests, attempt it once. All active-user pointers must be clear and the kernel root active. First dispatch must occur before 0019c's setup deadline, `shell_ready + 60 s`; otherwise abort the capture and reclaim anything created. Do not evict an existing app to obtain a slot.

Add a private `ShellRuntime` launch variant. It uses the existing app image, resource transaction and child slot. Separate its entry argument from its window metadata: entry argument is exactly `0x100`; Application kind remains COUNTER (`1`). Ordinary APP_LAUNCH still accepts only public kinds 0/1 and never passes this argument. The app entry accepts `0x100` as a proposed private mode before normal kind validation; other values remain invalid.

The private path never paints, logs, reads input or launches threads. It repeats a bounded batch of 1,000,000 dependent scalar operations, using the existing busy-probe approach to prevent optimization away, then reads SYS_CLOCK_GET=35. It never voluntarily yields or sleeps. Clock failure or its independent 450-second deadline from the first clock read exits nonzero. Such an exit is a failed run, not successful completion.

Use the process's existing 64 KiB stack and guards. Its policy remains Application: two-tick quanta and 32 charged user ticks per 100 delivered ticks. This is runnable background load, not continuous execution or full CPU utilization.

Reuse the app resource transaction: two surfaces consume 120 pages/480 KiB, plus the process image, stack, page tables and the child's existing bootstrap handles. The private launch skips insertion of Object::Application(pid) into the shell's handle table entirely. Kernel bookkeeping stores only the PID and slot; there is no parent control handle or returned token. Public APP_* calls cannot resolve this child even if shell code guesses its PID or a capability number. Close's parent-table revoke is consequently a safe no-op for this PID. The hidden child occupies one of two app slots, leaving one for Notes, and draws no window. Its lifecycle markers remain content-free. A smaller surface-free resource path is outside this proposal.

Extend the Keys and Exit lifecycle oracles for burn: require exactly one hidden `MIRROR_APP_STARTED pid=B kind=1` before arming, distinct from the one Notes kind=0 PID. On normal host exit, stop it explicitly before the generic SessionEnded sweep: require hidden `MIRROR_APP_RECLAIMED pid=B reason=closed status=0 frames_reclaimed=1`, then `MIRROR_APP_CLOSED pid=B surfaces_reclaimed=1`, then Notes' SessionEnded RECLAIMED/CLOSED. The teardown then drains INPUT, buffered MEM, WINDOW and BURN, before successful shell cleanup and console readiness. Permit no other app launches. Terminated's status=0 is lifecycle encoding, not proof of graceful child exit; BURN supplies the verdict. 0019c rejects burn in click/overlap/deadline scenarios. Exit is the short early-normal-exit case and requires BURN reason=2, completed=1; it can never pass the 200-key gate.

### Lifetime and stopping

0019c exposes immutable seal `{armed, start, end, reason}`, `finish=<not_waiting|awaiting_exit|timely_exit|failed>` and `drain_complete`. Burn reads that shared contract and may request abort sealing; it cannot rewrite a frozen quota result. These are internal calls, not user ABI.

After quota seal, keep burn running and hold all perf output while finish=awaiting_exit. The host observes final expected pixels, then sends ordinary Escape. Normal session exit must occur within 30 seconds of seal. Stop burn before further child dispatch, reclaim its resources and the remaining session resources, then use 0019c's single teardown drain before MIRROR_SHELL_CLEANUP_OK. WINDOW end remains the proxy-seal time; BURN end is later. No new completion syscall or shared stop word is introduced.

Deadline/capacity/loss failure, faulted shell, unexpected child exit, missing post-seal Escape, or clock failure aborts the run. Request 0019c sealing before cleanup/output if necessary, stop at the next scheduler boundary and report failure. If the shell still runs, its four-line-per-pass drain emits the terminal BURN only after WINDOW and verified burn cleanup. If the shell exits, teardown flushes the remaining suffix once. A normal early Exit diagnostic may terminate the workload cleanly but cannot satisfy the strict 200-key verdict.

The kernel also checks a hard cap of first-dispatch plus 450 seconds. Dispatch is at or after shell_ready; setup is at most 60 seconds, collection 330, and post-seal host completion 30. Thus a valid run needs at most 420 seconds from dispatch. Check deadlines before redispatch; a stalled guest or failed timer is caught by the host timeout, not described as a hard real-time guarantee.

Stop by the existing kernel `close(..., Outcome::Terminated)`/reclaim path for the recorded PID. Snapshot end immediately before marking the stopped process finished, after its last possible user instruction and with active contexts cleared; do not date the end after cleanup. Before generic finished-child reclaim, preserve an unexpected exit's boundary, charged ticks and actual outcome in perf state. Its later close frees the remaining surfaces without fabricating a second RECLAIMED or replacing its real lifecycle reason with closed. Release child handles, address space, both surfaces and the slot; the parent-table revoke finds no private control handle. Verify the exact owned-frame delta while other processes are stopped, without comparing against a whole-boot baseline while Notes remains allocated.

There is no THREAD_JOIN in this design. Expected kernel cancellation after host completion is a valid workload stop, not a fabricated zero-status exit. Forced teardown/fault remains distinguishable. Generic finished-child reclamation must first notify perf, so an unexpected exit cannot disappear behind a later success record.

### Evidence and verdict

Emit exactly one terminal line for every attempted launch, after verified cleanup/rollback and after WINDOW in 0019c's ordered drain. Never emit it while host completion still requires quiet:

`MIRROR_PERF_BURN origin=1 created= start_ns= end_ns= charged_ticks= reclaimed= completed= reason=`

`origin=1` means a separate BSP userspace process timed by kernel HPET snapshots. It is unrelated to INPUT device-origin numbers. `created`, `reclaimed`, `completed` are 0/1. Timestamps are decimal u64 or `none`; other fields are decimal u64. Start is actual first dispatch, not allocation or launch request. Absent dispatch gives start/end `none`. Reclaimed=1 requires verified complete cleanup or verified rollback; created=0 can never complete.

Charged_ticks counts timer-driven user ticks charged to this PID between first dispatch and quiescence. Preserve that counter in perf state before reclaiming process policy state. It is load evidence, not wall time or a CPU-utilization estimate. The host additionally enforces 0019c's conservative final-key-down-to-cleanup deadline; it does not compare QPC directly with guest HPET.

Reasons: 1=quota plus normal host exit; 2=other normal shell exit; 3=capture failure; 4=dispatch timeout; 5=hard cap; 6=create failure; 7=unexpected child exit/fault. Completed=1 requires created=1, a real dispatch, complete reclaim and reason 1 or 2. Requested load that never starts produces an explicit failure, not an empty successful run. Missing terminal output, including panic or lost serial bytes, is incomplete.

Strict burn-plus-key acceptance requires reason=1, completed=reclaimed=created=1, positive charged ticks, 0019c's valid quota WINDOW, and every INPUT start/present inside the BURN interval. Host evidence must separately show final expected pixels before Escape; guest timestamps alone cannot prove that ordering across clocks. Initial MEM may precede burn. The checker recomputes relationships and rejects unknown fields, duplicate terminal lines and inconsistent failures.

### Later schedulers

0030a's spec is confirmed and merged; its implementation must preserve this private child's policy, identity, deadlines and cleanup while moving it into global tables. After D7/D13 confirmation, Claude updates `0030a-process-thread-table.md:34,56` at `8e43e55`: replace the idle-point D7 flush with 0019c's live and shell-teardown drains, carry the private child, and retain the before/after loaded checks. Keep the one-slot cost for that comparison. This does not add AP userspace or remove throttling. RS6 retires AgentDemo when 0030a is implemented; input and burn skip it until then.

0051 owns one runnable burn context per online userspace CPU, replacing the two-app-slot restriction using the new process/thread tables. It must also make perf stamps/state and clock access safe on AP execution paths, or explicitly retain a BSP-pinned measured owner while providing AP-safe worker timing. Simply removing `clock::snapshot`'s BSP assertion is insufficient. 0055a owns calibrated TSC source selection; no AP is allowed to call today's BSP-only snapshot before an approved replacement. 0052 owns policy changes; reports identify which policy actually ran. Claude's roadmap handoff must name these prerequisites and 0019c's host oracle before claiming the Phase 1 exit.

P9 implementation handoff: update AGENTS.md's Fault injection section for PerfBurn's kernel SmokeMode, fixture marker and placement. Update Test and When to test in the same commit for the extra smoke case in both suites and the targeted loaded Keys/Exit runs, their 600-second accepted capture cap and added budgets below. Coordinate the edit with Claude; this proposal leaves the hot file untouched.

## Acceptance tests

1. Add host lifecycle/schema tests to `mirror-perf-protocol`: success, every failure reason, creation without dispatch, exact deadline boundaries, quota quiet hold, missing Escape, early child exit, double cleanup and missing/contradictory terminal records. Require create_failed before arming, WINDOW before BURN, teardown drain before cleanup, and no repeated suffix after a partial live drain.
2. Add `run.ps1 -Smoke -Fault PerfBurn` to both debug/release test.ps1 profiles. New `mirror.test=perf-burn`/SmokeMode selects `userspace::burn_self_test` after main's heap growth, before the AgentDemo/shell branch. Ordinary smoke keeps its skip and no long workload. This boot-smoke-only helper owns a non-running fixture owner and local runtime using the app image. Test rollback after process load, child bootstrap-handle setup, each surface allocation and private PID/slot publication; dispatch the private child for one quantum before kernel termination; verify exact frame/child-handle/slot restoration and unchanged parent handle table. Guessed PID or token APP_* calls must fail without stopping burn. Inject unexpected-exit/fault outcomes at that scheduler boundary to verify their true lifecycle reason and failed BURN verdict. Synthetic clocks cover long deadlines; no extra app entry mode or 450-second wait is needed.
3. The fixture emits exactly `MIRROR_BURN_TESTS_OK rollback=1 dispatch=1 stop=1 reclaim=1 abort=1`. run/common assertions require it once only in PerfBurn, with all fields, and forbid it elsewhere. This is not a PERF measurement line. Both ordinary and AgentDemo full suites include the case; the helper must own its fixture runtime and not depend on selecting the normal shell path.
4. Targeted default `capture.ps1 -PerfCapture -Perf 1,burn -PerfScenario Keys -CpuCount 4 -TimeoutSeconds 480` runs 0019c's 200-key oracle in debug and release. It opens only Notes, waits per key for expected pixels plus 250 ms within the inclusive 1.5-second per-key budget, then Escape after final pixels. Maximum accepted capture timeout is 600 seconds. These two long cases run once per implementation change, not on every full-suite invocation; worst-case added budget is 16 minutes. Also run the burn Exit case in each profile with a 90-second cap: one visible key then Escape, zero loss/overflow, WINDOW reason=3 and BURN reason=2 with complete reclaim. These two targeted short runs add at most 3 minutes; they do not duplicate 0019c's unladen suite cases. AgentDemo and no-HPET skip evidence reuses 0019a's Markers cases, with no new child.
5. Run affected Rust/tool checks and both full suites before implementation completion. VMware repeats the host oracle after separate run authorization; record profile, commit, actual resolution, monitor mode and raw guest/host evidence. No other-OS benchmark is included.

## Dependencies

No external crate, tool or download. The app adds the existing in-tree `mirror-clock-protocol` dependency. 0019a owns configuration, packaging, -PerfCapture and -NoHpet; 0019c owns finite capture, its added scenarios, the quiet finish phase, both drain sites and host timing/oracle checks. This slice extends their schemas and assertions together. Runtime/code work remains blocked on confirmation.

## Source evidence

At immutable `3c87bcb`: `kernel/src/userspace.rs:1418-1486` implements app launch, own policy and rollback; `1489-1580` reclaims/closes it; `2037-2110` schedules shell and children; `3279-3283` rejects public kinds above 1. `userspace/app/src/main.rs:306-326` contains finite busy work and exact current kind validation. `kernel/src/windows.rs:15-17` accounts 120 surface pages. `userspace/desktop/src/shell.rs:1098` is the non-returning smoke path. The proposed private mode/launch function does not exist yet.

At baseline `58461e0`, `kernel/src/userspace.rs:35,43-44,214-219,3441-3485` establishes the unchanged budgets/preemption; `clock.rs:314-334` requires BSP/IF clear. Confirmed, merged `0030a-process-thread-table.md` at `8e43e55`, lines 10, 13 and 56, preserves those limits and requests before/after burn reports. Its lines 34 and 56 need the explicit handoff above once these proposed contracts are confirmed.

## Open questions

Confirm D13's separate-process choice, resource cost/private mode, 450-second cap, expected cancellation semantics, fixture placement and long-run test schedule. Confirm the slice/roadmap/AP handoffs with Cameron. Alternatives require a revised contract before implementation; none is silently selected by future code.
