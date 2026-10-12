# 0050: Per-CPU data and per-thread kernel stacks

Status: draft
Depends on: 0030a (threads live in its global table; their stacks are allocated and freed there). Ordered third in Phase 1 (roadmap-replan.md), before kernel heap v2 and 0051.
Ledger: none directly (Phase 1 re-plan spec). Related: I6 (userspace on all CPUs is 0051, built on this), 0016-D4 (the guarded-stack rules this spec reuses).

Line references are at HEAD 970e48c.

## Goal
The kernel gains three things it needs for SMP userspace and later phases, built once:

1. **Per-CPU data.** Today there is no per-CPU pointer: state lives in static arrays (`SLOTS`, smp.rs:107; `SECONDARY_TSS/GDT`, interrupts.rs:186-190) and "which CPU am I" is a CPUID re-read plus a linear scan (smp.rs:588-605). After this spec each CPU runs with a kernel GS base pointing at its own `CpuData`, loaded by `swapgs` in the existing trap entry, and `current_cpu()` is a read, not a scan.
2. **Per-thread kernel stacks.** Today every user thread traps onto the BSP's single 64 KiB RSP0 stack (stacks.rs:119, installed once per session at userspace.rs:697/:1256/:2033). After this spec each thread owns a guarded kernel stack, allocated at create and freed at reap, and the running thread's stack is the TSS RSP0.
3. **A post-seal kernel VA allocator.** Today `map_kernel_page` is sealed at `share_with_secondaries` (paging.rs:606-608) and only two prepared windows survive: the 2 MiB single-page shared window (paging.rs:43-45) and the 16 MiB heap window. After this spec a third pinned window parcels out guarded VA ranges at runtime — the thread stacks are its first client.

The trap-frame-retention scheduling model is unchanged: a suspended thread keeps only its `TrapFrame` and FPU state (interrupts.rs:196-197), and the kernel continuation is still rebuilt on the scheduler stack. The SYSCALL entry is unchanged and belongs to 0021 (RD21).

