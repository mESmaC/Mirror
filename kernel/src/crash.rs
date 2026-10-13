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
//! The final action: a boot-smoke kernel exits QEMU with code 35, any other
//! kernel halts. Step 6 of spec 0051 adds the crash screen and the
//! halt-or-reboot policy (point 12).

use core::arch::asm;
#[cfg(feature = "boot-smoke")]
use core::fmt;
use core::ops::Range;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::{apic, arch, clock, interrupts, kernel_va, serial, smp, stacks};

/// `OWNER` while no CPU owns a crash. Initial APIC IDs are below 255.
const NO_OWNER: u32 = u32::MAX;
/// How long the owner waits for the stopped CPUs' bits.
const STOP_WAIT_NS: u64 = 1_000_000_000;
/// The stop wait's poll backstop, at 2 ns per poll as in `clock::SpinBound`:
/// it ends the wait when no clock is published or the counter stalls.
const STOP_POLLS: u64 = STOP_WAIT_NS / 2;
/// Polls between two clock reads in the stop wait.
const CLOCK_SAMPLE: u64 = 64;
/// The most frames a backtrace prints.
const MAX_FRAMES: usize = 32;
/// The most stacks a CPU walks: the BSP's five, and a running thread's.
const MAX_STACKS: usize = 6;
/// The slot the STOP line names for a CPU outside the slots.
const NO_SLOT: usize = 64;
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
/// The Panic case's planted return address (boot-smoke): the backtrace must
/// find it among its frames.
#[cfg(feature = "boot-smoke")]
static PLANTED: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "boot-smoke")]
const ACTION: &str = "exit";
#[cfg(not(feature = "boot-smoke"))]
const ACTION: &str = "halt";

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
    match cause {
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
    let known = known_stacks(slot);
    #[cfg(feature = "boot-smoke")]
    let planted = PLANTED.load(Ordering::Acquire);
    #[cfg(feature = "boot-smoke")]
    let mut found = false;
    let (frames, end) = walk(frame, known.ranges(), &text(), |index, rip| {
        report!("MIRROR_CRASH_FRAME index={} rip={:#x}", index, rip);
        #[cfg(feature = "boot-smoke")]
        if planted != 0 && rip == planted {
            found = true;
        }
    });
    report!(
        "MIRROR_CRASH_BACKTRACE frames={} end={}",
        frames,
        end.name()
    );
    #[cfg(feature = "boot-smoke")]
    if found {
        report!("MIRROR_CRASH_PROBE_OK");
    }
    act()
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

/// Report the final action once, then take it.
fn act() -> ! {
    if !ACTED.swap(true, Ordering::AcqRel) {
        report!("MIRROR_CRASH_ACTION action={} delay_ms=0", ACTION);
    }
    #[cfg(feature = "boot-smoke")]
    arch::exit_qemu(false);
    #[cfg(not(feature = "boot-smoke"))]
    arch::halt()
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
    (targets, wait_stopped(targets))
}

/// Wait until every slot in `targets` stopped, for at most `STOP_WAIT_NS` of
/// the HPET's forward progress, or `STOP_POLLS` polls without a clock.
fn wait_stopped(targets: u64) -> u64 {
    let mut last = clock::crash_counter_ns();
    let mut elapsed = 0_u64;
    for poll in 1..=STOP_POLLS {
        let stopped = STOPPED.load(Ordering::SeqCst) & targets;
        if stopped == targets {
            return stopped;
        }
        if poll.is_multiple_of(CLOCK_SAMPLE)
            && let (Some(before), Some(now)) = (last, clock::crash_counter_ns())
        {
            elapsed = elapsed.saturating_add(now.saturating_sub(before));
            last = Some(now);
            if elapsed >= STOP_WAIT_NS {
                break;
            }
        }
        core::hint::spin_loop();
    }
    STOPPED.load(Ordering::SeqCst) & targets
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

/// CPU `slot`'s mapped stacks: the BSP's scheduler, RSP0 placeholder and
/// IST1-3 stacks once mapped, or an AP's kernel and IST1-3 stacks, and the
/// running thread's kernel stack when TSS.RSP0 names one in the kernel VA
/// window. None for an unknown CPU.
fn known_stacks(slot: Option<usize>) -> Stacks {
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
    if let Some(range) = slot
        .and_then(interrupts::rsp0_of)
        .and_then(kernel_va::stack_below)
    {
        known.push(range);
    }
    known
}

/// Walk the RBP chain from `rbp`: each frame is the saved RBP at `rbp` and
/// the return address above it. `emit` gets each frame's index and return
/// address. Memory is read only inside `stacks`. Returns the frames emitted
/// and why the walk ended.
fn walk(
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
        // SAFETY: The two aligned words at rbp lie inside one of this CPU's
        // mapped stacks (or a self-test's buffer), and volatile reads form no
        // reference to memory another context may write.
        let (next, rip) = unsafe {
            (
                (rbp as *const u64).read_volatile(),
                ((rbp + 8) as *const u64).read_volatile(),
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
    // a return address inside the fake text range.
    let mut buffer = [0_u64; 2 * MAX_FRAMES + 2];
    let base = buffer.as_mut_ptr() as u64;
    let fake_text = 0xffff_ffff_8000_1000..0xffff_ffff_8000_2000;
    let frame = |index: usize| base + 16 * index as u64;
    let stack = base..base + core::mem::size_of_val(&buffer) as u64;
    let mut chain = |links: &[(u64, u64)]| {
        for (index, &(next, rip)) in links.iter().enumerate() {
            buffer[2 * index] = next;
            buffer[2 * index + 1] = rip;
        }
        // The writes are done before the walker's volatile reads.
        core::hint::black_box(&mut buffer);
        let mut rips = [0_u64; MAX_FRAMES];
        let (frames, end) = walk(
            frame(0),
            core::slice::from_ref(&stack),
            &fake_text,
            |index, rip| rips[index] = rip,
        );
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
    // The live chain: every frame from here up to boot's continuation.
    let known = known_stacks(Some(0));
    let (frames, end) = walk(frame_pointer(), known.ranges(), &text, |_, rip| {
        assert!(text.start < rip && rip <= text.end);
    });
    assert_eq!(end, End::Zero, "the boot RBP chain is broken");
    assert!(frames >= 1, "the boot RBP chain is empty");
    assert_eq!(OWNER.load(Ordering::Relaxed), NO_OWNER);
    serial::println!("MIRROR_CRASH_TESTS_OK ends=6");
}
