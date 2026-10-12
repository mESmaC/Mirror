//! A native, deterministic agent client. Its only desktop authority is the
//! request channel; it cannot draw pixels or consume the user's input stream.
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use mirror_desktop_protocol::{
    CONTROL_COMPLETE, CONTROL_CONFLICT, CONTROL_FAILED, CONTROL_RUN, Chunk, Command, Request,
    Response, Status, TextTarget,
};

const _: &str = include_str!("../linker.ld");
const NOTE: &[u8] = b"Hello from Mirror agent";
const SYS_WRITE: u64 = 1;
const SYS_EXIT: u64 = 3;
const SYS_SLEEP: u64 = 5;
const SYS_BOOT_HANDLE: u64 = 7;
const SYS_SEND: u64 = 8;
const SYS_RECV_WAIT: u64 = 10;
const SYS_SURFACE_INFO: u64 = 13;
const SYS_INPUT_POLL: u64 = 16;
const ETIMEDOUT: i64 = -110;
// Each RECV_WAIT waits at most 100 ticks. Give a slow demo desktop up to ten
// receive windows without resending a request whose outcome is still unknown.
const REPLY_WAIT_WINDOWS: usize = 10;

global_asm!(
    ".section .text.entry,\"ax\"",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "xor rbp, rbp",
    "and rsp, -16",
    "call agent_main",
    "ud2",
    ".size _start, .-_start",
    ".previous",
);

fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    // SAFETY: Mirror preserves all general registers except result RAX and
    // restores the user stack. Memory operands go through checked user copies.
    unsafe {
        asm!("int 0x80", inout("rax") number => result, in("rdi") arg0,
            in("rsi") arg1, in("rdx") arg2, options(nostack));
    }
    result
}

fn exit(status: u64) -> ! {
    syscall(SYS_EXIT, status, 0, 0);
    // SAFETY: EXIT must not return; trap deterministically on ABI failure.
    unsafe { asm!("ud2", options(noreturn, nostack)) }
}

fn require(condition: bool, status: u64) {
    if !condition {
        exit(status);
    }
}

