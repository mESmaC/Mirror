//! The crash path (spec 0051, points 10 and 11): one CPU owns a crash, stops
//! every other online CPU by NMI, prints the cause, the stop and a
//! frame-pointer backtrace, then takes the final action.
//!
//! `enter` is reached from the panic handler, from the BSP's fatal exception
//! tail and from an ONLINE AP's fatal traps (point 4). Its first step is a
//! compare-and-swap on `OWNER`, keyed by the initial APIC ID from CPUID,
//! which works before this CPU has per-CPU data. A CPU that loses stops
//! itself without a word. The owner sends one NMI to each other ONLINE slot,
//! never the all-but-self shorthand: an NMI reaching an AP before it is
//! ONLINE would fault on its unmapped IST2 stack (0016a). Each target parks
//! in `nmi_stop`, the paranoid dispatch's first statement, and sets its bit
//! in `STOPPED`. The owner waits at most `STOP_WAIT_NS` for the bits, then
//! prints without the serial lock, which a stopped CPU may still hold.
//!
//! Nothing here takes a lock, allocates or touches the scheduler; the report
//! lives in statics and on this stack. Another entry by the owner, such as
//! a panic while it prints, prints `MIRROR_CRASH_NESTED depth=2` and goes
//! straight to the final action; any later one goes there silently.
//!
//! The backtrace follows RBP, which every kernel function keeps as a frame
//! pointer (`-C force-frame-pointers=yes` in `.cargo/config.toml`). It reads
//! memory only inside this CPU's known stacks, requires RBP to rise strictly
//! within each stack and every return address to lie in `.text`, and stops
//! after `MAX_FRAMES` frames. The trap stubs keep the interrupted RBP, so a
//! walk from an exception continues through the interrupted code.
//!
//! After the report, the owner draws the crash screen over the boot
//! framebuffer when every other CPU stopped (point 11), then takes the final
//! action (point 12): `mirror.panic=halt|reboot` in any flavor, else a
//! boot-smoke kernel exits QEMU with code 35, a debug kernel halts and a
//! release kernel reboots after `RELEASE_REBOOT_DELAY_MS` through 0058a's
//! restart chain (`reset::Cause::Panic`).

use core::arch::asm;
#[cfg(feature = "boot-smoke")]
use core::fmt;
use core::ops::Range;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::{apic, arch, boot, clock, framebuffer, interrupts, kernel_va, reset, serial};
use crate::{smp, stacks, timer};

/// `OWNER` while no CPU owns a crash. Initial APIC IDs are below 255.
const NO_OWNER: u32 = u32::MAX;
/// How long the owner waits for the stopped CPUs' bits.
const STOP_WAIT_NS: u64 = 1_000_000_000;
/// The crash waits' poll backstop, at 2 ns per poll as in `clock::SpinBound`:
/// it ends a wait when no clock is published or the counter stalls.
const POLL_NS: u64 = 2;
/// Polls between two clock reads in a crash wait.
const CLOCK_SAMPLE: u64 = 64;
/// The most frames a backtrace prints.
const MAX_FRAMES: usize = 32;
/// The most stacks a CPU walks: the BSP's five, and a running thread's.
const MAX_STACKS: usize = 6;
/// The slot the STOP line names for a CPU outside the slots.
const NO_SLOT: usize = 64;
/// How long a release kernel shows the crash screen before it reboots
/// without `mirror.panic=` (spec 0051, open point 4). Smoke kernels exit.
#[cfg_attr(feature = "boot-smoke", allow(dead_code))]
const RELEASE_REBOOT_DELAY_MS: u64 = 10_000;
/// `POLICY` values: the flavor's default, or `mirror.panic=`'s choice.
const POLICY_DEFAULT: u8 = 0;
const POLICY_HALT: u8 = 1;
const POLICY_REBOOT: u8 = 2;
/// Canonical kernel-half addresses start here; user ones end at 2^47.
const KERNEL_HALF: u64 = 0xffff_8000_0000_0000;
const USER_END: u64 = 1 << 47;

