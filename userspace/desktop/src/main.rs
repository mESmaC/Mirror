#![no_std]
#![no_main]

mod font;
mod render;
mod shell;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const _: &str = include_str!("../linker.ld");
const SYS_WRITE: u64 = 1;
const SYS_TICKS: u64 = 2;
const SYS_EXIT: u64 = 3;
const SYS_SLEEP: u64 = 5;
const SYS_BOOT_HANDLE: u64 = 7;
const SYS_SURFACE_INFO: u64 = 13;
const SYS_SURFACE_PRESENT: u64 = 14;
const SYS_INPUT_POLL: u64 = 16;

global_asm!(
    ".section .text.entry,\"ax\"",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "xor rbp, rbp",
    "and rsp, -16",
    "call desktop_main",
    "ud2",
    ".size _start, .-_start",
    ".previous",
);

fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    // SAFETY: Mirror's int80 ABI preserves every GPR except result RAX and
    // restores the userspace stack. Memory effects include checked user copies.
    unsafe {
        asm!("int 0x80", inout("rax") number => result, in("rdi") arg0,
            in("rsi") arg1, in("rdx") arg2, options(nostack));
    }
    result
}

fn exit(status: u64) -> ! {
    syscall(SYS_EXIT, status, 0, 0);
    // SAFETY: An exit returning is an ABI failure. UD2 traps deterministically.
    unsafe { asm!("ud2", options(noreturn, nostack)) }
}

fn require(condition: bool, status: u64) {
    if !condition {
        exit(status);
    }
}

fn write(console: u64, bytes: &[u8]) {
    require(bytes.len() <= 256, 3);
    require(
        syscall(
            SYS_WRITE,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
            console,
        ) == bytes.len() as i64,
        4,
    );
}

fn boot_handle(index: u64) -> u64 {
    let result = syscall(SYS_BOOT_HANDLE, index, 0, 0);
    require(result > 0, 5);
    result as u64
}

// Lost names the devices whose input was lost; only the initial record of an
// input session names both (spec 0017a-D4). Position places the pointer at
// surface-local pixels (spec 0017-D17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Key { flags: u32, kind: u32, ascii: u8 },
    Mouse { dx: i32, dy: i32, buttons: u8 },
    Position { x: usize, y: usize, buttons: u8 },
    Lost { keyboard: bool, pointer: bool },
}

/// `surface` is the input surface's width and height: a position outside it
/// is rejected.
fn decode_event(bytes: &[u8; 32], surface: (usize, usize)) -> Event {
    let mut words = [0u32; 8];
    for (word, bytes) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    require(words[0] == 1, 6);
    match words[1] {
        1 => {
            require(words[2] & !63 == 0 && (1..=14).contains(&words[3]), 7);
            require(words[6] == 0 && words[7] == 0, 8);
            if words[3] == 1 {
                require((32..=126).contains(&words[4]), 9);
                require(words[5] == 0 || (32..=126).contains(&words[5]), 10);
            } else {
                require(words[4] == 0 && words[5] == 0, 11);
            }
            Event::Key {
                flags: words[2],
                kind: words[3],
                ascii: words[5] as u8,
            }
        }
        2 => {
            require(words[2] & !7 == 0 && words[3..6] == [0; 3], 12);
            let dx = words[6] as i32;
            let dy = words[7] as i32;
            require(i16::try_from(dx).is_ok() && i16::try_from(dy).is_ok(), 13);
            Event::Mouse {
                dx,
                dy,
                buttons: words[2] as u8,
            }
        }
        3 => {
            // Device mask (spec 0017a-D4): bit 0 keyboard, bit 1 pointer,
            // nonzero, every other bit and payload word zero.
            require(
                words[2] != 0 && words[2] & !3 == 0 && words[3..] == [0; 5],
                14,
            );
            Event::Lost {
                keyboard: words[2] & 1 != 0,
                pointer: words[2] & 2 != 0,
            }
        }
        4 => {
            // Pointer position (spec 0017-D17): button levels, then unsigned
            // surface-local x and y inside the surface.
            require(words[2] & !7 == 0 && words[3..6] == [0; 3], 16);
            let (x, y) = (words[6] as usize, words[7] as usize);
            require(x < surface.0 && y < surface.1, 18);
            Event::Position {
                x,
                y,
                buttons: words[2] as u8,
            }
        }
        _ => exit(15),
    }
}

fn next_input(number: u64, input: u64, surface: (usize, usize)) -> Result<Event, i64> {
    let mut bytes = [0u8; 32];
    let result = syscall(number, input, bytes.as_mut_ptr() as u64, 32);
    if result == 32 {
        Ok(decode_event(&bytes, surface))
    } else {
        Err(result)
    }
}

fn surface_dimensions(surface: u64) -> (usize, usize) {
    let mut bytes = [0u8; 24];
    require(
        syscall(SYS_SURFACE_INFO, surface, bytes.as_mut_ptr() as u64, 24) == 24,
        40,
    );
    let mut words = [0u32; 6];
    for (word, bytes) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    require(words[0] == 1 && words[3..] == [1, 16, 0], 41);
    require(
        (1..=render::MAX_WIDTH as u32).contains(&words[1])
            && (1..=render::MAX_HEIGHT as u32).contains(&words[2]),
        42,
    );
    (words[1] as usize, words[2] as usize)
}

/// The kernel starts the shell with mode 2, or 3 in smoke kernels. Modes 0
/// and 1 belonged to the retired agent-demo desktop (spec 0030a, RS6).
#[unsafe(no_mangle)]
extern "C" fn desktop_main(mode: u64) -> ! {
    require(mode == 2 || mode == 3, 1);
    shell::run(mode == 3)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    exit(127)
}