fn write(console: u64, bytes: &[u8]) {
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

fn handle(slot: u64) -> u64 {
    let value = syscall(SYS_BOOT_HANDLE, slot, 0, 0);
    require(value > 0, 5);
    value as u64
}

struct Client {
    send: u64,
    receive: u64,
    next_id: u32,
}

impl Client {
    fn send(&self, bytes: &[u8]) -> bool {
        // Retry only a rejected enqueue, never a request whose outcome is
        // unknown. The channel's bounded queue cannot partially accept a send.
        for _ in 0..100 {
            match syscall(
                SYS_SEND,
                self.send,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
            ) {
                value if value == bytes.len() as i64 => return true,
                -11 => {
                    syscall(SYS_SLEEP, 1, 0, 0);
                }
                _ => return false,
            }
        }
        false
    }

    fn receive(&self, bytes: &mut [u8; 64]) -> i64 {
        for _ in 0..REPLY_WAIT_WINDOWS {
            let result = syscall(SYS_RECV_WAIT, self.receive, bytes.as_mut_ptr() as u64, 64);
            if result != ETIMEDOUT {
                return result;
            }
        }
        ETIMEDOUT
    }

    fn call(&mut self, command: Command) -> Response {
        let request = Request {
            id: self.next_id,
            command,
        };
        self.next_id = self.next_id.checked_add(1).unwrap_or_else(|| exit(6));
        let bytes = request.encode().unwrap_or_else(|_| exit(7));
        require(self.send(&bytes), 8);
        let mut bytes = [0u8; 64];
        // Exhausting the receive windows is fatal for this client session:
        // resending a mutation could apply it twice. The desktop stays usable.
        require(self.receive(&mut bytes) == 64, 9);
        let response = Response::decode(&bytes).unwrap_or_else(|_| exit(10));
        require(
            response.id == request.id && response.opcode == request.command.opcode(),
            11,
        );
        response
    }

    fn malformed(&self, bytes: &[u8], revision: u64) {
        require(self.send(bytes), 12);
        let mut reply = [0u8; 64];
        require(self.receive(&mut reply) == 64, 13);
        let response = Response::decode(&reply).unwrap_or_else(|_| exit(14));
        require(
            response.status == Status::Malformed && response.snapshot.revision == revision,
            15,
        );
    }

    fn backpressure(&mut self) {
        let revision = self.call(Command::Inspect).snapshot.revision;
        let first = self.next_id;
        // More replies than the four-slot queue can hold. The desktop must
        // retain its pending fifth response while this client stops receiving.
        for index in 0..8 {
            let request = Request {
                id: self.next_id,
                command: if index == 4 {
                    Command::Apply {
                        expected_revision: revision,
                    }
                } else {
                    Command::Inspect
                },
            };
            self.next_id += 1;
            require(self.send(&request.encode().unwrap()), 18);
        }
        syscall(SYS_SLEEP, 8, 0, 0);
        for id in first..self.next_id {
            let mut bytes = [0u8; 64];
            require(self.receive(&mut bytes) == 64, 19);
            let response = Response::decode(&bytes).unwrap_or_else(|_| exit(27));
            require(
                response.id == id
                    && response.status == Status::Ok
                    && response.snapshot.revision == revision + u64::from(id - first >= 4),
                28,
            );
        }
    }
}

fn chunk(bytes: &[u8]) -> Chunk {
    Chunk::new(bytes).unwrap_or_else(|_| exit(17))
}

fn authority_checks(console: u64, client: &Client) {
    let mut buffer = [0xa5u8; 32];
    require(syscall(SYS_BOOT_HANDLE, 3, 0, 0) == -9, 20);
    require(syscall(SYS_BOOT_HANDLE, 4, 0, 0) == -9, 21);
    require(
        syscall(SYS_SURFACE_INFO, console, buffer.as_mut_ptr() as u64, 24) == -13,
        22,
    );
    require(
        syscall(
            SYS_INPUT_POLL,
            client.receive,
            buffer.as_mut_ptr() as u64,
            32,
        ) == -13,
        23,
    );
    require(
        syscall(SYS_SEND, client.receive, buffer.as_ptr() as u64, 8) == -13,
        24,
    );
    require(
        syscall(SYS_RECV_WAIT, client.send, buffer.as_mut_ptr() as u64, 32) == -13,
        25,
    );
    require(buffer == [0xa5; 32], 26);
}

fn checks(client: &mut Client) {
    client.backpressure();
    let first = client.call(Command::Discover);
    require(
        first.status == Status::Ok && first.snapshot.draft_length == 0,
        30,
    );
    let revision = first.snapshot.revision;
    client.malformed(&[0u8; 64], revision);
    let mut bad = Request {
        id: 0,
        command: Command::Inspect,
    }
    .encode()
    .unwrap();
    bad[1] = 127;
    client.malformed(&bad, revision);
    client.malformed(&bad[..63], revision);
    let response = client.call(Command::Read {
        target: TextTarget::Draft,
        offset: 1,
        length: 1,
    });
    require(
        response.status == Status::InvalidRange && response.snapshot.revision == revision,
        31,
    );
    let mut revision = revision;
    for start in [0u8, 32, 64] {
        let response = client.call(Command::Replace {
            expected_revision: revision,
            start,
            end: start,
            text: chunk(&[b'a'; 32]),
        });
        require(response.status == Status::Ok, 32);
        revision = response.snapshot.revision;
    }
    let full = client.call(Command::Inspect);
    require(full.snapshot.draft_length == 96, 33);
    let response = client.call(Command::Replace {
        expected_revision: revision,
        start: 96,
        end: 96,
        text: chunk(b"x"),
    });
    require(
        response.status == Status::CapacityExceeded && response.snapshot == full.snapshot,
        34,
    );
    let response = client.call(Command::Replace {
        expected_revision: revision - 1,
        start: 0,
        end: 96,
        text: chunk(b"stale"),
    });
    require(
        response.status == Status::StaleRevision && response.snapshot == full.snapshot,
        35,
    );
    let response = client.call(Command::Apply {
        expected_revision: revision - 1,
    });
    require(
        response.status == Status::StaleRevision && response.snapshot == full.snapshot,
        36,
    );
    let mut bad = Request {
        id: 0,
        command: Command::Replace {
            expected_revision: revision,
            start: 0,
            end: 1,
            text: chunk(b"x"),
        },
    }
    .encode()
    .unwrap();
    bad[32] = 0x1f;
    client.malformed(&bad, revision);
    for offset in [0u8, 32, 64] {
        let response = client.call(Command::Read {
            target: TextTarget::Draft,
            offset,
            length: 32,
        });
        require(
            response.status == Status::Ok && response.read.unwrap().text.as_bytes() == [b'a'; 32],
            37,
        );
    }
}

fn run_job(client: &mut Client) -> u8 {
    let discovered = client.call(Command::Discover);
    require(discovered.status == Status::Ok, 40);
    let state = client.call(Command::Inspect).snapshot;
    let replaced = client.call(Command::Replace {
        expected_revision: state.revision,
        start: 0,
        end: state.draft_length,
        text: chunk(NOTE),
    });
    if replaced.status == Status::StaleRevision {
        return 1;
    }
    if replaced.status != Status::Ok {
        return 2;
    }
    let applied = client.call(Command::Apply {
        expected_revision: replaced.snapshot.revision,
    });
    if applied.status == Status::StaleRevision {
        return 1;
    }
    if applied.status != Status::Ok {
        return 2;
    }
    let read = client.call(Command::Read {
        target: TextTarget::Applied,
        offset: 0,
        length: NOTE.len() as u8,
    });
    if read.snapshot.revision != applied.snapshot.revision {
        return 1;
    }
    if read.status != Status::Ok || read.read.unwrap().text.as_bytes() != NOTE {
        return 2;
    }
    0
}

#[unsafe(no_mangle)]
extern "C" fn agent_main(mode: u64) -> ! {
    require(mode <= 1, 1);
    let console = handle(0);
    let mut client = Client {
        send: handle(1),
        receive: handle(2),
        next_id: 1,
    };
    authority_checks(console, &client);
    write(console, b"Mirror agent: ready\n");
    loop {
        let mut bytes = [0u8; 64];
        match client.receive(&mut bytes) {
            ETIMEDOUT => continue,
            -32 => exit(0),
            8 if bytes[..8] == CONTROL_RUN => {}
            _ => exit(41),
        }
        if mode == 1 {
            checks(&mut client);
        }
        let result = run_job(&mut client);
        let control = match result {
            0 => &CONTROL_COMPLETE,
            1 => &CONTROL_CONFLICT,
            _ => &CONTROL_FAILED,
        };
        if mode == 1 {
            require(result == 0, 42);
        }
        require(client.send(control), 43);
        if mode == 1 {
            let before = client.call(Command::Inspect).snapshot;
            for command in [
                Command::Apply {
                    expected_revision: before.revision,
                },
                Command::Replace {
                    expected_revision: before.revision,
                    start: 0,
                    end: before.draft_length,
                    text: chunk(b"must be denied"),
                },
            ] {
                let response = client.call(command);
                require(
                    response.status == Status::Denied && response.snapshot == before,
                    44,
                );
            }
            write(console, b"Mirror agent: checks passed\n");
        }
        match result {
            0 => write(console, b"Mirror agent: task complete\n"),
            1 => write(console, b"Mirror agent: edit conflict\n"),
            _ => write(console, b"Mirror agent: task failed\n"),
        }
        if mode == 1 {
            exit(0);
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    exit(127)
}
