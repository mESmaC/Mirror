//! Opt-in, content-free milestones and allocator samples (0019a).
//!
//! Hooks run on the BSP with IF clear. Atomics avoid mutable static references;
//! they do not make the multi-field session or HPET observation AP-safe. 0051
//! must retain BSP ownership for measured sessions or migrate this contract.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use mirror_perf_protocol::{Config, Context, Event, Item, Record};

use crate::{arch, clock, memory::FrameAllocator, serial, smp};

const ENABLED: u8 = 1;
const MEM: u8 = 2;
const INPUT: u8 = 4;
const BURN: u8 = 8;
const FLIP: u8 = 16;
const TELEMETRY: u8 = 32;
const AGENT: u8 = 64;
const INVALID: u8 = 128;
const NO_PROCESS: u64 = u64::MAX;
const SAMPLE_NS: u64 = mirror_perf_protocol::MEMORY_INTERVAL_NS;

static FLAGS: AtomicU8 = AtomicU8::new(0);
static EVENTS: AtomicU8 = AtomicU8::new(0);
static SESSION_SELECTED: AtomicBool = AtomicBool::new(false);
static SESSION_ALLOWED: AtomicBool = AtomicBool::new(false);
static PROCESS: AtomicU64 = AtomicU64::new(NO_PROCESS);
static LAST_SAMPLE: AtomicU64 = AtomicU64::new(0);

/// Called immediately after CLI, before serial, allocation or clock setup.
/// Missing Limine command-line response means off, not a boot-time panic.
pub fn init(command_line: Option<&[u8]>) {
    let flags = match command_line.map(mirror_perf_protocol::parse_command_line) {
        Some(Ok(Some(config))) => flags(config),
        Some(Err(_)) => INVALID,
        _ => 0,
    };
    FLAGS.store(flags, Ordering::Relaxed);
}

fn flags(config: Config) -> u8 {
    ENABLED
        | (u8::from(config.mem) * MEM)
        | (u8::from(config.input) * INPUT)
        | (u8::from(config.burn) * BURN)
        | (u8::from(config.flip) * FLIP)
        | (u8::from(config.telemetry) * TELEMETRY)
        | (u8::from(config.agent) * AGENT)
}

fn config(flags: u8) -> Config {
    Config {
        mem: flags & MEM != 0,
        input: flags & INPUT != 0,
        burn: flags & BURN != 0,
        flip: flags & FLIP != 0,
        telemetry: flags & TELEMETRY != 0,
        agent: flags & AGENT != 0,
    }
}

pub fn configured() -> bool {
    FLAGS.load(Ordering::Relaxed) & ENABLED != 0
}

/// ON/OFF follows the kernel banner and precedes any test-trace announcement.
pub fn announce() {
    let flags = FLAGS.load(Ordering::Relaxed);
    if flags == INVALID {
        emit(Record::Off);
    } else if flags & ENABLED != 0 {
        emit(Record::on(config(flags)));
    }
}

fn owner() {
    assert!(!arch::interrupts_enabled() && !smp::is_secondary());
}

fn timestamp() -> Option<u64> {
    clock::snapshot().map(|snapshot| snapshot.monotonic_ns)
}

fn emit(record: Record) {
    serial::println!("{}", record);
}

fn event_bit(event: Event) -> u8 {
    match event {
        Event::Clock => 1,
        Event::Display => 2,
        Event::Input => 4,
        Event::Userspace => 8,
        Event::Frame => 16,
        Event::ShellReady => 32,
    }
}

pub fn milestone(event: Event) {
    if !configured() {
        return;
    }
    owner();
    let bit = event_bit(event);
    if EVENTS.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        emit(Record::Time {
            event,
            ns: timestamp(),
        });
    }
}

/// Decide skips once after physical input setup. The retired AgentDemo path
/// needs no runtime state. Input and burn remain unavailable until 0019c/d.
pub fn select_session(available: bool) {
    if !configured() {
        return;
    }
    owner();
    assert!(!SESSION_SELECTED.swap(true, Ordering::Relaxed));
    let config = config(FLAGS.load(Ordering::Relaxed));
    let smoke = cfg!(feature = "boot-smoke");
    SESSION_ALLOWED.store(available && !smoke, Ordering::Relaxed);
    let context = Context {
        smoke,
        agent_demo: false,
        has_clock: clock::is_available(),
        has_display: EVENTS.load(Ordering::Relaxed) & event_bit(Event::Display) != 0,
        has_session: available,
    };
    for item in Item::ALL {
        if let Some(reason) = context.skip(config, item) {
            emit(Record::Skipped { item, reason });
        }
    }
}

/// Sample after shell seeding, immediately before its first scheduler entry.
pub fn start_session(pid: u64, frames: &FrameAllocator<'_>) {
    if !SESSION_ALLOWED.load(Ordering::Relaxed) {
        return;
    }
    owner();
    assert_ne!(pid, NO_PROCESS);
    assert_eq!(PROCESS.swap(pid, Ordering::Relaxed), NO_PROCESS);
    milestone(Event::Userspace);
    if FLAGS.load(Ordering::Relaxed) & MEM != 0 {
        memory_record(frames, timestamp());
    }
}

pub fn end_session() {
    PROCESS.store(NO_PROCESS, Ordering::Relaxed);
}

pub fn frame(pid: u64) {
    if PROCESS.load(Ordering::Relaxed) == pid {
        milestone(Event::Frame);
    }
}

pub fn input_poll(pid: u64) {
    if PROCESS.load(Ordering::Relaxed) == pid
        && EVENTS.load(Ordering::Relaxed) & event_bit(Event::Frame) != 0
    {
        milestone(Event::ShellReady);
    }
}

/// The first scheduler pass at least ten actual HPET seconds after the last
/// sample emits one record. A delayed pass never produces a catch-up burst.
pub fn sample_memory(frames: &FrameAllocator<'_>) {
    if PROCESS.load(Ordering::Relaxed) == NO_PROCESS || FLAGS.load(Ordering::Relaxed) & MEM == 0 {
        return;
    }
    owner();
    let Some(now) = timestamp() else {
        return;
    };
    let last = LAST_SAMPLE.load(Ordering::Relaxed);
    if now
        .checked_sub(last)
        .is_some_and(|elapsed| elapsed >= SAMPLE_NS)
    {
        memory_record(frames, Some(now));
    }
}

fn memory_record(frames: &FrameAllocator<'_>, ns: Option<u64>) {
    let counts = frames.stats().and_then(|stats| {
        mirror_perf_protocol::memory_kib(
            u64::try_from(stats.recycled_frames).ok()?,
            u64::try_from(stats.never_allocated_frames).ok()?,
            u64::try_from(stats.total_frames).ok()?,
        )
        .ok()
    });
    emit(Record::Memory {
        ns,
        available_kib: counts.map(|(available, _)| available),
        pool_kib: counts.map(|(_, pool)| pool),
    });
    if let Some(now) = ns {
        LAST_SAMPLE.store(now, Ordering::Relaxed);
    }
}
