# Reserve and OOM order (follow-up to 0054a; unnumbered pending RD24)

Status: draft (split from 0054a per the 0054a review's C8 recommendation; Cameron assigns the number per RD24 — 0054b is taken by pressure notifications, roadmap-replan.md:383 — and it keeps the family's 0054-Dn IDs, split rule 1)
Depends on: 0054a (the refcount allocator and the class model, 0054-D1/D2), 0030a (the process table, wait-queue unlinking, zombie/reap, 0030-D1/D2/D5), 0050 (its stack pool is trimmed before and during victim choice, 0050-D2) and 0059 (0059-D3's charged-frames counter ranks victims). RD6 (confirmed 2026-10-12) places the reserve and OOM order in 0054a, so this spec proposes amending RD6 and S18 until Cameron decides C8 (open question 8).
Ledger: S18 (the reserve and the OOM order), S6 as amended 2026-10-11 (no syscall). Related: S4, G7, I6, RD6.

Line references are at origin/main 838c3b6.

## Goal
The kernel gains S18's memory policy (decisions.md:140): a reserve that only the kernel and the protected classes may draw, and an out-of-memory victim order — background work, then agent tasks, then normal apps, never the compositor, shell or system services — that runs whenever a frame allocation fails for lack of frames. A failing request kills a lower-class victim and retries, instead of today's "the faulting process kills itself" (userspace.rs:1034-1039) as anything but the last resort.

## Non-goals
- Pressure notifications (0054b, Phase 11); the engine/indexer's shrink behavior.
- Refcounts and the per-process limit table (0054a). This spec uses 0054a's classes without changing them.
- Userspace on APs (0051): allocation stays BSP-only with IF=0. 0051 inherits three obligations: the reserve check must be atomic with the allocation; count updates run under the allocator's lock; and a victim running on another CPU must be stopped and its TLB entries shot down before teardown.
- Charged kernel frames and image/stack frames count toward victim choice per open question 7; no limit charges them (0054a).
- Any new syscall (S6, amended).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0054-D4 | The reserve is a free-frame floor, sized per open question 2 (recommended: the smaller of a fixed MiB and a fraction of usable frames), fixed once at one point identical in every flavor before the first session, and printed once as `MIRROR_MEM_RESERVE frames=<n>` (content-free). `FrameAllocator::allocate` stays the kernel's own call and may draw the floor; `allocate_for(class)` refuses to cross it for a non-protected class. Per site: every runtime frame consumer — materialize's page tables and data pages (vm.rs:398-478), create_process's image and stack, 0050's stack builds, and pixels (windows.rs:37-64) — reserves its operation's worst-case frame count against the floor before it starts (the 0059-D1 preflight pattern: one check per operation), so no partial operation straddles the floor; boot-time DMA is the kernel's call and never selects a victim (a contiguous DMA32 failure can be fragmentation, memory.rs:153-190, which no kill fixes); heap growth uses the kernel's call, and 0059-D1's free-frame preflight counts the floor as available to the kernel. The class is already set at create (0054-D2), so every runtime allocation has a requester class. Rationale: one preflight per operation keeps the floor's invariant without threading a class through paging.rs and windows.rs. Rejected alternative: a class-scoped allocator view through paging.rs, windows.rs and 0050's builder. | proposed |
| 0054-D5 | Victim selection runs whenever a frame allocation fails for lack of frames — at the floor or at zero, never only at global exhaustion. Only a failed frame allocation triggers it: frame exhaustion gets its own kernel-internal error (a `VmError::NoFrames` that becomes -12 only at the syscall boundary), so a process at its own commit limit (vm.rs:411-413's quota failure, unchanged) never triggers it, and launch keeps OutOfFrames distinct instead of mapping every error to -12 (userspace.rs:1440). Candidates: for a non-protected requester, only strictly lower kill classes (S18's order: background below agent below normal); for a protected or kernel requester, every non-protected class. Within a class: the largest frame footprint (owned pages plus owned tables plus charged kernel frames, per open question 7), then the newest PID. Before choosing, trim 0050's stack pool (under its prepare_kernel_update rule); trim again after each kill, before the retry (this amends 0050-D2's drains-only trim point). A kill finishes the victim at once in the completion context — Outcome::OutOfMemory (open question 6), threads unlinked (0030-D2), handles closed, its address space destroyed immediately (the victim is never running: the scheduler is BSP-only with IF=0 until 0051) — and its zombie keeps only the Outcome for validation; 0030-D5's reap skips the destroy for an OOM zombie (amended here). The failed allocation retries once per kill, bounded, stopping at success or when no candidate remains. record_fault's self-kill (userspace.rs:1034-1039) is the last resort: with no candidate left, a protected process's failed fault still kills it, the shell session then ends, its children end SessionEnded and default and trace kernels start the console (0030-D5; smoke kernels go on to the capacity self-test). Rejected alternative: kill the requester always (a background hog then outlives a normal app, against S18). | proposed |
| 0054-D6 | Markers and scripts for this half. `MIRROR_MEM_RESERVE frames=<n>` prints once per boot in every flavor and joins test.ps1's Default and Trace `$required` lists (test.ps1:36, :40), checked exactly once with its full field list in run.ps1 and capture.ps1, with the script recomputing frames= from MIRROR_FRAME_POOL's total= and the open-question-2 sizing rule. `MIRROR_OOM_KILL class=<class> pid=<n>` prints per victim kill; its flavor gating is open question 5 (recommended: every flavor, as a content-free lifecycle marker, S7b); run.ps1 asserts it only in the OomOrder case and rejects it in every other scenario. Outcome::OutOfMemory maps to `reason=oom` in MIRROR_APP_RECLAIMED and the shell reads it as FAULTED with detail 0 (the window-protocol state set, crates/window-protocol/src/lib.rs:29-31, is unchanged — S6's amendment allows no Phase 1 ABI change), counted in windows.faults (userspace.rs:1514-1516); the desktop/agent cleanup lines map it too (:1386-1405). Rationale: the OOM kill is a lifecycle event the owner must be able to see (S17's log follows in Phase 3), and the reserve is a boot-time contract the scripts can recompute. | proposed |

