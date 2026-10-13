# 0054a: Frame allocator v2 — refcounts, per-process limits

Status: implemented, on `main` @4c05b02 (2026-10-12, kimi), revised after claude's reviews of 8762d0e, 95eafa1 and c5a39eb. C8 was settled on 2026-10-12 without the split: S18 stands, and 0054a carries the reserve and the OOM order (0054-D4 to D6 in the superseded `draft-reserve-oom-order.md`), each open question's recommendation adopted.
Depends on: 0030a (the process table and its per-entry state, 0030-D1), 0050 (the shared frame-check rule, 0050-D2) and 0059 (the charged-frames counter, 0059-D3). Ordered fifth in Phase 1 (roadmap-replan.md:124), before 0051.
Ledger: S18 (the class model; the reserve and OOM order are proposed for the follow-up spec, amending RD6 and S18), S6 as amended 2026-10-12 (a vm-protocol limit change; no syscall), I6 as amended 2026-10-12 (multi-GB per-process memory lands in Phase 1 with this spec — as the mechanism, see Non-goals and open question 4). Related: S4, RD6.

Line references are at origin/main 8e43e55 (unchanged from 838c3b6 in the cited files).

## Goal
The frame allocator gains per-frame reference counts, so sharing (0025's shared memory, the ELF loader v2's shared text) has a safe foundation. Per-process memory limits stop being one pair of compile-time constants (vm-protocol's 32 MiB / 256 MiB, crates/vm-protocol/src/lib.rs:17-18) and become per-process state carried on the process entry (0030-D1) and set from its class at creation. In this spec every class keeps today's values (256 MiB reserved, 32 MiB committed); the follow-up spec lifts them together with the reserve (open question 1). The S18 reserve and the OOM victim order are proposed for the follow-up spec (open question 4).

## Non-goals
- The reserve floor, victim selection, the lifted limit values and the OomOrder fault case: the follow-up spec (pending Cameron's C8; RD6 is confirmed and places them in 0054a, so the split proposes amending RD6 and S18).
- Pressure notifications (0054b, Phase 11).
- Userspace on APs and the allocator's locking (0051): allocation stays BSP-only with IF=0 (memory.rs:210-215, :313-314).
- Grant-carried per-agent limits (0022, Phase 2). I6's multi-GB lands here as the mechanism — per-process limits with no compile-time cap, on the 60 GiB arena — and the follow-up's lifted values and reservation check make it observable; no Phase 1 process needs multi-GB defaults (open question 4).
- Charged kernel frames (0059-D3's page tables and surfaces, 0050's stacks) count toward no limit in Phase 1 (open question 3 for the follow-up's victim choice).
- A free list or faster scans: the 2-bit bitmap and its scans stay (memory.rs:55-86).
- The vm-protocol ARENA bounds and MAX_REGIONS (64, after 0030a).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0054-D1 | Per-frame refcounts in a companion u16 array (2 bytes per frame), in its own span reserved once at boot as METADATA beside the 2-bit state bitmap through the mechanism `enable_recycling` already uses (memory.rs:249-263; about 96 KiB at a 256 MiB guest and 10 MiB at 20 GiB). `share(frame) -> Result<(), FrameError>` increments an ALLOCATED frame's count, failing with a new `ShareLimit` variant when the count is 65535 — impossible under 0030a's capacities (64 processes x 64 regions is 4,096 mappings) — and leaving the count unchanged; METADATA or free frames are rejected. `deallocate` decrements: a frame is recycled only when its count reaches zero (memory.rs:318-340's state rules otherwise unchanged), and a fresh allocation starts at 1. `enable_recycling` sets count 1 for every frame it marks ALLOCATED (memory.rs:294-296), so the boot test frames still free (main.rs:112-121). `deallocate_contiguous` requires count 1 on every page of a run, checked in its existing validation pass (memory.rs:349-386); the contiguous self-test builds a private count array (memory.rs:566-578). Sharing adds no stale-free detection: memory.rs:317's "cannot detect a stale free after reuse" stays true, and sharers track their own references (0025, the loader v2). Every current allocation site keeps count 1; sharing arrives with those specs. Rationale: the array is cheap, reserved once, and needs no locking beyond today's BSP+IF=0 rule. Rejected alternative: u8 (overflows at 256 sharers, reachable under 0030a's 64x64 once 0025 lands). | proposed |
| 0054-D2 | Per-process memory limits from a class table. The class is a `create_process` parameter (userspace.rs:468), set before the create's own allocations; 0030a's seeding and launch pass it (its create calls at :625, :627, :1193, :1202, :1432 and :1970). The classes: protected (shell, desktop), normal (apps), agent, background (its only Phase 1 occupant is the follow-up's smoke-given OomOrder process, C10) and the test class (init's probes). In this spec every class keeps today's values (256 MiB reserved, 32 MiB committed), so init's battery passes unchanged (init main.rs:170-171, :395-404) and no app can outgrow today's cap before the reserve exists (RD6's risk: a runaway app starving the compositor, roadmap-replan.md:521). CpuPolicy is unchanged; 0052 keeps each process's memory class when it replaces the policy table. vm-protocol's RESERVED_LIMIT/COMMITTED_LIMIT constants stay the values every class reports now; SYS_VM_STATS reports the reading process's own values (userspace.rs:3316-3336; the ABI shape is unchanged, and this is S6's vm-protocol-limit scope). Enforcement applies the process's own limits at reserve and commit (vm.rs:190-193, :411-413); the one-span bound at vm.rs:398 stays on a fixed MAX_MATERIALIZE_BYTES equal to today's 256 MiB, because the rollback bitmap is a fixed 8 KiB stack array (vm.rs:12, :401), and resident work stays non-preemptible (vm.rs:3-4), which the follow-up's larger limits make longer. Rationale: the mechanism lands where the phase needs it, and the values change only when the reserve exists to bound them. Rejected alternative: lift the values here (an app could then take the whole 256 MiB smoke guest before any reserve or victim order exists). | proposed |
| 0054-D3 | Markers and scripts for this half only. `MIRROR_FRAME_POOL` stays where it is (main.rs:186-193; 0019's sampler cites its position) and gains `refcount_meta=<bytes>`; common.ps1 gains a closed-world check that recomputes `refcount_meta` from `total`, and `metadata` and `free` from `total` and `allocated` (the array has its own span, D1). The follow-up spec owns `MIRROR_MEM_RESERVE`, `MIRROR_OOM_KILL` and the OomOrder script entries. Rationale: each marker and its script checks land in the commit that adds the behavior (AGENTS.md's marker rules), which the split keeps straight. | proposed |

## Interfaces
- `kernel/src/memory.rs`: the refcount array in its own METADATA span (init in `enable_recycling`; `allocate`/`deallocate`/`allocate_contiguous_dma32`/`deallocate_contiguous` maintain it), `share`, and the `ShareLimit` error.
- `kernel/src/vm.rs`: the per-process limits at :190-193 and :411-413; :398 on MAX_MATERIALIZE_BYTES; the doc bound at :3-4 reworded.
- `kernel/src/userspace.rs`: `create_process` (:468) takes the class; 0030a's seeding and launch pass it.
- `kernel/src/main.rs`: `MIRROR_FRAME_POOL` gains its field in place.
- `crates/vm-protocol/src/lib.rs`: the two constants are documented as the values every class reports in this spec; the encode/decode is unchanged. Host tests follow.
- `userspace/init`: unchanged (the battery keeps the test class's constants).
- Scripts: common.ps1's FRAME_POOL check. No run.ps1, test.ps1, capture.ps1 or boot.rs change in this half.
- readme.md: the pool paragraph (:317), the quota paragraphs (:325, :327, :333) and the fault-behavior line (:547), in the same commit.

Work order (one commit per step, each running its targeted cases, P8):
1. The refcount array, `share`/`ShareLimit`, the prefix counts and the contiguous count-1 rule.
2. The class table, per-process limits at today's values, and the FRAME_POOL field with its script check.
Then the full suite.

## Acceptance tests
1. The existing battery and every frame-baseline check pass unchanged: all current allocations hold count 1, and the refcount metadata is METADATA, not allocated_frames, so no session delta exists (0050-D2's shared rule sees none).
2. Refcount self-tests (boot-smoke, beside memory.rs's existing tests): share then free both references in either order, recycling exactly at zero; the `share` call that would take a count to 65536 fails with ShareLimit and leaves the count unchanged; a contiguous run with any count above 1 is rejected before any change; `enable_recycling`'s prefix frames hold count 1 (the main.rs:112-121 checks).
3. Class plumbing: each seeded process reports today's constants through SYS_VM_STATS from its own entry, and init's battery passes unchanged. Differentiation is the follow-up's (its lifted values and tests).
4. Before merge (P8): the full `scripts/test.ps1` in debug and release, plus `-AgentDemo` while the agent-demo feature exists (RS6, confirmed 2026-10-12). The marker change lands in the same commit.

## Dependencies
No new crates.

## Open questions (for Cameron, with the reviewer's recommendations)
1. The class limit table (C3): values take effect with the follow-up — normal, agent and background at 1 GiB reserved / 512 MiB committed, protected bounded by the arena and free memory, the test class at 256/32 MiB; 0022's grants raise them later. In this spec every class keeps today's values. Main alternative: scale normal's limits with usable RAM now (rejected: RD6's starvation risk).
2. Refcount width (C4). Recommended: u16 (adopted in D1). Alternative: u8.
3. What counts toward the follow-up's victim choice in Phase 1 (C7). Recommended: owned pages plus the 0059-D3 charge (page tables plus the kernel-frames counter), each counted once; limits count only the VM ledger.
4. I6's Phase 1 meaning (C14). Recommended: the mechanism — per-process limits with no compile-time cap, on the 60 GiB arena; the follow-up proves it (one process reserves past 4 GiB, taking no frames, and SYS_VM_STATS reports it). Alternative: multi-GB defaults for the background class in Phase 1.
5. The split (C8), proposed here: it amends RD6 and S18. Recommended (per the review): yes — this half keeps every class at today's values, and the follow-up lifts them together with the reserve and owns the 1024 MiB case. After a yes, claude edits S18's text, RD6's row (roadmap-replan.md:521) and the Phases note (decisions.md:187), roadmap-replan.md:124, :420 and :486, README.md's map line for 0054a, and 0059:5/:15 (which place the reserve and OOM order in 0054a). Alternative: one 0054a, as RD6 and S18 say.