/// The initial APIC ID of the CPU that owns the crash, or `NO_OWNER`.
static OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);
/// The slots that stopped for the crash, the owner's losers included.
static STOPPED: AtomicU64 = AtomicU64::new(0);
/// Set once the owner reported a nested entry, and once it reported its
/// final action.
static NESTED: AtomicBool = AtomicBool::new(false);
static ACTED: AtomicBool = AtomicBool::new(false);
/// The final action `mirror.panic=` chose at boot (`configure`).
static POLICY: AtomicU8 = AtomicU8::new(POLICY_DEFAULT);
/// The Panic case's planted return address (boot-smoke): the backtrace must
/// find it among its frames.
#[cfg(feature = "boot-smoke")]
static PLANTED: AtomicU64 = AtomicU64::new(0);

/// The crash path's final action (spec 0051, point 12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Exit QEMU with code 35 (boot-smoke kernels only).
    #[cfg(feature = "boot-smoke")]
    Exit,
    Halt,
    /// Restart through 0058a's chain after `delay_ms`.
    Reboot {
        delay_ms: u64,
    },
}

impl Action {
    /// `mirror.panic=`'s choice, else the flavor's default.
    fn current() -> Self {
        match POLICY.load(Ordering::Acquire) {
            POLICY_HALT => Self::Halt,
            POLICY_REBOOT => Self::Reboot { delay_ms: 0 },
            _ => Self::DEFAULT,
        }
    }

    /// Boot-smoke kernels exit, debug kernels halt and release kernels
    /// reboot after the delay.
    #[cfg(feature = "boot-smoke")]
    const DEFAULT: Self = Self::Exit;
    #[cfg(not(feature = "boot-smoke"))]
    const DEFAULT: Self = if cfg!(debug_assertions) {
        Self::Halt
    } else {
        Self::Reboot {
            delay_ms: RELEASE_REBOOT_DELAY_MS,
        }
    };

    fn name(self) -> &'static str {
        match self {
            #[cfg(feature = "boot-smoke")]
            Self::Exit => "exit",
            Self::Halt => "halt",
            Self::Reboot { .. } => "reboot",
        }
    }

    fn delay_ms(self) -> u64 {
        match self {
            Self::Reboot { delay_ms } => delay_ms,
            _ => 0,
        }
    }
}

/// Record `mirror.panic=`'s choice (spec 0051, point 12). Once, early on the
/// BSP. A malformed or repeated token is rejected: a boot-smoke kernel
/// panics on it, as on any unknown smoke token, and any other kernel keeps
/// its flavor's default action.
pub(crate) fn configure(policy: Result<Option<boot::PanicPolicy>, boot::PanicPolicyError>) {
    let value = match policy {
        Ok(Some(boot::PanicPolicy::Halt)) => POLICY_HALT,
        Ok(Some(boot::PanicPolicy::Reboot)) => POLICY_REBOOT,
        Ok(None) => POLICY_DEFAULT,
        Err(error) => {
            #[cfg(feature = "boot-smoke")]
            panic!("invalid mirror.panic= command line: {error:?}");
            #[cfg(not(feature = "boot-smoke"))]
            {
                let _ = error;
                POLICY_DEFAULT
            }
        }
    };
    POLICY.store(value, Ordering::Release);
}

/// Whether this CPU owns a crash. The restart path's BSP-only asserts admit
/// the owner once it has stopped the other CPUs (spec 0051, point 12), and
/// its prints skip the serial lock. One atomic load, and CPUID only while a
/// crash is in progress.
pub(crate) fn is_owner() -> bool {
    let owner = OWNER.load(Ordering::Acquire);
    owner != NO_OWNER && owner == smp::initial_apic_id()
}

unsafe extern "C" {
    /// The bounds of the kernel's `.text`, from kernel/linker.ld. Only their
    /// addresses are used.
    static __mirror_text_start: u8;
    static __mirror_text_end: u8;
}

/// Why the crash happened.
pub(crate) enum Cause<'a> {
    Panic(&'a PanicInfo<'a>),
    Exception(Exception),
}

/// An unexpected trap's frame, as `MIRROR_EXCEPTION` and its context line
/// report it.
pub(crate) struct Exception {
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cr2: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// How a backtrace ended: at a zero RBP, the chain's normal end; at an RBP
/// outside this CPU's stacks or misaligned; at an RBP that did not rise
/// within its stack; at a return address outside `.text`; after
/// `MAX_FRAMES`; or at a user RBP, below a trap from CPL3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Zero,
    Range,
    Order,
    Text,
    Limit,
    User,
}

