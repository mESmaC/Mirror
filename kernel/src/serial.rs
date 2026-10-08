//! Polling 16550 COM1 diagnostics with boot-CPU interrupt exclusion.
//! Normal IRQ handlers never print; SMP will need additional synchronization.

use core::fmt::{self, Write};

use crate::arch::{self, in8, out8};

const COM1: u16 = 0x3f8;

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
        // SAFETY: COM1 belongs to the boot console; no concurrent users exist.
        if unsafe { in8(COM1 + 5) } & 0x20 != 0 {
            // SAFETY: Same exclusive COM1 ownership; transmitter is ready.
            unsafe { out8(COM1, value) };
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

pub fn print(args: fmt::Arguments<'_>) {
    let was_enabled = arch::save_and_disable_interrupts();
    let _ = Console.write_fmt(args);
    // SAFETY: Restore this CPU's entry state after the whole message. No lock is
    // held, so fatal exception diagnostics can print even during an allocation.
    unsafe { arch::restore_interrupts(was_enabled) };
}

macro_rules! println {
    ($($arg:tt)*) => {
        $crate::serial::print(format_args!("{}\n", format_args!($($arg)*)))
    };
}

pub(crate) use println;
