//! The scheduler-class battery (spec 0052, boot-smoke): two sessions of
//! init's class loads (userspace/init/src/classes.rs), which the BSP's pass
//! measures and ends.
//!
//! The saturate session runs normal spinners on every CPU that runs
//! userspace and one background spinner. Once every normal thread has run, a
//! window of at least `WINDOW_TICKS` BSP ticks, in which every CPU takes
//! `CPU_TICKS` ticks of its own, measures work conservation (each CPU spends
//! at least 3/4 of its ticks in user mode), priority (normal takes at least
//! half) and the starvation guard (the background thread runs at least
//! twice). The latency session runs background spinners on every CPU, plus
//! one, and an interactive process whose threads all wake at once, every 4
//! ticks: at least 7/8 of those wakes must run within a tick of their
//! deadline. In both, the CPUs' user ticks by class must equal the
//! processes' charges exactly, and `status` must list the ended processes.
//!
//! This is a child module of `userspace`, so it shares the private process
//! types, as `sched` does.

use mirror_thread_protocol as thread_abi;

use super::{CpuClass, FrameBaseline, MemoryClass, Outcome, create_process, drain_stacks, sched};
use crate::{
    cpudata::{self, CLASSES, MAX_CPUS},
    elf, heap,
    ipc::Ipc,
    kernel_va,
    memory::FrameAllocator,
    paging::PageTables,
    serial, smp, timer,
};

/// Init's roles for the loads (userspace/init/src/classes.rs).
const SPINNERS: u64 = 8;
const SLEEPERS: u64 = 9;
/// Each sleeper thread's timed sleeps (userspace/init/src/classes.rs).
const ROUNDS: u64 = 12;
/// The most load processes one session holds: up to 64 CPUs' spinners, 16
/// threads per process, and the other class's process.
pub(super) const PROCESSES: usize = 6;
/// The saturate window's least length in BSP ticks, the ticks each CPU must
/// take of its own within it, and its bound.
const WINDOW_TICKS: u64 = 80;
const CPU_TICKS: u64 = 16;
const WINDOW_LIMIT: u64 = 1000;
/// How long the saturate session may take to start every normal thread, and
/// the latency session to finish its rounds, in BSP ticks.
const START_LIMIT: u64 = 1000;
const LATENCY_LIMIT: u64 = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Saturate,
    Latency,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for the loads to run.
    Starting,
    /// Measuring from `Battery::start`.
    Measuring,
    /// Measured and checked; the battery ended its spinners.
    Done,
}

/// One load process.
#[derive(Clone, Copy, Debug)]
struct Load {
    slot: usize,
    class: CpuClass,
    role: u64,
    threads: u64,
}

/// The accounts the battery compares, at one pass.
#[derive(Clone, Copy)]
struct Snapshot {
    tick: u64,
    cpu_ticks: [u64; MAX_CPUS],
    cpu_user: [[u64; CLASSES]; MAX_CPUS],
    charged: [u64; PROCESSES],
    dispatches: [usize; PROCESSES],
}

/// What a session measured, for its marker.
#[derive(Clone, Copy, Debug, Default)]
struct Report {
    window: u64,
    ticks: u64,
    by_class: [u64; CLASSES],
    background_dispatches: usize,
    sleepers: u64,
    wakes: u64,
    prompt: u64,
    worst: u64,
}

/// The session in progress, which the BSP's pass drives under the scheduler
/// lock.
struct Battery {
    kind: Kind,
    phase: Phase,
    loads: [Option<Load>; PROCESSES],
    began: u64,
    start: Option<Snapshot>,
    report: Option<Report>,
}

static BATTERY: sched::Locked<Option<Battery>> = sched::Locked::new(None);

/// The CPUs in `mask`, lowest first.
fn cpus_in(mask: u64) -> impl Iterator<Item = usize> {
    (0..MAX_CPUS).filter(move |&cpu| mask & 1 << cpu != 0)
}

