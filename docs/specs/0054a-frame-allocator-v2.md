# 0054a: Frame allocator v2 — refcounts, per-process limits, the reserve and the OOM order

Status: draft
Depends on: 0030a (the process table, its per-entry policy class and its zombie/reap, 0030-D1/D5) and kernel heap v2 (its per-process kernel-frames counter, heapv2-D3). Ordered fifth in Phase 1 (roadmap-replan.md:124), before 0051.
Ledger: S18 (the reserve and the OOM order), S6 as amended 2026-10-11 (a vm-protocol limit change; no syscall). Related: I6 (multi-GB per-process memory), S4 (the background class), RD6.

Line references are at HEAD 58461e0 (unchanged from 970e48c in these files).

## Goal
The frame allocator gains per-frame reference counts, so sharing (0025's shared memory, the ELF loader v2's shared text) has a safe foundation and a stale or double free is caught. Per-process memory limits stop being the compile-time constants every process shares (vm-protocol's 32 MiB / 256 MiB, crates/vm-protocol/src/lib.rs:17-18) and become per-process state set at spawn from the process's class. And the kernel gains S18's memory policy: a fixed reserve that only protected classes may consume, and an out-of-memory victim order — background work, then agent tasks, then normal apps, never the compositor, shell or system services — replacing today's "the faulting process kills itself" (userspace.rs:1034-1039).

## Non-goals
- Pressure notifications and cache-shrink requests (0054b, Phase 11; S18's engine/indexer behavior waits for it).
- Userspace on APs and the allocator's locking (0051): allocation stays BSP-only with IF=0 (memory.rs:210-215, :313-314); the bitmap's read-modify-write stays single-CPU.
- Grant-carried per-agent limits (0022, Phase 2; S18's "grants can carry per-agent memory limits"); limits here come from the class only.
- Heap growth policy (heap v2) and per-process heap charging limits (heapv2-D3 adds the counters; this spec adds the limits).
- A free list or faster scans: the 2-bit bitmap and its scans stay (memory.rs:55-86).
- The vm-protocol ARENA bounds and MAX_REGIONS (64, after 0030a).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0054-D1 | Per-frame refcounts in a companion array: one byte per frame (saturating at 255; an increment past 255 fails closed), reserved once at boot as METADATA beside the 2-bit state bitmap through the same mechanism `enable_recycling` uses (memory.rs:249-263), sized from `total_frames` (64 KiB per 256 MiB of guest RAM). `deallocate` becomes a decrement: a frame is recycled only when its count reaches zero (memory.rs:318-340's state rules otherwise unchanged), and a fresh allocation starts at 1. The count detects today's silent case — a stale free after reuse — whenever the stale count differs from the owner's. Every current allocation site keeps count 1 (page tables, user image and stacks, demand-zero pages, heap backing, DMA, pixels: memory.rs's callers are unchanged in behavior); sharing arrives with 0025 and the loader v2. Rationale: the array is cheap, reserved once, and needs no locking beyond today's BSP+IF=0 rule. Rejected alternative: widen the state bitmap (all four 2-bit states are consumed, memory.rs:14-17) or key refcounts to owners (owner identity is the limits' job, D2). | proposed |
| 0054-D2 | Per-process memory limits. Each process entry (0030-D1) carries its OOM class and its reserved/committed limits, set at spawn: the class derives from its CpuPolicy (userspace.rs:214-220, set at :550, :1220-1221, :1480, :1978) — Desktop and Shell are protected, Application is normal, Agent is agent, and init's probes are background. vm-protocol's RESERVED_LIMIT/COMMITTED_LIMIT constants become the *normal* class's defaults; the ABI shape of SYS_VM_STATS is unchanged and now reports the reading process's own values (userspace.rs:3314-3334; this is S6's vm-protocol-limit scope). Enforcement is today's, per process: reserve and materialize check the process's own limits (vm.rs:190-193, :411-413). Limits are set at spawn and not adjusted at runtime in this spec (no syscall; grants land in Phase 2). Rationale: the constants stay truthful for ordinary apps while the shell and desktop are no longer capped at 32 MiB. Rejected alternative: raise the global constants (every process, including a runaway, gets the headroom). | proposed |
| 0054-D3 | The S18 reserve and the OOM victim order. At the end of boot bring-up the kernel fixes the reserve: frames allocated at that point plus a margin (open question 2), printed once as `MIRROR_MEM_RESERVE frames=<n>` (every flavor, content-free). An allocation that would drop free frames below the reserve fails for a non-protected class (syscall ENOMEM as today; the demand-fault path kills the faulting process as today); protected classes may consume the reserve. On global exhaustion the scheduler picks a victim in class order — background, then agent, then normal, never protected — preferring a process over its class limit, finishes it with a new `Outcome::OutOfMemory` (beside Terminated, userspace.rs:88-89), and reclaims it through 0030-D5's zombie/reap; with no killable process the failing path behaves as today. The kill prints `MIRROR_OOM_KILL class=<class>` (content-free). `record_fault`'s self-kill (userspace.rs:1034-1039) becomes the last resort only. Rationale: S18's confirmed order, implemented where the failure actually occurs. Rejected alternative: kill the requester always (today's behavior — a background process's growth could then kill the compositor's peer or stall the shell). | proposed |
| 0054-D4 | Markers and scans. `MIRROR_FRAME_POOL` (main.rs:185-192) gains `refcount_meta=<bytes>`; its script check is updated in the same commit. `MIRROR_MEM_RESERVE frames=<n>` prints once per boot in every flavor. `MIRROR_OOM_KILL class=<class>` is boot-smoke only (a test outcome) and joins the `$forbidden` lists and capture.ps1's absence list with the other battery markers. | proposed |

## Interfaces
- `kernel/src/memory.rs`: the refcount array (init inside `enable_recycling`, and `allocate`/`deallocate`/`allocate_contiguous_dma32` maintain it; `deallocate_contiguous` decrements per frame).
- `kernel/src/vm.rs`: the per-process limits replace the constants (:190-193, :411-413, :398); the ledger is unchanged.
- `kernel/src/sched.rs` (0030a): the process entry carries class + limits; the OOM selection and the OutOfMemory finish run in the scheduler's completion context.
- `kernel/src/userspace.rs`: spawn sites set the class from CpuPolicy; the failure sites (:1012, :1034-1039, :1058-1062, :1439) route through victim selection; windows.rs:41/:51's pixel ENOMEM is unchanged.
- `kernel/src/main.rs`: the reserve is fixed after bring-up (before any session), and `MIRROR_FRAME_POOL`/`MIRROR_MEM_RESERVE` print there.
- `crates/vm-protocol/src/lib.rs`: the two constants are documented as the normal class's defaults; the encode/decode is unchanged. Host tests follow.
- `userspace/init`: the OOM and limit probes below join the existing battery; the probe bound (main.rs:1107-1109) is unchanged.
- Scripts: run.ps1/capture.ps1/test.ps1 lists per D4.
- readme.md: the limits and OOM paragraphs (:313-315's region/quota text and the fault-behavior lines) and :185-192's pool description, in the same commit.

## Acceptance tests
1. The existing battery and every frame-baseline check pass unchanged: all current allocations hold count 1, and the refcount metadata is reserved at boot so no session delta exists (0050-D2's shared delta rule sees none).
2. Refcount self-test (boot-smoke, beside memory.rs's existing tests): share then free both references in either order, and the frame is recycled exactly at zero; increment past 255 fails; a stale double free is still rejected by the state machine.
3. Dynamic limits: two probe processes with different class limits report their own values through SYS_VM_STATS; a background probe's allocation that a normal-class limit would allow fails at its lower class limit, and the shell's larger allowance lets it commit past 32 MiB.
4. The Phase 1 exit's OOM case: a background-class probe over-commits against a shrunken frame budget (the `allocation_budget` pattern, vm.rs:155-156, extended kernel-wide); the background probe is killed first with `MIRROR_OOM_KILL class=background`, the reserve is intact, and the protected and normal processes are unaffected.
5. Reserve: an allocation drive that reaches the reserve boundary fails ENOMEM for a normal-class process while a protected-class allocation succeeds; `MIRROR_MEM_RESERVE` matches the boot's accounting.
6. Before merge (P8): the full `scripts/test.ps1` in debug and release, plus `-AgentDemo` while the agent-demo feature exists (RS6). The marker changes land in the same commit.

## Dependencies
No new crates.

## Open questions
1. Whether the reserve margin (and with it the reserve) should be a fixed MiB figure (proposed: 64 MiB over the boot footprint) or a fraction of total RAM; the QEMU smoke fixtures use 256 MiB (run.ps1:30) and the daily VM 20 GiB (G11), so a fraction behaves very differently across them.
2. The class limit table's values (proposed: normal keeps 32 MiB/256 MiB; background lower; protected bounded only by the reserve).
3. The refcount width (u8 saturating at 255): confirm, or size it u16 at double the metadata cost.
4. Whether `Outcome::OutOfMemory` is a new variant (proposed) or reuses Terminated with a marker-only distinction.
