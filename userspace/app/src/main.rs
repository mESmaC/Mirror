//! Independent Notes and Counter processes. Each receives one private surface
//! and routed input; window policy and display composition belong to the shell.
#![no_std]
#![no_main]

mod document;
mod font;
mod render;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use mirror_window_protocol::{Event, SYS_SURFACE_COMMIT};

const _: &str = include_str!("../linker.ld");
const SYS_WRITE: u64 = 1;
const SYS_TICKS: u64 = 2;
const SYS_EXIT: u64 = 3;
const SYS_PID: u64 = 6;
const SYS_BOOT_HANDLE: u64 = 7;
const SYS_SURFACE_INFO: u64 = 13;
const SYS_SURFACE_PRESENT: u64 = 14;
const SYS_INPUT_WAIT: u64 = 15;
const SYS_INPUT_POLL: u64 = 16;
const EAGAIN: i64 = -11;
const ETIMEDOUT: i64 = -110;

global_asm!(
    ".section .text.entry,\"ax\"",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "xor rbp, rbp",
    "and rsp, -16",
    "call app_main",
    "ud2",
    ".size _start, .-_start",
    ".previous",
);

fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    // SAFETY: Mirror preserves all GPRs except result RAX across int80 and
    // restores the user stack. Pointer arguments use checked kernel copies.
    unsafe {
        asm!("int 0x80", inout("rax") number => result, in("rdi") arg0,
            in("rsi") arg1, in("rdx") arg2, options(nostack));
    }
    result
}

fn exit(status: u64) -> ! {
    syscall(SYS_EXIT, status, 0, 0);
    // SAFETY: EXIT must not return. Trap deterministically if the ABI fails.
    unsafe { asm!("ud2", options(noreturn, nostack)) }
}

fn require(condition: bool, status: u64) {
    if !condition {
        exit(status);
    }
}

fn handle(slot: u64) -> u64 {
    let result = syscall(SYS_BOOT_HANDLE, slot, 0, 0);
    require(result > 0, 2);
    result as u64
}

fn write(console: u64, bytes: &[u8]) {
    require(
        syscall(
            SYS_WRITE,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
            console,
        ) == bytes.len() as i64,
        3,
    );
}

fn decimal(mut value: u64, buffer: &mut [u8; 20]) -> &[u8] {
    let mut first = buffer.len();
    loop {
        first -= 1;
        buffer[first] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buffer[first..];
        }
    }
}

fn log(console: u64, pid: u64, event: &[u8], suffix: Option<&[u8]>) {
    let mut line = [0u8; 160];
    let mut length = 0;
    let mut digits = [0u8; 20];
    for bytes in [
        b"Mirror app: ".as_slice(),
        event,
        b" ",
        decimal(pid, &mut digits),
    ] {
        line[length..length + bytes.len()].copy_from_slice(bytes);
        length += bytes.len();
    }
    if let Some(bytes) = suffix {
        line[length] = b' ';
        length += 1;
        line[length..length + bytes.len()].copy_from_slice(bytes);
        length += bytes.len();
    }
    line[length] = b'\n';
    write(console, &line[..length + 1]);
}

fn next_event(number: u64, input: u64) -> Result<Event, i64> {
    let mut bytes = [0u8; 32];
    match syscall(number, input, bytes.as_mut_ptr() as u64, bytes.len() as u64) {
        32 => Ok(Event::decode(&bytes).unwrap_or_else(|_| exit(4))),
        error => Err(error),
    }
}

