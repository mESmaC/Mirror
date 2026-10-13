# 0051: Userspace on all CPUs

Status: draft (2026-10-12), claude; implementing on `claude/0051-smp-user`. Open points 1-5 are settled as recommended (team decision under Cameron's 2026-10-12 direction; claude decides), with point 2 subject to kimi's 0054a check.
Depends on:
- 0030a: the process and thread tables, one run queue, wait queues and wakeup preemption (sched.rs:195-211).
- 0050: per-CPU `CpuData` through GS (cpudata.rs:37-48), per-thread guarded stacks from the kernel VA window (kernel_va.rs:352-420), and RSP0 set per dispatch (userspace.rs:1753).
- 0059: the heap grows only on the BSP, at the loop-top safe point (sched.rs:1059, heap.rs:209-229).
- 0058a: its restart chain carries the release reboot (reset.rs:229-256).

Interplay with drafts that are not merged yet:
- 0054a: the frame allocator stays BSP-exclusive here (point 5), so 0054a's reserve check and refcounts stay single-threaded. Its `kill_oom_victim` destroys the victim at once (kimi/0054a-impl userspace.rs:1886-1902); it must use point 8's deferred teardown instead.
- 0019a: `perf::owner()` asserts the BSP (astra/0019a-perf-impl perf.rs:76-78). That still holds, because its hooks sit in BSP-pinned syscalls and in the BSP's pass (point 6).
- Whichever of these merges second reconciles the overlap.

Ledger: I6 (userspace on all CPUs is a v1 requirement), S17 (NMI stop, crash screen, halt or reboot), 0016-D4 as amended (the panic-path NMI stop is this spec's).

Line references are at `main` 29b2e2e (0050 merged), under `kernel/src/` unless a path is given.

## Goal
Today:
- Only the BSP runs `sched::run` (userspace.rs:1360, :2753).
- IF=0 on that one CPU is the only lock (sched.rs:77-81, :150-178).
- APs idle in `sti; hlt; cli` (smp.rs:578-588), and `MIRROR_SMP_READY` hard-codes `userspace_cpus=1` (smp.rs:294).
- A panic on an AP is silent, and a panic on the BSP stops no other CPU (main.rs:1556-1569).

After this spec:
- Every online CPU runs user threads from 0030a's queue. User code runs in parallel, and kernel entries serialize on one scheduler lock.
- Removing a user mapping shoots down exactly the CPUs that have that root loaded.
- A panic or fatal fault on any CPU stops the other CPUs by NMI, prints a frame-pointer backtrace, draws the S17 crash screen, and then halts (debug builds) or reboots (release builds).

It serves the Phase 1 exit checks at roadmap-replan.md:130-137.

## Design in brief

1. **One scheduler lock.**
   - `SCHED` is an IRQ-off ticket spinlock that records its owner slot. It replaces `Exclusive` (sched.rs:150-178), which today panics on contention, and the `Table` `Sync` argument (sched.rs:77-81).
   - It covers the tables, the run queue, `Waits`, every `Process` and `Thread`, `Ipc` and `windows::Session`. The session is published once, replacing `ACTIVE_IPC/SURFACE/INPUT/WINDOWS` (userspace.rs:65-71).
   - Each CPU holds it for its whole scheduler pass, and for the whole CPL3 handler, from the first `with_active` until the frame returns (userspace.rs:3255-3278). The handler-wide hold is needed because `wait_word` checks the futex word and then parks in a second step (userspace.rs:2429-2432, :4229-4234); locking per step would lose wakeups.
   - It is never held across `resume_user`, HLT, `heap::ensure`, `kernel_va` work or any IPI ACK wait (smp.rs:1125-1147). 2026-10-12, step 1: "`kernel_va` work" means the pool trim, whose batches wait for ACKs; a stack build or free never waits and runs under `SCHED` in the pass, and session setup and teardown hold `SCHED` except across `heap::ensure` and the trim.
   - The spin is bounded at about 1 s and then panics with "scheduler lock wait exceeded".
   - Lock order is `SCHED`, then serial. Nothing is taken while serial is held.

2. **Per-CPU dispatch.**
   - One global FIFO queue and 0030-D3's precedence stay (sched.rs:182-205). Each CPU runs its own loop over that queue.
   - These move into `CpuData`: the current process and thread slots, `need_resched` (was `RESCHEDULE`, sched.rs:207-211), `user_active` (was `USER_ACTIVE`, interrupts.rs:202), an idle flag, and the pass statistics (sched.rs:1050-1052). The 64-byte size assert at cpudata.rs:70 is raised, keeping offsets 0, 8 and 16.
   - Handlers read the current thread from `CpuData`, not `Process.current` (userspace.rs:335-336, :494-500).
   - Dispatch uses slot indices and holds no `&mut Process` across user entry (sched.rs:1247, userspace.rs:1756-1769).
   - `kernel_stack` and `set_kernel_stack` use this CPU's TSS (interrupts.rs:182, :194, :733-769). The TLS checks expect this CPU's `CpuData` (interrupts.rs:302-307, :857-858).
   - New `Thread.on_cpu` is set at dispatch and cleared at the end of `resume_process`, after the FPU image is stored (userspace.rs:1777). While it is set, `select` skips the thread and its signals stay pending. This stops a thread that parked inside its own handler (userspace.rs:4329-4358) from being dispatched on another CPU while its stack is still live.

3. **Ticks and the reschedule IPI.**
   - Each AP starts its LAPIC timer as periodic, reusing the BSP's calibrated reload (timer.rs:134-170). This needs a new "start the calling CPU's timer" path that skips `bsp_local` (apic.rs:649-655, :726-743).
   - On an AP, vector 241 sends EOI and calls `timer_tick` only when it interrupted CPL3 (interrupts.rs:1300-1321). It never advances `TICKS` (timer.rs:459-467), which stays the BSP's deadline clock.
   - New vector 242 gets a gate next to 240, 241 and 255 (interrupts.S:84-94). When it interrupts CPL3 and `need_resched` is set, it displaces the thread. `send_ipi` accepts 242 from any CPU (apic.rs:602-613, :631-633).
   - A CPU sends 242 to:
     - an idle CPU, when its pass leaves eligible work;
     - the CPUs running a killed or flushed process;
     - the BSP, when it hands the BSP work.
   - When the PIT or legacy PIC is the tick source, `userspace_cpus=1` and all userspace stays on the BSP.

4. **AP traps.**
   - On an ONLINE AP, these take the normal dispatch instead of `secondary_fault` (interrupts.rs:1203-1221): vector 0x80, CPL3 exceptions, 241, 242, and 240, which can now interrupt CPL3.
   - A CPL3 #PF on an AP goes to `userspace::fault` (userspace.rs:4360-4379).
   - CPL0 exceptions and device vectors 32-47 on an AP go to the crash path (point 10).
   - Two cases keep `secondary_fault`: a fault before the slot is ONLINE, and the armed smoke probes `OP_GUARD_FAULT` and `OP_UNMAPPED_FAULT` (smp.rs:968-993). A per-slot `expect_fault` flag marks the probes.

5. **What stays on the BSP.**
   - Device IRQs: IOAPIC routes 0, 1 and 12 keep targeting the BSP (apic.rs:36, :505), and PCI interrupts stay off (pci.rs:9).
   - The frame allocator stays a BSP `&mut` (main.rs:95, :166-171; memory.rs:313-314).
   - The heap keeps "only the BSP allocates" (heap.rs:68-72), and `alloc` now asserts it.
   - The kernel VA window (kernel_va.rs:270-298) and the command-transport producer (smp.rs:129-150) stay BSP-only.
   - So every completion that needs one of them runs in the BSP's pass:
     - `Memory`: fault materialize, the `VM_*` syscalls and `SyscallBuffer` (userspace.rs:1810-1918);
     - `Creating` (:2578-2636);
     - freeing an exited thread's stack (:940-953, :1714-1717);
     - launch and close (syscalls 17 and 21);
     - reclaim, reap and destroy (:2128-2229);
     - `heap::ensure` and stack-pool trimming.
   - An AP that gets a thread back in one of these states puts it on a BSP work list and kicks the BSP. AP scheduler stacks stay 64 KiB (smp.rs:28), because no AP runs these heavy completions.

6. **Device syscalls.**
   - Processes that hold a surface or input capability get a BSP-only affinity mask, `Process.cpus`, checked in `select` (sched.rs:1217-1234). Today those are the shell (userspace.rs:2729-2733) and init's smoke pair (:1322).
   - As a result, syscall 14 on `Object::Surface`, syscalls 15/16 on `Object::Input` (userspace.rs:3589, :4031-4076) and input-wait completion (:2342-2365) always run on the BSP. `with_surface` and `with_input` assert CPU 0 (userspace.rs:3321-3347).
   - Only the BSP's pass drains PS/2 and input arrivals (sched.rs:1151-1154). An AP's pass leaves those signals queued, so the empty-signal assert at sched.rs:1174-1177 is relaxed.
   - All other processes run on any CPU. Their `AppSurface`, `AppInput` and IPC syscalls touch only plain memory under `SCHED`.

7. **User address spaces under concurrent walkers.** Hardware on other CPUs may walk a live root while the BSP edits it.
   - Tables and leaves are published by an atomic Release store after they are zeroed (paging.rs:1155-1158, :1188-1190, :1636; vm.rs:438-440).
   - Leaves are withdrawn or downgraded by atomic swap, as the kernel windows already do (paging.rs:619-625).
   - `materialize` takes its frames and worst-case tables before publishing anything, so it never has to unmap a published leaf on failure (vm.rs:449-468).
   - User copies and the futex read (paging.rs:1494-1567, userspace.rs:2447-2449) use `rep movsb` and atomic loads, because sibling threads write user memory concurrently.
   - The BSP is the only writer of any space.

8. **User TLB shootdown and teardown.**
   - **CPU mask.** Each `UserAddressSpace` gets `cpus: AtomicU64`; `MAX_CPUS` is 64 (smp.rs:27). The bit is set (SeqCst) before `space.activate()` and cleared after `kernel.activate()` (userspace.rs:1763-1776). PCID stays off, so every root switch is a full flush (paging.rs:1879-1905) and the mask is exactly the set of CPUs that can hold translations for that root.
   - **Removal** covers `Vm::release`, `protect` downgrades and table pruning (vm.rs:243-318, paging.rs:1288-1312). It runs in two phases:
     1. Under `SCHED`, withdraw up to 512 entries into a batch, issue a fence, and read the mask.
     2. Drop `SCHED`.
     3. If the mask is non-zero, send the new `OP_FLUSH_USER(root)` to those CPUs. A target whose CR3 holds that root reloads CR3; any other target just ACKs. The operation has its own validation (root page-aligned, nonzero, within the physical mask). The kernel-only whitelist (smp.rs:658-681) is unchanged.
     4. Free the batch's frames, then retake `SCHED`.
   - `check_inactive` (paging.rs:1615-1620) becomes "the mask is 0".
   - **Teardown.** `Process::finish` (userspace.rs:509-530) marks the process killed and kicks the CPUs running its threads. Those threads return to the scheduler at their next trap. `destroy`, `reclaim` and `release_threads` (sched.rs:1069-1071) run only once the mask is 0 and no thread is `on_cpu`.
   - **Join.** A joiner is signalled only after the BSP has freed the exited thread's stack (userspace.rs:2253-2257, :2378-2383).

9. **Serial, clock, accounting and session end.**
   - `serial::print` (serial.rs:64-70) takes an owner-aware spinlock after `cli`. Re-entry on the same CPU prints without waiting. The crash owner bypasses the lock after the stop and starts with `\r\n`. 2026-10-12, step 1: the wait is bounded (at least 2.5 s), after which the CPU prints unlocked, so a CPU stopped mid-print can garble a line but never hang a report.
   - `clock::snapshot` drops its BSP assert. It loads `LAST_COUNTER` before reading the counter, then asserts the counter did not go backwards, then calls `fetch_max` (clock.rs:314-335).
   - `ServicePolicy::charge` saturates instead of asserting (userspace.rs:301-306).
   - `idle()`'s single-CPU asserts are relaxed (sched.rs:1283-1315).
   - At session end, the BSP stops new dispatches, kicks the APs, and waits outside `SCHED` until every AP is back in smp's idle loop. That keeps these true: `close`'s "no thread Running" (userspace.rs:2205), the frame baselines (:970-1000), and 0058a's "other CPUs idle" restart contract (reset.rs:5-6).

10. **Panic NMI stop** (new `crash.rs`).
    - `crash::enter(cause) -> !` is called from the panic handler (main.rs:1556-1569), the BSP fatal tail (interrupts.rs:1392-1411) and the AP routing in point 4.
    - It compare-and-swaps an owner keyed by the initial APIC ID:
      - a CPU that loses stops itself silently;
      - a CPU that is already the owner prints `MIRROR_CRASH_NESTED depth=2` and goes straight to the final action.
    - The owner sends an NMI with the new `apic::send_nmi`, which works from any CPU, uses physical destination and has bounded waits. It sends one per ONLINE slot other than itself, never the all-but-self shorthand, because an NMI that reaches an AP before it is ONLINE becomes a #PF (0016a:7). It then waits up to 1 s for each target's bit in a `STOPPED` mask.
    - The first statement of `mirror_paranoid_dispatch` (interrupts.rs:1239) parks a non-owner CPU when vector 2 arrives during a crash. Without a crash, the IST probe (interrupts.rs:1063-1146) and 0016-D4's fatal NMI behave as today.
    - The crash path takes no lock, never allocates and leaves the scheduler alone. The report lives in a static.

11. **Backtrace and crash screen.**
    - Add `-C force-frame-pointers=yes` to `.cargo/config.toml:4-9`. Today only `core`'s functions keep the frame-pointer prologue.
    - Add `.text` start and end symbols to `kernel/linker.ld:23`, and update the "Trap" note in AGENTS.md.
    - The walker follows rbp only inside this CPU's known stacks, requires rbp to increase strictly within each stack, requires return addresses inside `.text`, and stops at 32 frames.
    - At main.rs:227 the boot framebuffer's base and `Layout` are published. After the stop, the owner builds a raw writer (a documented exception to framebuffer.rs:309-347) and draws the reason, the stop line, the frames and the action on `danger_bg` (crates/ui-tokens/src/lib.rs:86-94), using the 5x7 font (framebuffer.rs:875-972).

12. **Halt or reboot.**
    - `mirror.panic=halt|reboot` is accepted in every flavor (boot.rs:189-195).
    - Without it, debug builds halt and release builds reboot after 10 s through 0058a's chain, with a new `reset::Cause::Panic` (reset.rs:33-51).
    - The BSP asserts at reset.rs:231, clock.rs:344-347 and timer.rs:252 also admit the crash owner after the stop. Before `reset` or `timer` is initialized, the action is a direct triple fault.
    - Boot-smoke builds default to `exit`, which gives exit code 35 (arch.rs:140-149).

13. **Markers.**
    - **Every flavor, content-free:**
      - `MIRROR_SMP_READY ... userspace_cpus=<n>`, where n is the online count with the LAPIC tick, else 1;
      - `MIRROR_PANIC:` and `MIRROR_EXCEPTION`, unchanged;
      - then `MIRROR_CRASH_STOP cpu= targets= stopped=`;
      - `MIRROR_CRASH_FRAME index= rip=` (at most 32);
      - `MIRROR_CRASH_BACKTRACE frames= end=<zero|range|order|text|limit|user>`;
      - `MIRROR_CRASH_SCREEN drawn=<0|1>`;
      - `MIRROR_CRASH_ACTION action=<exit|halt|reboot> delay_ms=`;
      - `MIRROR_CRASH_NESTED depth=2`.
    - **Boot-smoke only:**
      - `MIRROR_SMP_PRINT cpu= line= text=`, eight per printing CPU, then `MIRROR_SMP_TESTS_OK print_cpus=<n> print_lines=8` (step 1);
      - `MIRROR_SMP_USER_OK cpus= threads=16 ran_on= cross_wakes= flushes= stale=0`;
      - `MIRROR_USER_FLUSH_TESTS_OK`;
      - `MIRROR_TEST_EXPECT_PANIC cpu=`;
      - `MIRROR_CRASH_PROBE_OK`, `MIRROR_CRASH_SCREEN_OK`, `MIRROR_CRASH_TESTS_OK`.
    - No crash marker starts with `MIRROR_PANIC`, so none matches the panic regex at run.ps1:528-531.
    - Default builds still print nothing per input and nothing with user content.

## Implementation steps
Each step is one commit and must pass its targeted smoke cases.

1. **Lock and per-CPU state, userspace still on the BSP only.** Points 1, 2 and 9. Pass gate: the existing 1-CPU and `-CpuCount 4` cases pass unchanged, and `MIRROR_SMP_TESTS_OK` gains a check that APs and the BSP print at the same time without breaking lines. 2026-10-12: the check runs after the boot rendezvous (the BSP alone with `-LegacyPic` or one CPU). Each CPU prints a first line, waits for every CPU's first, then prints seven more; run.ps1 requires whole lines and every CPU's last line after every CPU's first, before `MIRROR_SMP_TESTS_OK print_cpus= print_lines=8`.
2. **User address-space concurrency and the shootdown.** Points 7 and 8: atomic publication, materialize taking frames up front, the CPU mask, `OP_FLUSH_USER`, two-phase removal, killed-process teardown, and join after the stack free. A boot-smoke self-test flushes a scratch root on every AP and prints `MIRROR_USER_FLUSH_TESTS_OK`. Add `UserFlushTimeout`.
3. **AP plumbing.** Points 3 and 4: AP timers, vector 242, `send_ipi` from any CPU, and AP trap routing with `expect_fault`. `MIRROR_SMP_TESTS_OK` gains checks that every AP ticks and answers a 242 round trip.
4. **NMI stop and backtrace.** Frame pointers, `crash.rs`, `send_nmi`, `STOPPED`, the paranoid hook and the routing. Add the STOP, FRAME, BACKTRACE, ACTION and NESTED markers, `Assert-MirrorCrash`, and the Panic, ApPanic, DoublePanic, NestedPanic and SchedStall cases. This lands before step 5, so that failures on APs in step 5 are visible rather than silent.
5. **APs run threads.**
   - AP loops join the session; add affinity, the BSP work list, the kicks and the session barrier.
   - Change `userspace_cpus=` together with its assertions at run.ps1:78 and capture.ps1:92.
   - Add the SMP battery.
   - Sample boot-smoke's `take_kva_dirty` at the barrier (paging.rs:632-658).
6. **Crash screen and policy.**
   - Points 11 (the screen part) and 12.
   - Add `-PanicReboot`, and a `cause=panic` branch in `Assert-MirrorReset` (scripts/common.ps1:825-916).
   - Extend the image scans (scripts/test.ps1:20-63) and the capture reject patterns.
   - Update AGENTS.md "Fault injection" and readme.md.
   - Then run the full `test.ps1` in debug and release.

## Tests
**SMP battery.** A new init role runs in every smoke boot, without a CPU pin, with 16 threads in one process. It asserts:
- every thread ran, and `ran_on=` equals `userspace_cpus`;
- futex wakes, IPC and join work across CPUs;
- with siblings writing on APs, `VM_RELEASE` makes the next sibling write fault, and the released frame, reallocated and filled with a pattern, is unchanged two ticks later (`stale=0`);
- `protect` from read-write to read-only faults the siblings;
- frame counts are restored at the barrier.

The existing init pair stays pinned to the BSP, because its alternation, budget, latency and 0030-D3 checks assume one CPU (userspace.rs:1629, :1785-1806; sched.rs:1320-1339).

**CPU counts:**
- 1: `ran_on=1`.
- 2 and 8: `-ExtendedCpu` (scripts/test.ps1:175-178).
- 4: the main SMP case and the fault set.
- `-LegacyPic` with 1 or 4 CPUs: `MIRROR_SMP_SKIPPED`, so userspace stays on the BSP.
- `-PitTimer -CpuCount 4` and `TimerCalibration`: `userspace_cpus=1`.

**Desktop.** `capture.ps1 -CpuCount 4` in both pointer modes, and the hygiene probe at `-CpuCount 4`. The shell stays on the BSP and apps run on APs. A default build must print no serial output from any AP.

**Fault cases.** Each new case follows AGENTS.md's fault-injection steps: a token in `smoke_mode` (boot.rs:196-253), the run.ps1 `ValidateSet` and `$testMode` (run.ps1:25, :288-337), a new `$panicFault` set exempt from run.ps1:528 that expects exit 35, and both build profiles in `test.ps1`.

| `-Fault` | Token | CPUs | Asserts |
| --- | --- | --- | --- |
| Panic | `panic` | 1, 4 | The BSP panics in its pass while battery threads run on every AP. `targets=n-1 stopped=n-1`. `MIRROR_CRASH_PROBE_OK` (the walker found the planted return address). `MIRROR_CRASH_SCREEN_OK` after the reason-line readback. Exit 35. |
| Panic `-PanicReboot` | `panic` and `mirror.panic=reboot` | 4 | `action=reboot delay_ms=0`, `MIRROR_RESET_BEGIN cause=panic`, the reset lines last, exit 0. |
| Panic `-NoDisplayDevice` | `panic` | 1 | `drawn=0` and no `SCREEN_OK`. |
| ApPanic | `ap-panic` | 4 | Slot 1 panics while holding `SCHED`, with the BSP spinning on it at IF=0. `cpu=1`, and every other CPU stopped by NMI. |
| DoublePanic | `double-panic` | 4 | Every CPU panics at once. Exactly one `MIRROR_PANIC` and one STOP line, with `stopped=targets`. |
| NestedPanic | `nested-panic` | 1 | A panic message whose `Display` panics. `MIRROR_CRASH_NESTED depth=2`, then `action=exit`, with no hang. |
| SchedStall | `sched-stall` | 4 | An AP holds `SCHED` forever. Another CPU's bounded spin panics with "scheduler lock wait exceeded", and the holder is stopped by NMI. |
| UserFlushTimeout | `user-flush-timeout` | 4 | One AP withholds its `OP_FLUSH_USER` ACK. The panic names the user shootdown timeout, and the batch's frames never reach the allocator. |
| Existing: PageFault, SmpTlbTimeout | none | 4 | Today's diagnostics, followed by the crash lines. |
| Existing: ThreadFault, ThreadProcessExit, ThreadLastExit, UserBudget, VmReleased, IpcPeerExit | none | 4 | The same markers as at 1 CPU, with the victim's siblings on APs, and frames restored. |

**Crash marker check.** A new closed-world `Assert-MirrorCrash` in common.ps1 runs on every smoke boot:
- outside the crash cases, no `MIRROR_CRASH_*` line may appear;
- in the crash cases, point 13's sequence appears exactly once each, in order;
- `targets=` is recomputed from the CPU count, `-LegacyPic` and whether the guard probe ran;
- `frames=` equals the number of FRAME lines, the indices are contiguous, and each rip is at least 0xffffffff80000000;
- `drawn=` matches the display switch.

**VMware (G11).** `userspace_cpus=` equals `online=` (roadmap-replan.md:130).

## Non-goals
- Per-CPU run queues, load balancing, affinity syscalls, and the CPU-time ABI for `status`.
- A frame-allocator lock, heap allocation on APs, and page-in on APs (see open point 2).
- PCID and range-based (INVLPG) user flushes.
- Device IRQs off the BSP, MSI and MSI-X.
- Saving the crash record (0034, Phase 6), the SVGA FIFO crash path (0069), and the app-crash notice (0027e).
- Non-fatal NMI handling (0016-D4).

## Open points (settled as recommended)
1. **How fine-grained device affinity is.** Recommendation: pin whole processes that hold a surface or input capability to the BSP. Use a per-thread sticky bounce instead (rewind `int 0x80` with `rip -= 2` on the AP, interrupts.rs:369-371) only if 0019c's p99 key-to-present while `burn` runs misses its 30 ms target (roadmap-replan.md:131). 0019d's `burn` must run on every CPU, not only the BSP (0019-D13).
2. **Frame allocator ownership.** Recommendation: keep it BSP-exclusive in 0051, so AP faults bounce to the BSP's pass. A global frame lock behind a `FrameSource` trait comes once AP fault latency is measured to matter, likely with 0054a's v2 allocator. The 0054a author should confirm.
3. **IF state while waiting for `SCHED`.** Recommendation: spin with IF=0, bounded, under the rule that no holder of `SCHED` waits on another CPU. A `CpuData` flag enforces the rule, and `wait_until` and `prepare_kernel_update` assert it is clear. Rejected alternative: spinning with IF=1 would force every CPL0 interrupt path to avoid `SCHED`.
4. **Release reboot delay.** Recommendation: 10 s, overridable with `mirror.panic=`. `package.ps1` builds debug by default (scripts/package.ps1:3-13), so daily VMs halt.
5. **VMware crash-screen check.** A default build cannot panic on demand without breaking hygiene. Recommendation: add a later 0015d case that boots a boot-smoke image with `mirror.test=panic`, rather than gating 0051 on it. Separately, check by hand whether VMware's "CPU disabled by the guest" dialog after the halt is acceptable.

## Risks and how the tests catch them
1. **A thread dispatched twice, or a wakeup lost across CPUs.** This is the window between parking, the FPU store and `on_cpu`, and `wait_word`'s check-then-park. Detection: `dispatch` asserts that the selected thread is not `on_cpu`. The battery's futex and IPC ping-pong with 16 threads runs at 2, 4 and 8 CPUs under `-ExtendedCpu`, and each thread checks a per-thread register and FPU canary after every wake. A lost wakeup shows up as the smoke timeout, not exit 33.
2. **Deadlock between `SCHED` and IPI ACK waits.** A holder waits for an ACK from a CPU that is spinning at IF=0, for example a heap growth or `kernel_va::trim` shootdown taken under the lock. Detection: the `CpuData` lock-held flag is asserted in `wait_until` and `prepare_kernel_update`. The bounded spin panics and names the owner slot. The heap-growth stress case and `VM_RELEASE` both run while 16 threads trap at `-CpuCount 8`, and SchedStall proves the bound fires.
3. **Stale user TLB entries, or use of a freed frame.** Causes include a missing flush, freeing frames before the ACK, a mask update raced with `activate`, and pruned tables left in paging-structure caches. Detection: the battery's `stale=0` canary, the read-write to read-only fault requirement on APs, UserFlushTimeout (frames never reach the allocator without the ACK), and exact frame counts at the session barrier.
4. **Teardown of a root still loaded on another CPU.** This covers `finish`, OOM kill and session close racing threads on APs, which ends in a kernel #PF or triple fault. Detection: `destroy` asserts the mask is 0 and no thread is `on_cpu`. ThreadFault, ThreadProcessExit, ThreadLastExit, UserBudget and IpcPeerExit run at 4 and 8 CPUs. `close` keeps its "no Running thread" assert (userspace.rs:2205), and the reclaim frame-delta asserts stay in every flavor.
5. **The crash path hangs or prints garbage.** Possible causes: panicking while holding `SCHED` or serial, an NMI reaching a pre-ONLINE or FAILED AP, a panic inside the stop handler, or frame pointers missing in release builds. Detection: ApPanic, SchedStall, DoublePanic and NestedPanic must all exit 35 within the timeout. `Assert-MirrorCrash` is closed-world, and its targets count leaves out the guard-probe slot. `MIRROR_CRASH_PROBE_OK` must appear in the release profile. Disassembling the release `kernel.elf` checks that kernel functions start with `push rbp; mov rbp,rsp`.
