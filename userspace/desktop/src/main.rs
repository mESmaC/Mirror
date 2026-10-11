#![no_std]
#![no_main]

mod font;
mod render;
mod service;
mod shell;
mod ui;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const _: &str = include_str!("../linker.ld");
const SYS_WRITE: u64 = 1;
const SYS_TICKS: u64 = 2;
const SYS_EXIT: u64 = 3;
const SYS_SLEEP: u64 = 5;
const SYS_BOOT_HANDLE: u64 = 7;
const SYS_CLOSE: u64 = 11;
const SYS_DUP: u64 = 12;
const SYS_SURFACE_INFO: u64 = 13;
const SYS_SURFACE_PRESENT: u64 = 14;
const SYS_INPUT_WAIT: u64 = 15;
const SYS_INPUT_POLL: u64 = 16;
const EBADF: i64 = -9;
const EAGAIN: i64 = -11;
const EACCES: i64 = -13;
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const ETIMEDOUT: i64 = -110;
const CODE_BASE: u64 = 0x0040_0000;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Key { flags: u32, kind: u32, ascii: u8 },
    Mouse { dx: i32, dy: i32, buttons: u8 },
    Lost,
}

fn decode_event(bytes: &[u8; 32]) -> Event {
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
            require(words[2..] == [0; 6], 14);
            Event::Lost
        }
        _ => exit(15),
    }
}

fn next_input(number: u64, input: u64) -> Result<Event, i64> {
    let mut bytes = [0u8; 32];
    let result = syscall(number, input, bytes.as_mut_ptr() as u64, 32);
    if result == 32 {
        Ok(decode_event(&bytes))
    } else {
        Err(result)
    }
}