fn check_authority(console: u64, surface: u64, input: u64) {
    let mut buffer = [0xa5u8; 32];
    let address = buffer.as_mut_ptr() as u64;
    require(syscall(SYS_BOOT_HANDLE, 5, 0, 0) == -9, 10);
    require(syscall(SYS_SURFACE_INFO, console, address, 24) == -13, 11);
    require(syscall(SYS_INPUT_POLL, console, address, 32) == -13, 12);
    require(syscall(SYS_INPUT_POLL, surface, address, 32) == -13, 13);
    require(syscall(SYS_SURFACE_INFO, input, address, 24) == -13, 14);
    require(syscall(SYS_SURFACE_COMMIT, console, 0, 0) == -13, 15);
    require(buffer == [0xa5; 32], 16);
    require(syscall(SYS_SURFACE_INFO, surface, address, 24) == 24, 17);
    let mut words = [0u32; 6];
    for (word, bytes) in words.iter_mut().zip(buffer[..24].as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    require(
        words == [1, render::WIDTH as u32, render::HEIGHT as u32, 1, 16, 0],
        18,
    );
}

struct App {
    kind: u64,
    busy: bool,
    document: document::Document,
    focused: bool,
    buttons: u8,
    clicks_armed: bool,
    counter_start: u64,
    elapsed: u64,
    last_counter_frame: u64,
}

impl App {
    fn new(kind: u64) -> Self {
        let now = syscall(SYS_TICKS, 0, 0, 0) as u64;
        Self {
            kind,
            busy: false,
            document: document::Document::new(),
            focused: false,
            buttons: 0,
            clicks_armed: false,
            counter_start: now,
            elapsed: 0,
            last_counter_frame: now,
        }
    }

    fn reset_counter(&mut self) {
        self.counter_start = syscall(SYS_TICKS, 0, 0, 0) as u64;
        self.elapsed = 0;
        self.last_counter_frame = self.counter_start;
    }

    fn event(&mut self, event: Event, console: u64, pid: u64) -> Changes {
        let mut changes = Changes::default();
        match event {
            Event::Key { flags, kind, ascii } if self.focused && flags & 1 != 0 => {
                // Control/Alt chords belong to the shell and never enter text.
                if flags & (8 | 16) == 0 {
                    if self.kind == 0 {
                        (changes.repaint, changes.text) = self.document.key(kind, ascii);
                    } else if kind == 2 && flags & 2 == 0 {
                        self.reset_counter();
                        changes.repaint = true;
                        changes.reset = true;
                    }
                }
            }
            Event::Pointer { x, y, buttons } => {
                if buttons == 0 {
                    self.clicks_armed = true;
                }
                if self.focused && self.clicks_armed && buttons & 1 != 0 && self.buttons & 1 == 0 {
                    if self.kind == 0 {
                        if let Some(position) = render::note_position(x as usize, y as usize) {
                            changes.repaint = self.document.place(position);
                        }
                    } else if render::reset_hit(x as usize, y as usize) {
                        self.reset_counter();
                        changes.repaint = true;
                        changes.reset = true;
                    }
                }
                self.buttons = buttons;
            }
            Event::Focus(focused) => {
                changes.repaint = self.focused != focused;
                self.focused = focused;
                self.buttons = 0;
                self.clicks_armed = false;
            }
            Event::Reset => {
                self.buttons = 0;
                self.clicks_armed = false;
            }
            Event::TestFault => {
                log(console, pid, b"fault requested", None);
                // SAFETY: Only the smoke supervisor can route this diagnostic
                // event. UD2 intentionally exercises containment of this process.
                unsafe { asm!("ud2", options(noreturn, nostack)) }
            }
            Event::TestBusy => {
                self.busy = true;
                render::paint(self, handle(3));
                log(console, pid, b"busy start", None);
                busy_probe();
                self.busy = false;
                log(console, pid, b"busy done", None);
                changes.repaint = true;
            }
            Event::Key { .. } => {}
        }
        changes
    }
}

fn pointer_policy_checks(console: u64, pid: u64) {
    // Exercise real app event handling against a temporary document; these
    // events neither render nor change the application's visible state.
    let mut app = App::new(0);
    app.event(Event::Focus(true), console, pid);
    for &ascii in b"abc" {
        app.event(
            Event::Key {
                flags: 1,
                kind: 1,
                ascii,
            },
            console,
            pid,
        );
    }
    require(app.document.caret() == 3, 30);
    let held = Event::Pointer {
        x: 16,
        y: 72,
        buttons: 1,
    };
    let neutral = Event::Pointer {
        x: 16,
        y: 72,
        buttons: 0,
    };
    app.event(Event::Reset, console, pid);
    app.event(Event::Focus(true), console, pid);
    let changes = app.event(held, console, pid);
    require(app.document.caret() == 3 && !changes.repaint, 31);
    app.event(neutral, console, pid);
    let changes = app.event(held, console, pid);
    require(
        app.document.caret() == 0 && changes.repaint && app.document.text() == b"abc",
        32,
    );
    app.document.place(3);
    app.event(Event::Focus(false), console, pid);
    app.event(neutral, console, pid);
    require(app.clicks_armed, 33);
    app.event(Event::Focus(true), console, pid);
    app.event(held, console, pid);
    require(app.document.caret() == 3 && !app.clicks_armed, 34);
    app.event(neutral, console, pid);
    app.event(held, console, pid);
    require(app.document.caret() == 0, 35);
}

#[derive(Default)]
struct Changes {
    repaint: bool,
    text: bool,
    reset: bool,
}

impl Changes {
    fn merge(&mut self, other: Self) {
        self.repaint |= other.repaint;
        self.text |= other.text;
        self.reset |= other.reset;
    }
}

fn busy_probe() {
    let start = syscall(SYS_TICKS, 0, 0, 0) as u64;
    while (syscall(SYS_TICKS, 0, 0, 0) as u64).wrapping_sub(start) < 200 {
        // SAFETY: Finite scalar work with no memory accesses or hidden state;
        // the loop never yields or sleeps, so preemption belongs to the kernel.
        unsafe {
            asm!("2:", "dec rcx", "jnz 2b", inout("rcx") 100_000_u64 => _, options(nomem, nostack));
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn app_main(kind: u64) -> ! {
    require(kind <= 1, 1);
    let console = handle(0);
    let surface = handle(3);
    let input = handle(4);
    let pid = syscall(SYS_PID, 0, 0, 0);
    require(pid > 0, 19);
    let pid = pid as u64;
    check_authority(console, surface, input);
    pointer_policy_checks(console, pid);
    let mut app = App::new(kind);
    render::paint(&app, surface);
    log(
        console,
        pid,
        b"ready",
        Some(if kind == 0 { b"0" } else { b"1" }),
    );
    loop {
        let mut changes = Changes::default();
        match next_event(SYS_INPUT_WAIT, input) {
            Ok(event) => changes.merge(app.event(event, console, pid)),
            Err(ETIMEDOUT) => {}
            Err(_) => exit(20),
        }
        for _ in 1..16 {
            match next_event(SYS_INPUT_POLL, input) {
                Ok(event) => changes.merge(app.event(event, console, pid)),
                Err(EAGAIN) => break,
                Err(_) => exit(21),
            }
        }
        let now = syscall(SYS_TICKS, 0, 0, 0) as u64;
        if kind == 1 && now.wrapping_sub(app.last_counter_frame) >= 100 {
            app.elapsed = now.wrapping_sub(app.counter_start);
            changes.repaint = true;
        }
        if changes.repaint {
            if kind == 1 {
                app.elapsed = now.wrapping_sub(app.counter_start);
                app.last_counter_frame = now;
            }
            render::paint(&app, surface);
            if changes.text {
                log(console, pid, b"text", Some(app.document.text()));
            }
            if changes.reset {
                log(console, pid, b"reset", None);
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    exit(127)
}