/// Run both sessions and print their markers (spec 0052). The BSP holds the
/// scheduler lock, with IF=0, as init's other batteries do.
pub(super) fn run(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
) {
    let cpus = u64::from(smp::userspace_mask().count_ones());
    let report = session(
        kernel,
        frames,
        bytes,
        image,
        direct_map,
        Kind::Saturate,
        &[
            (CpuClass::Normal, SPINNERS, cpus),
            (CpuClass::Background, SPINNERS, 1),
        ],
    );
    serial::println!(
        "MIRROR_SCHED_SATURATE_OK cpus={} window={} ticks={} user={} normal={} background={} background_dispatches={}",
        cpus,
        report.window,
        report.ticks,
        report.by_class.iter().sum::<u64>(),
        report.by_class[usize::from(CpuClass::Normal.rank())],
        report.by_class[usize::from(CpuClass::Background.rank())],
        report.background_dispatches
    );
    let sleepers = cpus.min(thread_abi::MAX_THREADS as u64);
    let report = session(
        kernel,
        frames,
        bytes,
        image,
        direct_map,
        Kind::Latency,
        &[
            (CpuClass::Background, SPINNERS, cpus + 1),
            (CpuClass::Interactive, SLEEPERS, sleepers),
        ],
    );
    serial::println!(
        "MIRROR_SCHED_LATENCY_OK cpus={} sleepers={} wakes={} prompt={} worst={} interactive={} background={} user={}",
        cpus,
        report.sleepers,
        report.wakes,
        report.prompt,
        report.worst,
        report.by_class[usize::from(CpuClass::Interactive.rank())],
        report.by_class[usize::from(CpuClass::Background.rank())],
        report.by_class.iter().sum::<u64>()
    );
}

/// Run one session of `loads`, each (class, init role, threads), split into
/// processes of at most 16 threads. Validate the zombies, check that
/// `status` lists them, restore the frame baseline and return the report.
fn session(
    kernel: &PageTables,
    frames: &mut FrameAllocator<'_>,
    bytes: &[u8],
    image: &elf::Image,
    direct_map: u64,
    kind: Kind,
    loads: &[(CpuClass, u64, u64)],
) -> Report {
    // 0059-D1: a session's seeding is a heap safe point.
    sched::unlocked(|| heap::ensure(kernel, frames, direct_map));
    sched::assert_drained();
    let baseline = FrameBaseline::take(frames);
    let mut table = [None; PROCESSES];
    let mut count = 0;
    for &(class, role, threads) in loads {
        let mut left = threads;
        while left != 0 {
            let chunk = left.min(thread_abi::MAX_THREADS as u64);
            assert!(count < PROCESSES, "too many class battery processes");
            let slot = create_process(
                kernel,
                frames,
                bytes,
                image,
                direct_map,
                role << 8 | chunk,
                MemoryClass::Test,
            )
            .expect("load a class battery process");
            // SAFETY: A fresh slot that nothing else references.
            let process = unsafe { sched::process(slot) };
            process.class = class;
            // The battery's own limits bound the session (`START_LIMIT`,
            // `WINDOW_LIMIT`, `LATENCY_LIMIT`), and it ends the spinners.
            process.tick_budget = None;
            process.timed_sleeps = role == SLEEPERS;
            table[count] = Some(Load {
                slot,
                class,
                role,
                threads: chunk,
            });
            count += 1;
            left -= chunk;
        }
    }
    BATTERY.with(|battery| {
        assert!(battery.is_none(), "a class battery session is running");
        *battery = Some(Battery {
            kind,
            phase: Phase::Starting,
            loads: table,
            began: timer::ticks(),
            start: None,
            report: None,
        });
    });
    let mut session = sched::Session {
        ipc: Ipc::new(),
        surface: None,
        input: None,
        runtime: None,
        end: sched::End::Classes {
            processes: table.map(|load| load.map(|load| load.slot)),
        },
    };
    sched::run(&mut session, kernel, frames, direct_map);
    assert!(session.ipc.is_drained(), "class battery IPC leaked");
    let battery = BATTERY.with(Option::take).expect("class battery state");
    assert_eq!(
        battery.phase,
        Phase::Done,
        "a class battery session ended early"
    );
    let report = battery
        .report
        .expect("the class battery session made no report");
    // Each load as `status` must list it: pid, class name and ticks.
    let mut ended = [(0, "", 0); PROCESSES];
    for (index, load) in battery.loads.iter().flatten().enumerate() {
        {
            // SAFETY: The session has returned past its barrier, and nothing
            // else references the zombie until this borrow ends.
            let process = unsafe { sched::process(load.slot) };
            assert!(
                process.zombie() && process.handles.is_empty(),
                "a class battery process kept its threads or handles"
            );
            let expected = if load.role == SLEEPERS {
                Outcome::Exit(0)
            } else {
                Outcome::Terminated
            };
            assert_eq!(
                process.outcome,
                Some(expected),
                "a class battery process ended wrongly"
            );
            assert!(process.dispatches != 0, "a class battery process never ran");
            ended[index] = (process.pid, load.class.name(), process.charged_ticks);
        }
        // SAFETY: Validation's borrow has ended, no thread is current, and
        // nothing references the entry; reap checks that it finished.
        let mut zombie = unsafe { sched::reap(load.slot) };
        // SAFETY: Every thread has left every CPU, the kernel root is active,
        // and no handler retains a reference into the user tree.
        unsafe { zombie.space.destroy(frames) }.expect("class battery address-space cleanup");
        zombie.check_charges_zero();
    }
    sched::assert_drained();
    // `status` lists the newest ended processes first (spec 0052, point 8).
    let mut listed = 0;
    sched::ended_processes(|record| {
        if listed < count {
            let (pid, class, ticks) = ended[count - 1 - listed];
            assert_eq!(
                (record.pid, record.class, record.ticks),
                (pid, class, ticks),
                "status does not list a class battery process"
            );
            listed += 1;
        }
    });
    assert_eq!(listed, count, "status lists too few ended processes");
    // Every load's stack is pooled; reset the pool's high-water mark for the
    // next session, as the SMP battery does, and trim it empty.
    // SAFETY: The BSP with IF=0 and the kernel root active; no thread runs.
    let watermark = unsafe { kernel_va::take_pool_high_water(kernel) }
        .expect("class battery stack pool high-water mark");
    assert!(
        (1..=kernel_va::STACK_PAGES).contains(&watermark),
        "class battery thread stack high-water mark out of range"
    );
    let trimmed = drain_stacks(kernel, frames);
    assert!(trimmed.complete, "the class battery drain skipped its trim");
    baseline.assert_kept(frames, "class battery frame leak");
    report
}