## Non-goals
- The SYSCALL entry rework (0021; RD21 writes it per-CPU from the start on top of this spec's GS discipline). EFER.SCE stays off (0016b).
- Userspace on APs, per-CPU run queues, and moving the `ACTIVE*` statics into `CpuData` (0051). The scheduler still runs on the BSP with IF=0.
- Changing the suspend/resume model to frozen kernel stacks. Retention is kept (rejected below); a later spec can freeze stacks on top of these per-thread stacks if deep in-kernel blocking wants it.
- Shrinking or restacking the BSP scheduler stack, the IST stacks or the AP slot stacks (0016a's layout, stacks.rs:31-46, is untouched).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0050-D1 | Per-CPU data over `swapgs`. Each CPU owns a `CpuData` (slot back-reference, current thread, its kernel stack top, per-CPU scratch such as today's `MIRROR_USER_KERNEL_RSP`, interrupts.rs:198-199). At boot each CPU's `KERNEL_GS_BASE` MSR is set to its `CpuData`; the existing trap entry (interrupts.S:93-118), which today saves and zeroes the user FS/GS state, swaps GS instead (no swap on the CPL0 path). User TLS handling and its validation (`prepare_return`, userspace.rs:3568) are unchanged; the kernel-mode TLS assertions (`kernel_tls_clean`, interrupts.rs:274-296) now require GS.base == this CPU's `CpuData`, not zero. `smp.rs`'s CPUID rescan (`current_slot`, :597-605) stays only as the early-boot fallback before GS is live. Rationale: the entry stub already pays the MSR save/zero cost, so swapgs adds nothing measurable to entry, and 0021 inherits a proven per-CPU path. Rejected alternative: keep rescanning CPUID (every scheduler operation pays a scan once APs run userspace). | proposed |
| 0050-D2 | Per-thread kernel stacks of 64 KiB, guarded to 0016a's rules (one unmapped page each side, verified before and after, `stacks.rs:197-223`), allocated from the D3 window at thread create and freed at reap (0030a-D5). The 64 KiB matches the proven RSP0 depth (stacks.rs:37); boot-smoke adds a high-water mark check to inform a later shrink. A create that cannot get a stack fails EAGAIN with full rollback, as slot-full does today (userspace.rs:1887-1891). At dispatch the BSP TSS RSP0 is set to the thread's stack top (`set_kernel_stack`, interrupts.rs:687-702, moves from once-per-session to once-per-dispatch). Frames come from the frame allocator and count in the process's owned accounting; heap v2 (Phase 1 item 4) charges them per D3 there. Rejected alternative: per-CPU trap stacks (they suffice only while suspend unwinds; per-thread stacks are the form every later model needs, and the phase is titled "built once"). | proposed |
| 0050-D3 | A post-seal kernel VA window: 64 MiB at 0xffff_d300_0000_0000, its table hierarchy pinned at init beside the heap window (the `prepare_heap_window` pattern, paging.rs:332-358), with a page-granularity VA allocator whose stack allocations always include their two guard pages, which are never mapped. Mapping and unmapping inside the window go through batched leaf updates with the `smp::KernelUpdate` shootdown protocol (smp.rs:787-865); `invalidation_range_valid` (smp.rs:636-658) whitelists the new window beside the shared and heap windows. On a 1-CPU boot, where the seal is never set (smp.rs:400-406), the window still works through the same API. Rationale: guard gaps make ad-hoc reuse of the shared window (single-page, 2 MiB, paging.rs:241-303) unworkable for 256 stacks (18 MiB of VA with guards). Rejected alternative: extend the heap window (mixing allocator metadata and guard-only VA in one arena complicates both). | proposed |
| 0050-D4 | Boot markers: `MIRROR_CPU_DATA_READY cpus=<n>` after every CPU's GS base is verified by readback (every flavor; content-free); `MIRROR_KVA_READY base=0xffffd30000000000 size=67108864` once; boot-smoke adds `MIRROR_KSTACK_OK threads=<n> watermark_pages=<n>` after test 4. The run.ps1/capture.ps1 lists are updated in the same commit (closed-world rule). | proposed |

## Interfaces
- `kernel/src/cpudata.rs` (new): `CpuData`, boot init per CPU, `current_cpu()`.
- `kernel/src/kernel_va.rs` (new): the D3 window allocator; `stacks::map_guarded` gains a post-seal path on it (the pre-seal path and its contract, stacks.rs:231-235, stay for boot stacks).
- `kernel/src/interrupts.S` / `interrupts.rs`: swapgs entry/exit, GS assertions, per-dispatch RSP0, `MIRROR_USER_KERNEL_RSP` moves into `CpuData`.
- `kernel/src/smp.rs`: APs install their `CpuData` at `secondary_entry` (:458); the invalidation whitelist (:636-658).
- `kernel/src/sched.rs` (0030a): thread create allocates the stack, dispatch installs it, reap frees it.
- `kernel/src/paging.rs`: the third prepared window.
- Scripts: marker lists only. readme.md's stack and status lines (readme.md:7) updated in the same commit.

## Acceptance tests
1. The existing battery passes unchanged: TLS, FPU, threads, sync, paging and scheduler checks, probes 7 and 18-20, and both fault suites, now entering through the swapgs path. `MIRROR_TLS_READY`/`MIRROR_TLS_KERNEL_OK` assert the new kernel-GS invariant.
2. Per-CPU data: a boot-smoke check reads `CpuData` through GS on the BSP and on every AP (via the IPI proof path, smp.rs:982-990), matching each against its `SLOTS` entry; `MIRROR_CPU_DATA_READY` reports the online count.
3. Thread stacks: two threads interleaving syscalls trap onto distinct stacks (entry RSP readback inside each thread's guarded range, boot-smoke only); a deliberate thread-stack overflow faults into #DF on IST1 exactly as the KernelStackOverflow case (0016a test 4); reap returns the stack frames to the baseline.
4. VA allocator: a boot-smoke self-test allocates, maps, probes on every CPU (the heap-growth pattern, heap.rs:304-311), unmaps and frees ranges with guard gaps intact; exhaustion and partial-failure rollback leave the window and frame counts untouched. `MIRROR_KSTACK_OK` reports the stack high-water mark.
5. Capacity: with 0030a, 256 threads across 64 processes each get a stack; creation beyond that fails EAGAIN with rollback, and frame accounting returns to baseline after reap.
6. Before merge (P8): the full `scripts/test.ps1` in debug and release (plus `-AgentDemo` while the flavor exists). New markers join the script lists in the same commit.

## Dependencies
No new crates.

## Open questions
1. The thread-stack size: 64 KiB is proven; 32 KiB halves the 16 MiB full-capacity frame cost if the watermark allows. Decided after test 4's first measurements; the spec keeps 64 KiB until then.
2. The window size (64 MiB): 18 MiB serves 256 stacks with guards; the headroom is for Phase 2 object storage. Confirm or adjust.