impl End {
    fn name(self) -> &'static str {
        match self {
            Self::Zero => "zero",
            Self::Range => "range",
            Self::Order => "order",
            Self::Text => "text",
            Self::Limit => "limit",
            Self::User => "user",
        }
    }
}

/// Print one crash line without the serial lock.
macro_rules! report {
    ($($arg:tt)*) => {
        serial::print_unlocked(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Crash with `cause`: own the crash or stop, stop the other CPUs, report,
/// and take the final action. Any CPU, any context, IF in any state.
pub(crate) fn enter(cause: Cause<'_>) -> ! {
    arch::disable_interrupts();
    // This function's own frame starts the walk, so its first frame is the
    // return into the panic handler or the fatal trap path.
    let frame = frame_pointer();
    let me = smp::initial_apic_id();
    if let Err(owner) = OWNER.compare_exchange(NO_OWNER, me, Ordering::AcqRel, Ordering::Acquire) {
        if owner == me {
            nested();
        }
        stop_here();
    }
    let slot = smp::crash_slot();
    let (targets, stopped) = stop_others(slot);
    // The first newline ends any line that a stopped CPU left unfinished.
    serial::print_unlocked(format_args!("\n"));
    match &cause {
        Cause::Panic(info) => report!("MIRROR_PANIC: {}", info),
        Cause::Exception(trap) => {
            report!(
                "MIRROR_EXCEPTION vector={} error={:#x} rip={:#x} cr2={:#x}",
                trap.vector,
                trap.error,
                trap.rip,
                trap.cr2
            );
            report!(
                "Exception context: cs={:#x} rflags={:#x} rsp={:#x} ss={:#x}",
                trap.cs,
                trap.rflags,
                trap.rsp,
                trap.ss
            );
        }
    }
    report!(
        "MIRROR_CRASH_STOP cpu={} targets={} stopped={}",
        slot.unwrap_or(NO_SLOT),
        targets.count_ones(),
        stopped.count_ones()
    );
    let known = known_stacks();
    #[cfg(feature = "boot-smoke")]
    let planted = PLANTED.load(Ordering::Acquire);
    #[cfg(feature = "boot-smoke")]
    let mut found = false;
    let mut rips = [0_u64; MAX_FRAMES];
    // SAFETY: `known` holds only this CPU's own stacks, which stay mapped
    // through the walk (`known_stacks`). They are raw stack memory that
    // compiled code reaches through RBP and RSP, and the walk reads frame
    // links there, which no Rust borrow covers. No other CPU writes them, and
    // with IF clear only an NMI or a fault could run here, which is fatal and
    // enters `nested`, never returning to the walk.
    let (frames, end) = unsafe {
        walk(frame, known.ranges(), &text(), |index, rip| {
            report!("MIRROR_CRASH_FRAME index={} rip={:#x}", index, rip);
            if let Some(slot) = rips.get_mut(index) {
                *slot = rip;
            }
            #[cfg(feature = "boot-smoke")]
            if planted != 0 && rip == planted {
                found = true;
            }
        })
    };
    report!(
        "MIRROR_CRASH_BACKTRACE frames={} end={}",
        frames,
        end.name()
    );
    #[cfg(feature = "boot-smoke")]
    if found {
        report!("MIRROR_CRASH_PROBE_OK");
    }
    let action = Action::current();
    // Draw only once every target stopped: a CPU still running could still
    // be drawing, and the screen must be the framebuffer's only writer.
    let screen = (stopped == targets).then(|| {
        let stop = Stop {
            slot,
            targets: targets.count_ones(),
            stopped: stopped.count_ones(),
        };
        draw(&cause, stop, &rips[..frames.min(MAX_FRAMES)], end, action)
    });
    let screen = screen.flatten();
    report!("MIRROR_CRASH_SCREEN drawn={}", u8::from(screen.is_some()));
    #[cfg(feature = "boot-smoke")]
    if screen.as_ref().is_some_and(|screen| screen.verify_reason()) {
        report!("MIRROR_CRASH_SCREEN_OK");
    }
    act()
}

/// The STOP line's facts, for the screen.
#[derive(Clone, Copy)]
struct Stop {
    slot: Option<usize>,
    targets: u32,
    stopped: u32,
}

/// Draw the crash screen (spec 0051, point 11): the reason, the stop line,
/// the backtrace and the action. `None` without a published framebuffer.
/// Only the owner calls this, once every target stopped.
fn draw(
    cause: &Cause<'_>,
    stop: Stop,
    rips: &[u64],
    end: End,
    action: Action,
) -> Option<framebuffer::CrashScreen<'static>> {
    // SAFETY: Only the owner reaches this, after `stop_others` saw every
    // target stop; a CPU that lost the race stopped too. Stopped CPUs halt
    // for good, and this path never returns to code that could draw, so the
    // screen is the framebuffer's only writer.
    let mut screen = unsafe { framebuffer::crash_screen() }?;
    match cause {
        Cause::Panic(info) => {
            screen.reason(format_args!("{}", info.message()));
            if let Some(location) = info.location() {
                screen.detail(format_args!("at {location}"));
            }
        }
        Cause::Exception(trap) => {
            screen.reason(format_args!(
                "CPU exception {} ({}), error {:#x}",
                trap.vector,
                exception_name(trap.vector),
                trap.error
            ));
            screen.detail(format_args!("rip {:#x}  cr2 {:#x}", trap.rip, trap.cr2));
        }
    }
    match stop.slot {
        Some(slot) => screen.detail(format_args!(
            "CPU {slot} crashed; {} of {} other CPUs stopped.",
            stop.stopped, stop.targets
        )),
        None => screen.detail(format_args!(
            "An unlisted CPU crashed; {} of {} other CPUs stopped.",
            stop.stopped, stop.targets
        )),
    }
    screen.detail(format_args!(
        "Backtrace: {} frames, end {}",
        rips.len(),
        end.name()
    ));
    screen.frames(rips);
    match action {
        #[cfg(feature = "boot-smoke")]
        Action::Exit => screen.action(format_args!("Test kernel: ending the run.")),
        Action::Halt => screen.action(format_args!("Halted. Restart the machine to continue.")),
        Action::Reboot { delay_ms: 0 } => screen.action(format_args!("Restarting now.")),
        Action::Reboot { delay_ms } => screen.action(format_args!(
            "Restarting in {} seconds.",
            delay_ms.div_ceil(1000)
        )),
    }
    Some(screen)
}

/// A short name for an exception vector, for the crash screen.
fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "divide error",
        1 => "debug",
        2 => "NMI",
        3 => "breakpoint",
        4 => "overflow",
        5 => "bound range",
        6 => "invalid opcode",
        7 => "device not available",
        8 => "double fault",
        10 => "invalid TSS",
        11 => "segment not present",
        12 => "stack fault",
        13 => "general protection",
        14 => "page fault",
        16 => "x87 floating point",
        17 => "alignment check",
        18 => "machine check",
        19 => "SIMD floating point",
        20 => "virtualization",
        21 => "control protection",
        _ => "unexpected vector",
    }
}

