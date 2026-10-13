//! Polling 16550 COM1 diagnostics. Every CPU prints under one owner-aware
//! spinlock, taken with IF clear, so lines from different CPUs never break
//! each other (spec 0051, point 9). A CPU that prints again while it holds the
//! lock, from an exception or NMI that interrupted its own print, goes ahead
//! without waiting. Lock order is the scheduler lock, then this one; nothing
//! is taken while it is held. Normal IRQ handlers never print.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::{self, in8, out8};

const COM1: u16 = 0x3f8;
/// The lock word when no CPU prints.
const FREE: u32 = 0;
/// How long a CPU waits for another's print before it prints unlocked. A
/// holder only writes bytes, each bounded by `byte`, so only a CPU stopped
/// while printing can hold the lock longer; at 5 ns per poll this is at least
/// 2.5 s. Printing unlocked can then garble a line, but it never hangs a
/// diagnostic, a panic report above all.
const WAIT_POLLS: u64 = 500_000_000;

/// The printing CPU's owner value, its slot plus one, or `FREE`.
static OWNER: AtomicU32 = AtomicU32::new(FREE);

pub fn init() {
    // SAFETY: This kernel owns COM1 and initializes it on the sole running CPU.
    unsafe {
        out8(COM1 + 1, 0x00); // Disable UART interrupts.
        out8(COM1 + 3, 0x80); // Divisor latch.
        out8(COM1, 0x01); // 115200 baud.
        out8(COM1 + 1, 0x00);
        out8(COM1 + 3, 0x03); // 8 data bits, no parity, one stop bit.
        out8(COM1 + 2, 0xc7); // Enable and clear FIFOs.
        out8(COM1 + 4, 0x03); // DTR and RTS.
    }
}

fn byte(value: u8) {
    // Bound the wait so missing or broken serial hardware cannot hang boot.
    for _ in 0..100_000 {
        // SAFETY: COM1 belongs to the boot console, and the print lock (or a
        // stopped holder, see WAIT_POLLS) serializes its users.
        if unsafe { in8(COM1 + 5) } & 0x20 != 0 {
            // SAFETY: Same COM1 ownership; the transmitter is ready.
            unsafe { out8(COM1, value) };
            return;
        }
        core::hint::spin_loop();
    }
}

/// Wait until the transmitter is empty (LSR bit 6), so every byte written so
/// far has left the UART: the restart path calls it before a write that may
/// reset the machine (spec 0058a, D7). Bounded like `byte`.
pub fn flush() {
    for _ in 0..100_000 {
        // SAFETY: COM1 belongs to the boot console; reading LSR has no effect
        // on the transmitter, and IF exclusion is the caller's (restart, IF=0).
        if unsafe { in8(COM1 + 5) } & 0x40 != 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

struct Console;

impl Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for value in text.bytes() {
            if value == b'\n' {
                byte(b'\r');
            }
            byte(value);
        }
        Ok(())
    }
}

/// This CPU's owner value. It names the CPU by IA32_GS_BASE without reading
/// through GS, so it works before install, when only the BSP prints: APs
/// install their CpuData before anything on them can print.
fn owner() -> u32 {
    let slot = crate::cpudata::try_current_slot().unwrap_or(0);
    u32::try_from(slot).expect("per-CPU slot") + 1
}

/// Take the print lock for `me`, or give up after WAIT_POLLS. Returns whether
/// this call took it, and so must release it.
fn lock(me: u32) -> bool {
    for _ in 0..WAIT_POLLS {
        // Read before the exchange, so waiters do not keep stealing the
        // holder's cache line.
        if OWNER.load(Ordering::Relaxed) == FREE
            && OWNER
                .compare_exchange_weak(FREE, me, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

pub fn print(args: fmt::Arguments<'_>) {
    let was_enabled = arch::save_and_disable_interrupts();
    let me = owner();
    // Only this CPU stores its own owner value, and it clears it before it
    // re-enables interrupts, so finding it here means this print interrupted
    // this CPU's own: print at once, and leave the release to the outer print.
    let nested = OWNER.load(Ordering::Relaxed) == me;
    let locked = !nested && lock(me);
    let _ = Console.write_fmt(args);
    if locked {
        OWNER.store(FREE, Ordering::Release);
    }
    // SAFETY: Restore this CPU's entry state after the whole message. No other
    // lock is held, so fatal exception diagnostics can print even during an
    // allocation.
    unsafe { arch::restore_interrupts(was_enabled) };
}

macro_rules! println {
    ($($arg:tt)*) => {
        $crate::serial::print(format_args!("{}\n", format_args!($($arg)*)))
    };
}

pub(crate) use println;