/// The BSP's pass hook for the session in progress (`sched::End::Classes`).
pub(super) fn pass(now: u64) {
    BATTERY.with(|battery| {
        battery.as_mut().expect("class battery state").pass(now);
    });
}

impl Battery {
    fn loads(&self) -> impl Iterator<Item = (usize, Load)> + '_ {
        self.loads
            .iter()
            .enumerate()
            .filter_map(|(index, load)| load.map(|load| (index, load)))
    }

    fn pass(&mut self, now: u64) {
        if self.phase == Phase::Done {
            return;
        }
        // Only the battery ends a spinner; the sleepers end by themselves.
        for (_, load) in self.loads() {
            // SAFETY: The scheduler's pass holds no other process reference;
            // this borrow ends with the statement.
            let outcome = unsafe { sched::process(load.slot) }.outcome;
            assert!(
                load.role == SLEEPERS || outcome.is_none(),
                "a class battery spinner ended early: {outcome:?}"
            );
        }
        match self.phase {
            Phase::Starting => {
                if self.started() {
                    self.start = Some(self.snapshot(now));
                    self.phase = Phase::Measuring;
                } else {
                    assert!(
                        now.saturating_sub(self.began) < START_LIMIT,
                        "the class battery's loads never all ran"
                    );
                }
            }
            Phase::Measuring => {
                let start = self.start.expect("a class battery window without a start");
                let elapsed = now.saturating_sub(start.tick);
                if self.measured(&start, elapsed) {
                    self.finish(now);
                } else {
                    let limit = match self.kind {
                        Kind::Saturate => WINDOW_LIMIT,
                        Kind::Latency => LATENCY_LIMIT,
                    };
                    assert!(
                        elapsed < limit,
                        "the class battery's {:?} window never closed",
                        self.kind
                    );
                }
            }
            Phase::Done => {}
        }
    }

    /// Whether the window may open: in the saturate session, once every
    /// normal thread exists and has been charged a tick; in the latency
    /// session, at once.
    fn started(&self) -> bool {
        match self.kind {
            Kind::Saturate => self
                .loads()
                .filter(|(_, load)| load.class == CpuClass::Normal)
                .all(|(_, load)| {
                    // SAFETY: As in `pass`.
                    let process = unsafe { sched::process(load.slot) };
                    process.threads().count() as u64 == load.threads
                        && process.threads().all(|thread| thread.charged_ticks != 0)
                }),
            Kind::Latency => true,
        }
    }

    /// Whether the window may close: the saturate session's after
    /// `WINDOW_TICKS` once every userspace CPU took `CPU_TICKS` of its own,
    /// the latency session's once the sleepers exited.
    fn measured(&self, start: &Snapshot, elapsed: u64) -> bool {
        match self.kind {
            Kind::Saturate => {
                elapsed >= WINDOW_TICKS
                    && cpus_in(smp::userspace_mask())
                        .all(|cpu| cpudata::tick_counts(cpu).0 - start.cpu_ticks[cpu] >= CPU_TICKS)
            }
            Kind::Latency => {
                self.loads()
                    .filter(|(_, load)| load.role == SLEEPERS)
                    .all(|(_, load)| {
                        // SAFETY: As in `pass`.
                        unsafe { sched::process(load.slot) }.finished()
                    })
            }
        }
    }

    fn snapshot(&self, now: u64) -> Snapshot {
        let mut snapshot = Snapshot {
            tick: now,
            cpu_ticks: [0; MAX_CPUS],
            cpu_user: [[0; CLASSES]; MAX_CPUS],
            charged: [0; PROCESSES],
            dispatches: [0; PROCESSES],
        };
        for cpu in (0..MAX_CPUS).filter(|&cpu| cpudata::is_installed(cpu)) {
            (snapshot.cpu_ticks[cpu], snapshot.cpu_user[cpu]) = cpudata::tick_counts(cpu);
        }
        for (index, load) in self.loads() {
            // SAFETY: As in `pass`.
            let process = unsafe { sched::process(load.slot) };
            snapshot.charged[index] = process.charged_ticks;
            snapshot.dispatches[index] = process.dispatches;
        }
        snapshot
    }

    /// Close the window: check the session's properties and its accounts,
    /// keep the report and end the spinners. The scheduler lock orders every
    /// charge, so the CPUs' and the processes' counts agree exactly.
    fn finish(&mut self, now: u64) {
        let start = self.start.expect("a class battery window without a start");
        let end = self.snapshot(now);
        let userspace = smp::userspace_mask();
        let mut report = Report {
            window: now - start.tick,
            ..Report::default()
        };
        for cpu in (0..MAX_CPUS).filter(|&cpu| cpudata::is_installed(cpu)) {
            let ticks = end.cpu_ticks[cpu] - start.cpu_ticks[cpu];
            let user: [u64; CLASSES] =
                core::array::from_fn(|class| end.cpu_user[cpu][class] - start.cpu_user[cpu][class]);
            let total: u64 = user.iter().sum();
            if userspace & 1 << cpu == 0 {
                assert_eq!(total, 0, "CPU {cpu} ran user code outside userspace");
                continue;
            }
            // Work conservation: with a spinner for every CPU, a CPU spends
            // its ticks in user mode but for the scheduler's own passes.
            if self.kind == Kind::Saturate {
                assert!(
                    total * 4 >= ticks * 3,
                    "CPU {cpu} left work queued: {total} of {ticks} ticks in user mode"
                );
            }
            report.ticks += ticks;
            for (sum, class) in report.by_class.iter_mut().zip(user) {
                *sum += class;
            }
        }
        let mut charged = [0; CLASSES];
        for (index, load) in self.loads() {
            charged[usize::from(load.class.rank())] += end.charged[index] - start.charged[index];
        }
        assert_eq!(
            charged, report.by_class,
            "the CPUs' user ticks by class differ from the processes' charges"
        );
        let user: u64 = report.by_class.iter().sum();
        let [interactive, normal, background] = report.by_class;
        match self.kind {
            Kind::Saturate => {
                let (index, _) = self
                    .loads()
                    .find(|(_, load)| load.class == CpuClass::Background)
                    .expect("the saturate session's background load");
                report.background_dispatches = end.dispatches[index] - start.dispatches[index];
                assert!(
                    report.background_dispatches >= 2 && background >= 1,
                    "the background thread starved: {} dispatches and {background} ticks in {} ticks",
                    report.background_dispatches,
                    report.window
                );
                assert!(
                    normal * 2 >= user,
                    "the normal class took {normal} of {user} user ticks"
                );
                assert_eq!(interactive, 0, "an interactive tick without a process");
            }
            Kind::Latency => {
                let (_, load) = self
                    .loads()
                    .find(|(_, load)| load.role == SLEEPERS)
                    .expect("the latency session's sleepers");
                // SAFETY: As in `pass`.
                let process = unsafe { sched::process(load.slot) };
                assert_eq!(
                    process.outcome,
                    Some(Outcome::Exit(0)),
                    "the class battery's sleepers failed"
                );
                report.sleepers = load.threads;
                report.wakes = process.sleep_wakes;
                report.prompt = process.sleep_prompt;
                report.worst = process.sleep_worst;
                assert_eq!(
                    report.wakes,
                    load.threads * ROUNDS,
                    "an interactive sleep went untimed"
                );
                assert!(
                    report.prompt * 8 >= report.wakes * 7,
                    "interactive wakes ran late: {} of {} within a tick, the worst {} ticks",
                    report.prompt,
                    report.wakes,
                    report.worst
                );
                assert!(
                    report.worst < 2 * sched::AGE_TICKS,
                    "an interactive wake waited {} ticks",
                    report.worst
                );
                assert_eq!(normal, 0, "a normal tick without a process");
            }
        }
        self.report = Some(report);
        for (_, load) in self.loads() {
            // SAFETY: As in `pass`.
            let process = unsafe { sched::process(load.slot) };
            if !process.finished() {
                process.finish(Outcome::Terminated);
            }
        }
        self.phase = Phase::Done;
    }
}