/// The paranoid dispatch's first statement (spec 0051, point 10): during a
/// crash, a CPU other than the owner that takes an NMI parks here for good.
/// Without a crash, or on the owner, it returns, and the vector is handled
/// as before: an NMI stays fatal (0016-D4).
pub(crate) fn nmi_stop(vector: u64) {
    if vector != 2 {
        return;
    }
    let owner = OWNER.load(Ordering::Acquire);
    if owner != NO_OWNER && owner != smp::initial_apic_id() {
        stop_here();
    }
}

/// The owner entered again, while it reported or acted: report that once,
/// then go straight to the final action.
fn nested() -> ! {
    if !NESTED.swap(true, Ordering::AcqRel) {
        // The newline ends the line the first entry was printing.
        report!("\nMIRROR_CRASH_NESTED depth=2");
    }
    act()
}

/// Report the final action once, then take it (spec 0051, point 12). A
/// reboot waits its delay, then restarts through 0058a's chain, whose BSP
/// asserts admit the owner. Before `reset` or `timer` is initialized, or
/// when the owner enters again from inside the action, such as a panic in
/// the restart chain, a reboot is a direct triple fault.
fn act() -> ! {
    let action = Action::current();
    let again = ACTED.swap(true, Ordering::AcqRel);
    if !again {
        report!(
            "MIRROR_CRASH_ACTION action={} delay_ms={}",
            action.name(),
            action.delay_ms()
        );
    }
    match action {
        #[cfg(feature = "boot-smoke")]
        Action::Exit => arch::exit_qemu(false),
        Action::Halt => arch::halt(),
        Action::Reboot { delay_ms } => {
            if !again {
                spin_until(delay_ms.saturating_mul(1_000_000), || false);
                if reset::is_initialized() && timer::is_initialized() {
                    reset::restart(reset::Cause::Panic);
                }
            }
            interrupts::trigger_triple_fault()
        }
    }
}

