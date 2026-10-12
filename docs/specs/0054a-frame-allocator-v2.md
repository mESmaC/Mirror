# 0054a: Frame allocator v2 — refcounts, per-process limits

Status: draft (revised after claude's reviews of 8762d0e and 95eafa1; the reserve and OOM-order half moves to a follow-up spec, pending Cameron's C8/RD24 answer — open question 4)
Depends on: 0030a (the process table and its per-entry state, 0030-D1). Ordered fifth in Phase 1 (roadmap-replan.md:124), before 0051. RD6 (confirmed 2026-10-12) places the S18 reserve and OOM order in 0054a, so this split proposes amending RD6 and S18 until Cameron decides C8 (open question 4); they live in the follow-up spec.
Ledger: S18 (partially — the reserve and the OOM order are the follow-up spec's; class-based limits start its class model), S6 as amended 2026-10-11 (a vm-protocol limit change; no syscall). Related: I6 (multi-GB per-process memory), S4, RD6.

Line references are at origin/main 838c3b6.

## Goal
The frame allocator gains per-frame reference counts, so sharing (0025's shared memory, the ELF loader v2's shared text) has a safe foundation. Per-process memory limits stop being the compile-time constants every process shares (vm-protocol's 32 MiB / 256 MiB, crates/vm-protocol/src/lib.rs:17-18) and become per-process state, set from the process's class at creation — with normal lifted past 32 MiB, since the 32 MiB commit is one of the foundations this phase lifts (roadmap-replan.md:58) and this phase's exits commit 256 MiB in one process. The S18 reserve and the OOM victim order are the follow-up spec (open question 8).

## Non-goals
- The reserve floor, victim selection, and the OomOrder fault case (the follow-up spec; RD6 is confirmed with it).
- Pressure notifications and cache-shrink requests (0054b, Phase 11; S18's engine/indexer behavior waits for it).
- Userspace on APs and the allocator's locking (0051): allocation stays BSP-only with IF=0 (memory.rs:210-215, :313-314). 0051 inherits the obligations the follow-up spec adds to.
- Grant-carried per-agent limits (0022, Phase 2; S18's "grants can carry per-agent memory limits"); limits here come from the class only, and 0022's grants raise them later — that is the path to I6's multi-GB.
- Charged kernel frames (0059-D3's page tables and surfaces, 0050's stacks) count toward neither limits nor (in the follow-up spec) victim choice in Phase 1 unless Cameron decides otherwise (open question 7).
- A free list or faster scans: the 2-bit bitmap and its scans stay (memory.rs:55-86).
- The vm-protocol ARENA bounds and MAX_REGIONS (64, after 0030a).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0054-D1 | Per-frame refcounts in a companion u16 array (2 bytes per frame, reserved once at boot as METADATA beside the 2-bit state bitmap through the mechanism `enable_recycling` already uses, memory.rs:249-263; about 96 KiB at a 256 MiB guest and 10 MiB at 20 GiB). `share(frame) -> Result<(), FrameError>` increments an ALLOCATED frame's count, failing with a new `ShareLimit` variant when the count is 65535 — impossible under 0030a's capacities (64 processes x 64 regions is 4,096 mappings) — and leaving the count unchanged; METADATA or free frames are rejected. `deallocate` decrements: a frame is recycled only when its count reaches zero (memory.rs:318-340's state rules otherwise unchanged), and a fresh allocation starts at 1. `enable_recycling` sets count 1 for every frame it marks ALLOCATED (memory.rs:294-296), so the boot test frames still free (main.rs:112-121). `deallocate_contiguous` requires count 1 on every page of a run, checked in its existing validation pass (memory.rs:349-386); the contiguous self-test builds a private count array (memory.rs:566-578). Sharing adds no stale-free detection: memory.rs:317's "cannot detect a stale free after reuse" stays true, and sharers track their own references (0025, the loader v2). Every current allocation site keeps count 1; sharing arrives with those specs. Rationale: the array is cheap, reserved once, and needs no locking beyond today's BSP+IF=0 rule. Rejected alternative: u8 (overflows at 256 sharers, reachable under 0030a's 64x64 once 0025 lands). | proposed |
| 0054-D2 | Per-process memory limits from a class table. The class is a `create_process` parameter, set before the create's own allocations (0030-D4 reserves the slots first, so the class lands before any frame moves). The table (values per open question 3): protected (the shell and desktop) is bounded only by the 60 GiB arena and free memory; normal (apps) is lifted past 32 MiB — proposed 1 GiB reserved / 512 MiB committed, which the 256 MiB commit case exercises; agent matches normal until grants land (Phase 2); background (no CpuPolicy maps to it yet — its only Phase 1 occupant is the follow-up spec's smoke-given OomOrder process, C10) matches normal in this spec; and the test class (init's probes) keeps today's constants (256 MiB reserved, 32 MiB committed), so the battery's equality and boundary checks (init main.rs:170-171, :395-404) pass unchanged. CpuPolicy is unchanged; the class no longer derives from it, and 0052 must keep a CpuPolicy-to-class mapping when it replaces the policy table. vm-protocol's RESERVED_LIMIT/COMMITTED_LIMIT constants stay as the test class's values and the documented defaults the table starts from; SYS_VM_STATS reports the reading process's own values (userspace.rs:3316-3336; the ABI shape is unchanged, and this is S6's vm-protocol-limit scope). Enforcement applies the process's own limits at reserve and commit (vm.rs:190-193, :411-413); the one-span bound at vm.rs:398 stays on a fixed MAX_MATERIALIZE_BYTES equal to today's 256 MiB, because the rollback bitmap is a fixed 8 KiB stack array (vm.rs:12, :401), and resident work stays non-preemptible (vm.rs:3-4), which larger limits make longer. Rejected alternative: keep the global constants (a runaway app gets the shell's headroom, and I6's multi-GB process never fits). | proposed |
| 0054-D3 | Markers and scripts for this half only. `MIRROR_FRAME_POOL` stays where it is (main.rs:186-193; 0019's sampler cites its position) and gains `refcount_meta=<bytes>`; common.ps1 gains a closed-world check that recomputes `refcount_meta` and `free` from `total` and `allocated`. The follow-up spec owns `MIRROR_MEM_RESERVE`, `MIRROR_OOM_KILL` and the OomOrder script entries. Rationale: each marker and its script checks land in the commit that adds the behavior (AGENTS.md's marker rules), which the split keeps straight. | proposed |

## Interfaces
- `kernel/src/memory.rs`: the refcount array (init in `enable_recycling`; `allocate`/`deallocate`/`allocate_contiguous_dma32`/`deallocate_contiguous` maintain it), `share`, and the `ShareLimit` error.
- `kernel/src/vm.rs`: the per-process limits at :190-193 and :411-413; :398 on MAX_MATERIALIZE_BYTES; the doc bound at :3-4 reworded.
- `kernel/src/userspace.rs`: `create_process` takes the class (spawn sites pass it: :550's Probe maps to the test class; :1220-1221, :1481, :1979 pass theirs).
- `kernel/src/main.rs`: `MIRROR_FRAME_POOL` gains its field in place.
- `crates/vm-protocol/src/lib.rs`: the two constants are documented as the test class's values and the defaults the table starts from; the encode/decode is unchanged. Host tests follow.
- `userspace/init`: unchanged (the battery keeps the test class's constants).
- Scripts: common.ps1's FRAME_POOL check; test.ps1 adds the `-MemoryMiB 1024` commit case in both profiles (the Phase 1 exit, roadmap-replan.md:135).
- readme.md: the pool paragraph (:317), the quota paragraphs (:325, :327, :333) and the fault-behavior lines (:547), in the same commit.

Work order (one commit per step, each running its targeted cases, P8):
1. The refcount array, `share`/`ShareLimit`, the prefix counts and the contiguous count-1 rule.
2. The class table, per-process limits, the FRAME_POOL field and the `-MemoryMiB 1024` case.
Then the full suite.

## Acceptance tests
1. The existing battery and every frame-baseline check pass unchanged: all current allocations hold count 1, and the refcount metadata is METADATA, not allocated_frames, so no session delta exists (0050-D2's shared rule sees none).
2. Refcount self-tests (boot-smoke, beside memory.rs's existing tests): share then free both references in either order, recycling exactly at zero; the `share` call that would take a count to 65536 fails with ShareLimit and leaves the count unchanged; a contiguous run with any count above 1 is rejected before any change; `enable_recycling`'s prefix frames hold count 1 (the main.rs:112-121 checks).
3. Class limits: SYS_VM_STATS reports each class's own values; init's battery (test class) keeps the constants and passes unchanged; a normal-class check commits past 32 MiB and fails at its own bound.
4. The Phase 1 exit's commit case: a new test.ps1 case boots `-MemoryMiB 1024` and has one normal-class process commit 256 MiB (past the old constant), with frame counts restored afterwards (0050-D2's rule).
5. Before merge (P8): the full `scripts/test.ps1` in debug and release, plus `-AgentDemo` while the agent-demo feature exists (RS6). The marker changes land in the same commit.

## Dependencies
No new crates.

## Open questions (for Cameron, with the reviewer's recommendations)
1. The class limit table (C3). Recommended: fixed Phase 1 defaults — normal, agent and background equal (1 GiB reserved, 512 MiB committed); protected limited only by the arena and free memory; the test class keeps 256/32 MiB; 0022's grants raise limits later. Main alternative: scale normal's limits with usable RAM now.
2. Refcount width (C4). Recommended: u16 (adopted in D1). Alternative: u8.
3. Do charged kernel frames (page tables, surfaces, 0050's stacks) or image and stack pages count toward limits or victim choice in Phase 1 (C7)? Heap v2 and 0050 both defer this here. Recommended: limits count only the VM ledger; the follow-up spec's victim choice counts all of a process's frames.
4. The split itself (C8): this draft takes it — allocator, classes, limits and the 256 MiB case stay here; the reserve floor, victim order and OomOrder move to the follow-up spec, whose number Cameron assigns (RD24; 0054b is taken by pressure notifications, roadmap-replan.md:383). Both keep 0054-Dn IDs (rule 1). Because RD6 (confirmed 2026-10-12) places the reserve and OOM order in 0054a, the split proposes amending RD6 and S18 until Cameron decides. Alternative: one spec, built in five commits.
