//! One pending response bounds memory and stops further requests when a client
//! stops receiving. Human input continues while the response waits for space.

use crate::{SYS_CLOSE, syscall, ui};
use mirror_desktop_protocol::{
    CONTROL_COMPLETE, CONTROL_CONFLICT, CONTROL_FAILED, CONTROL_RUN, Command, Request, Response,
    Status,
};

const SYS_SEND: u64 = 8;
const SYS_RECV: u64 = 9;

pub struct Service {
    send: u64,
    receive: u64,
    reply: Option<[u8; 64]>,
    available: bool,
    busy: bool,
    completed: Option<u8>,
}

impl Service {
    pub fn new(send: u64, receive: u64) -> Self {
        Self {
            send,
            receive,
            reply: None,
            available: true,
            busy: false,
            completed: None,
        }
    }

    pub fn completed(&self) -> Option<u8> {
        self.completed
    }

    pub fn available(&self) -> bool {
        self.available
    }

    pub fn start(&mut self, ui: &mut ui::Ui) -> u8 {
        if !self.available || self.busy {
            return 0;
        }
        if self.reply.is_some() {
            // Never put a job control ahead of a retained inspection response.
            return ui.set_agent_result(2);
        }
        match syscall(
            SYS_SEND,
            self.send,
            CONTROL_RUN.as_ptr() as u64,
            CONTROL_RUN.len() as u64,
        ) {
            8 => {
                self.busy = true;
                self.completed = None;
                ui.set_agent_running(true)
            }
            -11 => ui.set_agent_result(2),
            _ => self.disconnect(ui),
        }
    }

    fn dispatch(&self, ui: &mut ui::Ui, request: Request) -> (Response, u8) {
        // The exclusive channel identifies this client. An explicit task grant
        // permits mutation; an idle client can inspect but cannot change state.
        if !self.busy
            && matches!(
                request.command,
                Command::Replace { .. } | Command::Apply { .. }
            )
        {
            return (
                Response {
                    id: request.id,
                    opcode: request.command.opcode(),
                    status: Status::Denied,
                    snapshot: ui.snapshot(),
                    read: None,
                },
                0,
            );
        }
        ui.dispatch_request(request)
    }

    pub fn poll(&mut self, ui: &mut ui::Ui) -> (u8, bool) {
        if !self.available || self.reply.is_some() {
            return (0, false);
        }
        let mut bytes = [0u8; 64];
        let length = syscall(SYS_RECV, self.receive, bytes.as_mut_ptr() as u64, 64);
        if length == -11 {
            return (0, false);
        }
        if !(1..=64).contains(&length) {
            return (self.disconnect(ui), true);
        }
        let bytes = &bytes[..length as usize];
        if self.busy {
            let status = if bytes == CONTROL_COMPLETE {
                Some(0)
            } else if bytes == CONTROL_CONFLICT {
                Some(1)
            } else if bytes == CONTROL_FAILED {
                Some(2)
            } else {
                None
            };
            if let Some(status) = status {
                self.busy = false;
                self.completed = Some(status);
                return (ui.set_agent_result(status), true);
            }
        }
        let (response, changes) = match Request::decode(bytes) {
            Ok(request) => self.dispatch(ui, request),
            Err(_) => {
                let id = bytes
                    .get(4..8)
                    .map(|id| u32::from_le_bytes(id.try_into().unwrap()))
                    .unwrap_or(0);
                (Response::malformed(id, ui.snapshot()), 0)
            }
        };
        self.reply = Some(response.encode().expect("valid desktop response"));
        (changes, true)
    }

    /// Call after painting so replies acknowledge completed visible transport.
    pub fn flush_reply(&mut self, ui: &mut ui::Ui) -> u8 {
        let Some(bytes) = &self.reply else {
            return 0;
        };
        match syscall(SYS_SEND, self.send, bytes.as_ptr() as u64, 64) {
            64 => {
                self.reply = None;
                0
            }
            -11 => 0,
            _ => self.disconnect(ui),
        }
    }

    fn disconnect(&mut self, ui: &mut ui::Ui) -> u8 {
        self.available = false;
        self.busy = false;
        self.reply = None;
        syscall(SYS_CLOSE, self.send, 0, 0);
        syscall(SYS_CLOSE, self.receive, 0, 0);
        ui.set_agent_unavailable()
    }

    pub fn check_idle_authority(&self, ui: &mut ui::Ui) {
        let before = ui.snapshot();
        for command in [
            Command::Apply {
                expected_revision: before.revision,
            },
            Command::Replace {
                expected_revision: before.revision,
                start: 0,
                end: before.draft_length,
                text: mirror_desktop_protocol::Chunk::new(b"denied").unwrap(),
            },
        ] {
            let (response, changes) = self.dispatch(ui, Request { id: 1, command });
            crate::require(
                response.status == Status::Denied && response.snapshot == before && changes == 0,
                60,
            );
        }
    }
}