/// Park this CPU for good: record its slot in `STOPPED`, then halt with IF
/// clear. An NMI still wakes the HLT, and `nmi_stop` parks it again.
fn stop_here() -> ! {
    if let Some(slot) = smp::crash_slot().filter(|&slot| slot < NO_SLOT) {
        STOPPED.fetch_or(1 << slot, Ordering::SeqCst);
    }
    arch::halt()
}

/// Send one NMI to each ONLINE slot but `me`, then wait for their bits.
/// Returns the targets and the targets that stopped, as slot masks. A failed
/// send leaves its target running, so `stopped` then falls short.
fn stop_others(me: Option<usize>) -> (u64, u64) {
    let mut targets = 0_u64;
    for (slot, apic_id) in smp::online_cpus() {
        if Some(slot) == me || slot >= NO_SLOT {
            continue;
        }
        targets |= 1 << slot;
        let _ = apic::send_nmi(apic_id);
    }
    spin_until(STOP_WAIT_NS, || {
        STOPPED.load(Ordering::SeqCst) & targets == targets
    });
    (targets, STOPPED.load(Ordering::SeqCst) & targets)
}

/// Spin until `done` holds, for at most `limit_ns` of the HPET's forward
/// progress, or `limit_ns / POLL_NS` polls without a clock or while its
/// counter stalls. Returns whether `done` ended the wait.
fn spin_until(limit_ns: u64, mut done: impl FnMut() -> bool) -> bool {
    let mut last = clock::crash_counter_ns();
    let mut elapsed = 0_u64;
    for poll in 1..=limit_ns / POLL_NS {
        if done() {
            return true;
        }
        if poll.is_multiple_of(CLOCK_SAMPLE)
            && let (Some(before), Some(now)) = (last, clock::crash_counter_ns())
        {
            elapsed = elapsed.saturating_add(now.saturating_sub(before));
            last = Some(now);
            if elapsed >= limit_ns {
                break;
            }
        }
        core::hint::spin_loop();
    }
    done()
}

/// This function's caller's RBP: with frame pointers forced, the frame of
/// the function this is inlined into.
#[inline(always)]
fn frame_pointer() -> u64 {
    let rbp: u64;
    // SAFETY: Reading RBP changes no state; every kernel function keeps it
    // as its frame pointer.
    unsafe { asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags)) };
    rbp
}

/// The kernel's `.text` bounds.
fn text() -> Range<u64> {
    (&raw const __mirror_text_start) as u64..(&raw const __mirror_text_end) as u64
}

/// The stacks a CPU's walk may read, guards excluded.
struct Stacks {
    ranges: [Range<u64>; MAX_STACKS],
    count: usize,
}

impl Stacks {
    fn push(&mut self, range: Range<u64>) {
        if self.count < MAX_STACKS {
            self.ranges[self.count] = range;
            self.count += 1;
        }
    }

    fn ranges(&self) -> &[Range<u64>] {
        &self.ranges[..self.count]
    }
}

/// This CPU's mapped stacks, by its crash slot: the BSP's scheduler, RSP0
/// placeholder and IST1-3 stacks once mapped, or an AP's kernel and IST1-3
/// stacks, and the running thread's kernel stack when TSS.RSP0 names one in
/// the kernel VA window. None for a CPU outside the slots. Each stays mapped
/// while this CPU runs: the BSP region is never unmapped, an AP's stacks are
/// mapped before it starts and an AP takes the crash path only once it is
/// ONLINE, and RSP0 names a thread's stack only while that thread runs on
/// this CPU (0050-D2).
fn known_stacks() -> Stacks {
    let slot = smp::crash_slot();
    let mut known = Stacks {
        ranges: [const { 0..0 }; MAX_STACKS],
        count: 0,
    };
    match slot {
        Some(0) => stacks::bsp_stack_ranges()
            .into_iter()
            .flatten()
            .for_each(|range| known.push(range)),
        Some(slot) => smp::slot_stack_ranges(slot)
            .into_iter()
            .flatten()
            .for_each(|range| known.push(range)),
        None => {}
    }
    // SAFETY: `slot` is `crash_slot`'s, so it is this CPU's own.
    let rsp0 = slot.and_then(|slot| unsafe { interrupts::rsp0_of(slot) });
    if let Some(range) = rsp0.and_then(kernel_va::stack_below) {
        known.push(range);
    }
    known
}

