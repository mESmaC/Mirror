# 0052: Scheduler v2: work-conserving, priority classes, accounting

Status: drafted 2026-10-13 (claude), implementing on `claude/0052-sched-v2`. Settled points 1-6 below are claude's calls under Cameron's 2026-10-12 direction.
Depends on: 0030a (one run queue, wait queues, wakeup preemption), 0050 (per-CPU data), 0051 (every CPU runs user threads under `SCHED`, reschedule IPI 242, the shell pinned to the BSP).
Ledger: S4 (work-conserving, interactive above normal above background, replacing the hard 32% cap). It serves the Phase 1 exits at roadmap-replan.md:130-133: `status` shows user time on every CPU and each process's CPU time and committed memory, and 0019c's p99 key-to-present holds while 0019d's `burn` spins on every CPU.

Line references are at `main` e137f8d, under `kernel/src/` unless a path is given.

## Goal
Today:
- `select` gates every shell and app process on `ServicePolicy`: 32 charged ticks per 100-tick window, about 32% of one core for all of a process's threads together (userspace.rs:362-407, sched.rs:2037-2060). A throttled process waits even while CPUs idle.
- All ready threads share one FIFO order with no priority, so a background load delays the shell like any other thread.
- CPU time is counted per process and per thread only. The console's `status` shows the BSP's tick count and keyboard IRQs (console.rs:313-323).

After this spec:
- No CPU idles while a thread that may run on it is ready.
- Interactive threads run ahead of normal ones, and normal ones ahead of background ones, across every CPU, while a starvation guard keeps background work moving.
- CPU time is counted per thread, process, class and CPU, and `status` shows it.

## Design in brief
1. **Classes.** `CpuClass` is Interactive, Normal or Background, with ranks 0, 1 and 2. Each process gets one at creation (`Process.class`):
   - the shell, which is the desktop, is Interactive;
   - apps are Normal. `ShellRuntime::launch` takes the class, so 0019d's private burn launch can pass Background. No user ABI marks a class yet;
   - init's pair and the 0051 batteries are Normal. Each runs alone in its sessions, so their alternation, latency and budget checks are unchanged;
   - the 0052 battery's loads use all three classes.
2. **One queue, ordered by rank.** The global queue under `SCHED` stays. `select` takes the eligible thread with the least key (effective rank, woken in this pass first, queue order). Eligible means its process may run on this CPU and no CPU holds it; nothing else gates it.
   - Within one rank, 0030-D3 is unchanged: woken threads first, then a displaced thread at the head, then FIFO.
   - The starvation guard is aging. Effective rank is the class rank less one for each `AGE_TICKS` = 20 ticks (200 ms) the thread has waited ready, and never below 0. Under normal load a starved background thread runs one quantum about every 20 ticks; under interactive load, about every 40.
   - A displaced thread keeps its ready time, so a promoted thread finishes its quantum at its promoted rank.
3. **Quanta per class.** Interactive 2 ticks, normal 2 (today's value, which init's checks assume), background 4. Higher classes preempt background on wake, so its longer slice costs no latency.
4. **Work conservation.** `ServicePolicy`, `CpuPolicy`, `throttles` and the 32% cap go. After every pass the CPU places the queued threads, in selection order:
   - each takes the lowest idle CPU it may use (an AP outside the open session counts as idle);
   - failing that, it takes the CPU whose running thread has the highest dispatch rank strictly above its own class rank;
   - each CPU takes at most one thread per pass, and each target gets IPI 242 (`smp::kick`).
   This replaces 0051's "kick every idle CPU that could take any work" (sched.rs:2108-2128) and keeps its kick for a signaled wait that only a busy CPU may complete.
5. **Wakeup preemption across classes.** This falls out of point 4: a woken interactive thread queued by any CPU's pass preempts a CPU running a lower class, by IPI when the CPU is another. Local events still displace on their own CPU, as 0030-D3 says: a signaling syscall, the BSP's deadline tick and a PS/2 IRQ. `CpuData.running_rank` holds the rank at which the running thread was dispatched, its effective rank, so a promoted thread is not preempted by peers of the class it was promoted to.
6. **Budgets.** `Process.tick_budget` becomes an `Option`. It is kept only where tests need it: init's pair (192 ticks) and the 0051 batteries (4096). The shell and apps have none. UserBudget is unchanged; it still exhausts role 0's lifetime budget exactly.
7. **Accounting.**
   - Per thread and process: the charged ticks that exist today. `Process.retired_ticks` keeps the ticks of member threads already removed, and `reap` asserts, in every flavor, that `charged_ticks` equals the sum of the retired ticks and the live threads' ticks.
   - Per CPU (`CpuData`): every selected-source tick it took, and the ticks that charged a user thread, split by class. They are counted where the process is charged, under `SCHED`, so the sums match exactly.
   - Ended processes: the last 8 that ran, recorded at reap (pid, class, ticks, peak committed bytes). `Vm` tracks the committed peak. The BSP writes the record; the console reads it.
