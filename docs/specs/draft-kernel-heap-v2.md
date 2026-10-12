# Kernel heap v2: on-demand growth, slab caches, per-process charging

Status: draft (unnumbered NEW spec — roadmap-replan.md Phase 1 item 4; Cameron assigns the number per RD24, and the heapv2-Dn IDs below take it)
Depends on: 0030a (its tables are the heap's first persistent runtime consumer, and charging attaches to its process entries). Ordered after 0050 in Phase 1; no build dependency on 0050 — the heap window's leaf tables are already pinned at init (heap.rs:211, paging.rs:332-358), so growth maps into existing tables and needs no post-seal VA allocation.
Ledger: none directly (re-plan NEW spec). Related: S18 (the reserve and OOM order land in 0054a on top of this charging), I6 (capacity), RS1.

Line references are at HEAD 970e48c.

## Goal
The kernel heap stops being a fixed boot-sized arena. Today it is a first-fit free list (linked_list_allocator =0.10.6, kernel/Cargo.toml:16) at 1 MiB initial and 16 MiB maximum (heap.rs:28-32), grown only by two explicit boot calls (main.rs:358, :383-388), with no slab caching and no `alloc_error_handler` ("no allocation panics by design", heap.rs:126). Nothing heap-allocates at runtime, and no kernel memory is charged to a process. When this spec is done: `alloc` grows the heap on demand up to the 16 MiB cap; small objects come from size-class slab caches; and kernel memory held on behalf of a process is charged to that process's entry (0030a-D5), so 0054a can limit it.

## Non-goals
- Per-process memory limits, the S18 fixed reserve and the OOM victim order (0054a); pressure notifications (0054b). This spec adds counters, never enforcement.
- Allocation on secondary CPUs (0051): the "only the BSP allocates" invariant (heap.rs:60-64) and the non-migrating HeapGuard are unchanged.
- A kernel VA allocator (0050) — see Depends on.
- Removing the two boot growth points; they stay as pre-warming so steady-state boots rarely grow at runtime.
- The vm-protocol user-arena limits (0054a). Charging user image and stack frames is 0054a's limits decision, not this spec's.

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| heapv2-D1 | On-demand growth. On a free-list miss, `alloc` drops the lock, takes the GrowthGuard (still BSP-only and IF=0, heap.rs:155-177), grows in 1 MiB steps (MAX_GROW_PAGES, heap.rs:30) until the request fits or HEAP_MAX_SIZE rejects it, and retries once. At the cap it still returns null; no allocation panics and no `alloc_error_handler` is added. The lock is still never held across a shootdown (heap.rs:155-156), and handlers still never allocate (heap.rs:8-9). An allocation from an interrupts-enabled context cannot grow (GrowthGuard rejects it, heap.rs:162-170) and simply fails. heap.rs:7's "never implicitly grows" comment and readme.md:299-303 and :309 are updated in the same commit. Rationale: 0030a's tables and Phase 2's objects make runtime allocation permanent; explicit growth points cannot serve them. Rejected alternative: grow eagerly to 16 MiB at boot (wastes up to 13 MiB of frames on every boot for a worst case most sessions never need). | proposed |
| heapv2-D2 | Slab caches for small objects: size classes 16 through 2048 bytes in powers of two, each cache a list of arena pages with a per-page free list; allocations over 2048 bytes keep first-fit on the free list. A slab page whose objects are all free returns to the free list, so coalescing (heap.rs:756-761) is unchanged. Rationale: the 0030a table entries and Phase 2 kernel objects are small and churn; first-fit alone fragments the arena over a session. Rejected alternative: per-type caches (couples the allocator to object types; size classes cover them). | proposed |
| heapv2-D3 | Per-process charging. Every allocation site that exists to serve one process takes an owner and charges the process entry: user page-table frames (paging.rs:1385-1400, :739-747; today's `owned_tables` count, paging.rs:820, becomes the charge) and each heap object allocated with an owner. Two counters per process — kernel heap bytes and kernel frames — charged on allocation, released on free, and asserted back to zero at reap (0030a-D5). Boot and global allocations (stacks, the 0030a tables' fixed storage, DMA) stay uncharged. No limit is enforced anywhere in this spec. Rationale: S18 accounting needs the numbers before 0054a can set limits. Rejected alternative: charge frames only (Phase 2 puts object storage on the heap). | proposed |
| heapv2-D4 | Observability. `MIRROR_HEAP_GROWN` (main.rs:544) gains `phase=auto` for on-demand growths; the closed-world script check (common.ps1:768) changes from "exactly 2" to "exactly 2 boot-phase, any number of auto", and still recomputes size/free from added_pages. Boot-smoke adds `MIRROR_HEAP_CHARGE_OK` after test 4 below. The run.ps1/capture.ps1 lists are updated in the same commit (closed-world rule). | proposed |

## Interfaces
- `kernel/src/heap.rs`: the alloc slow path (D1) and the slab caches (D2); KernelAllocator, HeapGuard, GrowthGuard and the fault-injection modes are unchanged.
- `kernel/src/paging.rs`: `allocate_table` sites take an owner and charge it (D3); reclaim paths release the charge.
- `kernel/src/sched.rs` (0030a): the process entry gains the two charge counters; reap asserts both are zero.
- `scripts/common.ps1`: the `MIRROR_HEAP_GROWN` count check (D4); `run.ps1`/`capture.ps1` lists for `MIRROR_HEAP_CHARGE_OK`.
- `kernel/src/main.rs`: the two `grow_kernel_heap` calls stay; nothing else changes.

## Acceptance tests
1. The existing heap battery passes unchanged: growth rollback and guards (`MIRROR_HEAP_GROW_TESTS_OK`), the 16 MiB limit and reclaim (`MIRROR_HEAP_LIMIT_OK`), coalescing, the cross-table paging checks (`MIRROR_HEAP_PAGING_OK`) and the interrupts test (`MIRROR_HEAP_IRQ_OK`). The end-of-boot `used == 0` invariant (heap.rs:587-672) is kept, adjusted only for the named persistent allocations.
2. On-demand growth (boot-smoke): allocations past the current free space grow the heap with no explicit call and print `phase=auto`; growth to the cap succeeds; the next allocation returns null, panics nothing, and leaves the heap consistent (a following allocation after a free still succeeds).
3. Slabs: allocate/free cycles across every size class interleaved with large first-fit allocations; pattern-check the bytes; every emptied slab page returns to the free list (proven by a full-heap allocation afterwards, the heap.rs:756-761 pattern).
4. Charging: across a process create/run/reap cycle (0030a's table), the entry's kernel heap bytes and page-table frames rise and return exactly to zero, and `MIRROR_HEAP_CHARGE_OK` prints; a boot-smoke injection that leaks one charge fails the reap assert.
5. Failure injection: the existing `FailFramesAfter`/`CancelBeforeCommit` modes (heap.rs:259-265) pass against the on-demand path.
6. Before merge (P8): the full `scripts/test.ps1` in debug and release (plus `-AgentDemo` while the flavor exists). The marker script changes land in the same commit.

## Dependencies
No new crates. `linked_list_allocator =0.10.6` stays (the slab caches sit above it inside the same arena).

## Open questions
1. The spec number (RD24) and with it the decision-ID prefix.
2. The size-class list (16-2048 by powers of two) and the slab page size (one 4 KiB arena page per slab page is proposed).
3. Should `MIRROR_HEAP_READY`'s exact-value check (common.ps1:760-764) also cover the initial slab caches, or do they start empty?