/// Walk the RBP chain from `rbp`: each frame is the saved RBP at `rbp` and
/// the return address above it. `emit` gets each frame's index and return
/// address. Memory is read only inside `stacks`. Returns the frames emitted
/// and why the walk ended.
///
/// # Safety
///
/// Every range in `stacks` is mapped, readable memory for the whole walk,
/// and no other CPU or interrupt handler writes it until the walk returns.
/// The walk reads it through pointers made from integers, so a range inside
/// a Rust object needs that object's address exposed by a pointer-to-integer
/// cast, and no `&mut` to the object may be used until the walk returns.
unsafe fn walk(
    mut rbp: u64,
    stacks: &[Range<u64>],
    text: &Range<u64>,
    mut emit: impl FnMut(usize, u64),
) -> (usize, End) {
    let mut highest = [None::<u64>; MAX_STACKS];
    for index in 0..=MAX_FRAMES {
        if rbp == 0 {
            return (index, End::Zero);
        }
        if rbp < KERNEL_HALF {
            return (
                index,
                if rbp < USER_END {
                    End::User
                } else {
                    End::Range
                },
            );
        }
        if index == MAX_FRAMES {
            return (index, End::Limit);
        }
        let Some(stack) = stacks.iter().take(MAX_STACKS).position(|stack| {
            rbp.is_multiple_of(8)
                && stack.start <= rbp
                && rbp.checked_add(16).is_some_and(|end| end <= stack.end)
        }) else {
            return (index, End::Range);
        };
        if highest[stack].is_some_and(|previous| rbp <= previous) {
            return (index, End::Order);
        }
        highest[stack] = Some(rbp);
        // SAFETY: The two aligned words at rbp lie inside one range of
        // `stacks`, which the contract makes readable through pointers made
        // from integers, with no concurrent writer. Volatile reads form no
        // reference.
        let (next, rip) = unsafe {
            (
                core::ptr::with_exposed_provenance::<u64>(rbp as usize).read_volatile(),
                core::ptr::with_exposed_provenance::<u64>(rbp as usize + 8).read_volatile(),
            )
        };
        // A return address follows a call instruction, so it is never the
        // first byte of .text and may be its end.
        if !(text.start < rip && rip <= text.end) {
            return (index, End::Text);
        }
        emit(index, rip);
        rbp = next;
    }
    (MAX_FRAMES, End::Limit)
}

/// The Panic case (spec 0051, step 4): record the return address of a call,
/// then make that call into a function that panics. The crash's backtrace
/// must find the address among its frames (`MIRROR_CRASH_PROBE_OK`).
#[cfg(feature = "boot-smoke")]
pub(crate) fn planted_panic() -> ! {
    // SAFETY: The block stores label 2's address into PLANTED, an aligned
    // static, through RDI, then calls a function that never returns. RAX is
    // scratch and nothing follows the block. Without `nostack`, RSP is
    // aligned for the call.
    unsafe {
        asm!(
            "lea rax, [rip + 2f]",
            "mov qword ptr [rdi], rax",
            "call {target}",
            "2:",
            "ud2",
            in("rdi") PLANTED.as_ptr(),
            target = sym probe_panic,
            options(noreturn),
        )
    }
}

#[cfg(feature = "boot-smoke")]
extern "C" fn probe_panic() -> ! {
    panic!("crash probe panic")
}

/// A panic message whose formatting panics (NestedPanic, spec 0051, step 4).
#[cfg(feature = "boot-smoke")]
pub(crate) struct NestedPanic;

#[cfg(feature = "boot-smoke")]
impl fmt::Display for NestedPanic {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("nested crash test panic")
    }
}