## Interfaces
- `kernel/src/memory.rs`: `allocate_for(class)`, the floor, and the preflight helper.
- `kernel/src/vm.rs`: `VmError::NoFrames` (becomes -12 only at the syscall boundary); the quota failure keeps its ENOMEM.
- `kernel/src/sched.rs` (0030a): victim selection, the kill/reap split and the bounded retry run in the scheduler's completion context.
- `kernel/src/userspace.rs`: the frame-failure sites (:1034-1039, :1058-1062, :1440 with the distinct error) route through selection; :1012 (reserve/release/protect) never does. `launch` keeps OutOfFrames distinct.
- `kernel/src/windows.rs`: a pixel-allocation failure routes like the create failure (it is a frame failure on the launch path).
- `kernel/src/boot.rs` and `main.rs`: the `oom-order` token and SmokeMode beside the existing modes; the reserve is fixed and printed before the first session.
- 0050's kernel_va.rs: the trim is callable from the completion context, before a victim choice and after each kill.
- `crates/window-protocol`: unchanged (FAULTED with detail 0).
- `userspace/init`: the OomOrder scenario's processes; validate_process excludes the scenario (userspace.rs:2214).
- Scripts: run.ps1 gains `-Fault OomOrder` (ValidateSet, `$testMode`), asserts OOM_KILL only there; test.ps1 adds the case in both profiles; common.ps1 gains the MEM_RESERVE recompute; capture.ps1's absent list follows open question 5.
- readme.md: the pool paragraph (:317) and the fault-behavior lines (:327, :547), in the same commit.

## Acceptance tests
1. The existing battery passes unchanged (the floor is not reached in any existing case, and no marker moves).
2. `-Fault OomOrder` (`mirror.test=oom-order`): the scenario replaces the relaunch pair's two probes with a normal requester and a background victim (two processes, the pair's end rule, 0030-D1), with the shell session after it holding the protected class. A kernel-wide frame budget shrinks only in this case and lifts before the session drains (so later heap growth and capacity_self_test are unaffected). Assert: the background process dies with Outcome::OutOfMemory, `MIRROR_OOM_KILL class=background` prints exactly here, the requester's retry succeeds after the kill, the reserve holds, and the protected and normal processes survive. The scenario is excluded from validate_process's battery checks. Marker counts it changes are updated in the same commit (the relaunch pair's MIRROR_USERSPACE_OK/sync/TLS lines, run.ps1:585-639; heap v2's per-drain CHARGE_OK; 0050's KSTACK_OK; 0030a's PREEMPT_OK).
3. Reserve: a boot-smoke self-test right after the floor is fixed drives allocations to it: a normal-class `allocate_for` fails there while the kernel's own `allocate` still succeeds, with frame counts restored.
4. The class-limit checks live here: the OomOrder session's processes include the test class and the normal class, whose limits differ (0054-D2); each fails at its own bound and not the other's.
5. Before merge (P8): the full `scripts/test.ps1` in debug and release, plus `-AgentDemo` while the agent-demo feature exists (RS6). The marker changes land in the same commit.

## Dependencies
No new crates.

## Open questions (for Cameron, with the reviewer's recommendations)
1. When a normal app or agent reaches the floor, does victim selection run before it fails (C1)? Recommended: yes — background first, then agent (0054-D5). Alternative: fail the requester at the floor, as today.
2. The reserve's size, and who may draw it (C2). Recommended: the smaller of a fixed MiB and a fraction of usable frames, as a free floor; the kernel and protected classes may draw it (0054-D4). Alternative: a fixed MiB floor only.
3. The class limit table (C3): settled in 0054a's open question 1 (recommended fixed Phase 1 defaults; 0022's grants raise them later).
4. Refcount width (C4): settled in 0054a (u16).
5. Does `MIRROR_OOM_KILL` print in every flavor as a content-free lifecycle marker (C5)? Recommended: yes (S7b); then it leaves the `$forbidden` image lists and run.ps1 rejects it by scenario. Alternative: boot-smoke only.
6. `Outcome::OutOfMemory` as a new variant with `reason=oom`, shown to the shell as FAULTED (C6)? Recommended: yes. Alternative: reuse Terminated, with only the marker to tell them apart.
7. What counts toward victim choice in Phase 1 (C7)? Recommended: all of a process's frames — owned pages (image and stack), owned tables and charged kernel frames. Limits stay the VM ledger's (0054a).
8. The split (C8): taken. This spec is the second half.
9. RD6 (the S18 reserve and OOM order land in this phase-1 spec) is due before Phase 1 and unconfirmed; confirm it with this spec. Alternative: leave the reserve and OOM order to 0054b in Phase 11.
10. The OOM case's background-class process is a boot-smoke-only class given at seeding (C10, recommended), not init's Probe — so the battery's normal-equivalent limits and kill semantics stay clean. Alternative: a background CpuPolicy ahead of 0052.
11. Does 0018-D6's idle memory keep counting the reserve as available (C12)? Recommended: yes, unchanged; reports also show MIRROR_MEM_RESERVE. Alternative: subtract the reserve, as Linux's MemAvailable does, and re-confirm 0018-D6.