fn check_input(console: u64, surface: u64, input: u64) {
    let mut buffer = [0xa5u8; 32];
    let pointer = buffer.as_mut_ptr() as u64;
    require(syscall(SYS_BOOT_HANDLE, 1, 0, 0) > 0, 20);
    require(syscall(SYS_BOOT_HANDLE, 2, 0, 0) > 0, 21);
    require(syscall(SYS_INPUT_POLL, u64::MAX, pointer, 32) == EBADF, 22);
    require(syscall(SYS_INPUT_WAIT, u64::MAX, 0, 0) == EBADF, 23);
    require(syscall(SYS_INPUT_POLL, console, pointer, 32) == EACCES, 24);
    require(syscall(SYS_INPUT_WAIT, surface, pointer, 32) == EACCES, 25);
    let inert = syscall(SYS_DUP, input, 0, 0);
    require(inert > 0 && inert as u64 != input, 26);
    let inert = inert as u64;
    require(syscall(SYS_INPUT_POLL, inert, pointer, 32) == EACCES, 27);
    require(syscall(SYS_INPUT_WAIT, inert, 0, 0) == EACCES, 28);
    require(syscall(SYS_DUP, inert, 2, 0) == EACCES, 29);
    require(syscall(SYS_CLOSE, inert, 0, 0) == 0, 30);
    require(syscall(SYS_INPUT_POLL, inert, pointer, 32) == EBADF, 31);
    require(syscall(SYS_INPUT_POLL, input, pointer, 31) == EINVAL, 32);
    require(syscall(SYS_INPUT_WAIT, input, pointer, 33) == EINVAL, 33);
    require(syscall(SYS_INPUT_POLL, input, 0, 32) == EFAULT, 34);
    require(syscall(SYS_INPUT_WAIT, input, CODE_BASE, 32) == EFAULT, 35);
    require(
        syscall(SYS_INPUT_POLL, input, u64::MAX - 15, 32) == EFAULT,
        36,
    );
    require(buffer == [0xa5; 32], 37);
    // Invalid destinations and rights must not consume the session's first
    // staged event. A valid copy now receives its exact loss/baseline barrier.
    require(syscall(SYS_INPUT_POLL, input, pointer, 32) == 32, 38);
    require(decode_event(&buffer) == Event::Lost, 39);
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

fn smoke(console: u64, surface: u64, input: u64, service: &mut service::Service) -> ! {
    ui::self_test();
    let mut state = ui::Ui::new(1280, 800);
    service.check_idle_authority(&mut state);
    service.start(&mut state);
    let start = syscall(SYS_TICKS, 0, 0, 0) as u64;
    while service.available() {
        service.poll(&mut state);
        service.flush_reply(&mut state);
        require(
            (syscall(SYS_TICKS, 0, 0, 0) as u64).wrapping_sub(start) < 1000,
            61,
        );
        require(syscall(SYS_SLEEP, 1, 0, 0) == 0, 62);
    }
    require(service.completed() == Some(0), 63);
    service.check_idle_authority(&mut state);
    require(state.submitted() == b"Hello from Mirror agent", 64);
    write(console, b"Mirror desktop: agent operations passed\n");
    let [red, green, blue, unused] = mirror_ui_tokens::DEFAULT.accent.rgbx();
    let packet = [
        0u8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, red, green, blue, unused,
    ];
    require(
        syscall(SYS_SURFACE_PRESENT, surface, packet.as_ptr() as u64, 20) == 20,
        43,
    );
    write(console, b"Mirror desktop: ready\n");
    let mut buffer = [0xa5u8; 32];
    let pointer = buffer.as_mut_ptr() as u64;
    require(syscall(SYS_INPUT_POLL, input, pointer, 32) == EAGAIN, 44);
    require(buffer == [0xa5; 32], 45);
    let before = syscall(SYS_TICKS, 0, 0, 0);
    require(before >= 0, 46);
    require(syscall(SYS_INPUT_WAIT, input, pointer, 32) == ETIMEDOUT, 47);
    let after = syscall(SYS_TICKS, 0, 0, 0);
    require(after >= before && after - before >= 100, 48);
    require(buffer == [0xa5; 32], 49);

    // This phase never yields or blocks. The kernel's independent 32/100-tick
    // desktop allowance must throttle it before 150 delivered ticks pass.
    let start = syscall(SYS_TICKS, 0, 0, 0);
    require(start >= 0, 50);
    let mut last = start;
    let mut complete = false;
    for _ in 0..1_000_000 {
        let remaining: u64;
        // SAFETY: A finite scalar-register loop. No stack, memory, flags, SIMD,
        // or hidden syscall state is promised preserved by these instructions.
        unsafe {
            asm!("2:", "dec rcx", "jnz 2b", inout("rcx") 100_000_u64 => remaining, options(nomem, nostack));
        }
        require(remaining == 0, 51);
        let now = syscall(SYS_TICKS, 0, 0, 0);
        require(now >= last, 52);
        last = now;
        if now - start >= 150 {
            complete = true;
            break;
        }
    }
    require(complete, 53);
    write(console, b"Mirror desktop: checks passed\n");
    // SAFETY: Deliberate contained userspace fault exercises desktop teardown.
    unsafe { asm!("ud2", options(noreturn, nostack)) }
}

fn log_submission(console: u64, bytes: &[u8]) {
    const PREFIX: &[u8] = b"Mirror desktop: submit ";
    let mut line = [0u8; 128];
    line[..PREFIX.len()].copy_from_slice(PREFIX);
    line[PREFIX.len()..PREFIX.len() + bytes.len()].copy_from_slice(bytes);
    let length = PREFIX.len() + bytes.len();
    line[length] = b'\n';
    write(console, &line[..length + 1]);
}

fn append_decimal(bytes: &mut [u8; 96], length: &mut usize, mut value: usize) {
    let mut digits = [0u8; 20];
    let mut first = digits.len();
    loop {
        first -= 1;
        digits[first] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let count = digits.len() - first;
    bytes[*length..*length + count].copy_from_slice(&digits[first..]);
    *length += count;
}

fn log_pointer(console: u64, position: (usize, usize, u8)) {
    const PREFIX: &[u8] = b"Mirror desktop: pointer ";
    let mut bytes = [0u8; 96];
    bytes[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut length = PREFIX.len();
    for (index, value) in [position.0, position.1, usize::from(position.2)]
        .into_iter()
        .enumerate()
    {
        append_decimal(&mut bytes, &mut length, value);
        bytes[length] = if index == 2 { b'\n' } else { b' ' };
        length += 1;
    }
    write(console, &bytes[..length]);
}

#[unsafe(no_mangle)]
extern "C" fn desktop_main(mode: u64) -> ! {
    require(mode <= 3, 1);
    if mode >= 2 {
        shell::run(mode == 3);
    }
    let code_segment: u16;
    // SAFETY: Reading CS is permitted in userspace.
    unsafe {
        asm!("mov {selector:x}, cs", selector = out(reg) code_segment, options(nomem, nostack));
    }
    require(code_segment & 3 == 3, 2);
    let console = boot_handle(0);
    let surface = boot_handle(3);
    let input = boot_handle(4);
    let mut service = service::Service::new(boot_handle(1), boot_handle(2));
    check_input(console, surface, input);
    let (width, height) = surface_dimensions(surface);
    if mode == 1 {
        smoke(console, surface, input, &mut service);
    }
    if width < 480 || height < 360 {
        write(console, b"Mirror desktop: display is too small\n");
        exit(0);
    }
    let mut renderer = render::Renderer::new(width, height);
    let mut ui = ui::Ui::new(width, height);
    ui.paint_initial(&mut renderer);
    require(renderer.flush(surface), 54);
    write(console, b"Mirror desktop: ready\n");
    let mut deferred_changes = 0;
    loop {
        let mut changes = deferred_changes;
        deferred_changes = 0;
        // Bound each source independently; a busy agent must leave human input
        // responsive, and waiting for input alone would starve IPC requests.
        for _ in 0..16 {
            match next_input(SYS_INPUT_POLL, input) {
                Ok(event) => changes |= ui.process(event),
                Err(EAGAIN) => break,
                Err(_) => exit(56),
            }
            if changes & ui::EXIT != 0 {
                exit(0);
            }
        }
        if changes & ui::RUN_AGENT != 0 {
            changes |= service.start(&mut ui);
        }
        let completed = service.completed();
        let (remote_changes, _) = service.poll(&mut ui);
        changes |= remote_changes;
        ui.paint_changes(&mut renderer, changes);
        if renderer.flush(surface) {
            write(console, b"Mirror desktop: frame\n");
        }
        // These application diagnostics follow completed pixel transport, so
        // capture tooling can synchronize against the actual visible state.
        if changes & ui::SUBMIT != 0 {
            log_submission(console, ui.submitted());
        }
        if changes & ui::POINTER != 0 {
            log_pointer(console, ui.pointer());
        }
        if service.completed() != completed {
            match service.completed() {
                Some(0) => write(console, b"Mirror desktop: agent complete\n"),
                Some(1) => write(console, b"Mirror desktop: agent conflict\n"),
                Some(_) => write(console, b"Mirror desktop: agent failed\n"),
                None => {}
            }
        }
        // A reply is released only after the frame it describes is visible.
        deferred_changes |= service.flush_reply(&mut ui);
        require(syscall(SYS_SLEEP, 1, 0, 0) == 0, 65);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    exit(127)
}