/// Boot-smoke checks of the walker: synthetic chains end in each of the six
/// ways, and the live chain from here on the BSP's scheduler stack ends at
/// the zero RBP that boot's stack switch set. Prints
/// `MIRROR_CRASH_TESTS_OK ends=6`.
#[cfg(feature = "boot-smoke")]
pub(crate) fn self_test() {
    assert!(!arch::interrupts_enabled());
    let text = text();
    assert!(
        text.start >= 0xffff_ffff_8000_0000 && text.start < text.end,
        ".text bounds"
    );
    let here = self_test as *const () as u64;
    assert!(
        text.start < here && here < text.end,
        ".text holds the kernel"
    );
    // A synthetic stack: frame i at word 2i saves the next frame's RBP and
    // a return address inside the fake text range. Past `ptr`, the buffer is
    // reached only through `ptr` and through the walk's pointers made from
    // `base`, never through a borrow that would invalidate their provenance.
    const WORDS: usize = 2 * MAX_FRAMES + 2;
    let mut buffer = [0_u64; WORDS];
    let ptr = (&raw mut buffer).cast::<u64>();
    let base = ptr.expose_provenance() as u64;
    let fake_text = 0xffff_ffff_8000_1000..0xffff_ffff_8000_2000;
    let frame = |index: usize| base + 16 * index as u64;
    let stack = base..base + core::mem::size_of::<[u64; WORDS]>() as u64;
    let chain = |links: &[(u64, u64)]| {
        assert!(2 * links.len() <= WORDS, "synthetic chain too long");
        for (index, &(next, rip)) in links.iter().enumerate() {
            // SAFETY: 2 * index + 1 < WORDS, so both words lie inside
            // `buffer`, which is live and which no borrow aliases.
            unsafe {
                ptr.add(2 * index).write(next);
                ptr.add(2 * index + 1).write(rip);
            }
        }
        let mut rips = [0_u64; MAX_FRAMES];
        // SAFETY: `stack` is exactly `buffer`, a live local of this frame
        // whose address `base` exposed. Only this CPU touches it, through
        // `ptr` alone, and not during the walk.
        let (frames, end) = unsafe {
            walk(
                frame(0),
                core::slice::from_ref(&stack),
                &fake_text,
                |index, rip| rips[index] = rip,
            )
        };
        assert!(rips[..frames].iter().all(|rip| fake_text.contains(rip)));
        (frames, end)
    };
    let rip = fake_text.start + 0x10;
    assert_eq!(chain(&[(frame(1), rip), (0, rip)]), (2, End::Zero));
    assert_eq!(chain(&[(0xffff_ffff_9000_0000, rip)]), (1, End::Range));
    assert_eq!(chain(&[(frame(1) + 4, rip)]), (1, End::Range));
    assert_eq!(chain(&[(frame(0), rip)]), (1, End::Order));
    assert_eq!(
        chain(&[(frame(1), rip), (0, fake_text.start)]),
        (1, End::Text)
    );
    assert_eq!(chain(&[(0x7fff_0000_0000, rip)]), (1, End::User));
    let mut long = [(0, rip); MAX_FRAMES + 1];
    for (index, link) in long.iter_mut().enumerate() {
        link.0 = frame(index + 1);
    }
    assert_eq!(chain(&long), (MAX_FRAMES, End::Limit));
    // The live chain: every frame from here up to boot's continuation, on
    // the BSP's stacks.
    assert_eq!(
        smp::crash_slot(),
        Some(0),
        "the walker test runs on the BSP"
    );
    let known = known_stacks();
    let rbp = frame_pointer();
    // SAFETY: `known` holds only this CPU's own mapped stacks, as in
    // `enter`. No other CPU writes them, and with IF clear nothing else runs
    // here until the walk returns, short of a fatal NMI or fault.
    let (frames, end) = unsafe {
        walk(rbp, known.ranges(), &text, |_, rip| {
            assert!(text.start < rip && rip <= text.end);
        })
    };
    assert_eq!(end, End::Zero, "the boot RBP chain is broken");
    assert!(frames >= 1, "the boot RBP chain is empty");
    assert_eq!(OWNER.load(Ordering::Relaxed), NO_OWNER);
    serial::println!("MIRROR_CRASH_TESTS_OK ends=6");
}