8. **`status`.** After its two existing lines, it shows `CPU user/all ticks:` lines with `N:user/all` for every installed CPU (at most 3 lines, then `+K more`), and one line for each of the last 4 ended processes: `pid P class: T ticks, peak C KiB committed`. The console runs only after the shell session ends, so this lists ended processes. A live view comes with a shell task view, not here.
9. **Why one queue.** `SCHED` already covers each whole pass, the tables, the waits and the sessions (0051 point 1), so per-CPU queues would not remove the global lock. They would add balancing and migration, and cross-CPU priority would then need the same IPI placement. A pass scans at most 256 entries. Per-CPU queues come only if 0019c's p99 under `burn` shows the lock or the scan as the cost.

## Tests
**Class self-test** (every smoke boot, replacing `MIRROR_SERVICE_POLICY_TESTS_OK`):
- Three processes, one per class, plus a second normal thread, queued and never run. `select` must order them by rank; within a rank by FIFO; a woken thread first within its rank but never ahead of a better rank; at 20 and 40 ticks of age, one and two ranks higher; and must skip a process whose CPU mask excludes this CPU.
- The placement function is checked on synthetic CPU states:
  - an idle CPU first;
  - else the CPU with the lowest-priority dispatch rank;
  - never one at equal rank;
  - never a CPU twice.
- The quanta are checked, and the frame baseline holds after teardown. The test prints `MIRROR_SCHED_CLASS_TESTS_OK quanta=2,2,4 age_ticks=20`.

**Class battery** (boot-smoke, `-Fault None` boots only, after `MIRROR_SMP_USER_OK` and before the first init pair). Init roles 8 (spinners) and 9 (timed sleepers) are the loads, with the thread count in the probe byte. N is the number of userspace CPUs; a spinner process holds at most 16 threads, so more processes cover N > 15.
- **Saturate session:** normal spinners on N threads, and one background spinner.
  - The window opens once every normal thread has been charged a tick. It closes after 80 BSP ticks, and once every userspace CPU has taken 16 ticks of its own; it is bounded at 1000 ticks.
  - Work conservation: on each userspace CPU, user ticks are at least 3/4 of its ticks.
  - Priority: normal took at least half of the user ticks.
  - Starvation freedom: the background thread was dispatched at least twice and charged at least once.
  - Accounting: the CPUs' user ticks, by class and in total, equal the processes' charged ticks exactly.
  - It prints `MIRROR_SCHED_SATURATE_OK cpus= window= ticks= user= normal= background= background_dispatches=`.
- **Latency session:** background spinners on N + 1 threads, and an interactive process of K = min(N, 16) threads.
  - Each thread runs 12 rounds: sleep to the next multiple of 4 ticks, then spin until two ticks later, so all K need CPUs at once.
  - The kernel times every sleep from its deadline to its dispatch.
  - Required: all 12K wakes timed; at least 7/8 dispatched within 1 tick; the worst under 40 ticks (2 x AGE, which tolerates host stalls on QEMU); and the same exact accounting sums.
  - Without placement, the APs would take the woken threads only at background quantum ends, up to 4 ticks late.
  - It prints `MIRROR_SCHED_LATENCY_OK cpus= sleepers= wakes= prompt= worst= interactive= background= user=`.
- `run.ps1` requires each line exactly once in `-Fault None` boots, in order, with `cpus=` equal to the userspace CPUs. It recomputes the relations: user equals the classes' sum, the 3/4 floor in aggregate, at least 2 dispatches, normal at least half, wakes equal 12 x sleepers, the 7/8 floor, and worst under 40. Every other scenario must print neither line. The image scans forbid all three markers in default and trace kernels, and the captures reject them.

**Cases.** `build.ps1 -Smoke`, then `run.ps1 -Smoke -NoBuild` with: no switches; `-CpuCount 4`; `-ExtendedCpu -CpuCount 8`; `-LegacyPic` with 1 and 4 CPUs; and `-PitTimer -CpuCount 4`. Also UserBudget and the thread, IPC and VM faults at 1 and 4 CPUs, and the key ones in release. The shell's class changes, so the captures at `-CpuCount 4` in both pointer modes and the hygiene probe run too. Then the full `test.ps1`.

## Non-goals
Per-CPU run queues and load balancing (point 9); CPU affinity syscalls; nice values or more classes; real-time classes; a user ABI to choose a class; a live process view; tickless or sub-tick accounting (the high-resolution timer spec, Phase 2).

## Settled points (claude, 2026-10-13)
1. One global queue (point 9).
2. The guard is aging by one class per 20 ticks, not a reserved share. It needs no per-CPU budget state, and its bound on waiting holds on every CPU count.
3. Init's pair and the 0051 batteries stay Normal with their lifetime budgets, since each class is alone in its session. "Test loads background" applies to the 0052 battery's background loads.
4. Preemption compares a queued thread's class with the running thread's dispatch rank (point 5), so aging never triggers a preemption by itself.
5. The battery runs in `-Fault None` boots only, about 25 per profile at 1, 2, 4 and 8 CPUs, with the PIT and with the legacy PIC. It costs about 1.5 s per boot. Fault cases do not exercise classes.
6. `status` reports ended processes, because the console runs only after the shell (point 8).
