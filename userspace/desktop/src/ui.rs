//! Bounded application state and drawing policy for the native scratchpad.

use crate::render::{self, Rect, Renderer};
use crate::{Event, require};
use mirror_desktop_protocol::{Change, Editor, Operation, Request, Response, Snapshot};

const CAPACITY: usize = mirror_desktop_protocol::CAPACITY;
const CELL: usize = 12;
pub const EDIT: u8 = 1;
pub const OUTPUT: u8 = 2;
pub const POINTER: u8 = 4;
pub const SUBMIT: u8 = 8;
pub const EXIT: u8 = 16;
const NOTICE: u8 = 32;
pub const RUN_AGENT: u8 = 64;
const AGENT: u8 = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentStatus {
    Ready,
    Running,
    Complete,
    Conflict,
    Failed,
    Unavailable,
}

pub struct Layout {
    window: Rect,
    field: Rect,
    button: Rect,
    output: Rect,
    agent: Rect,
    text_x: usize,
    text_y: usize,
    visible: usize,
}

impl Layout {
    fn new(width: usize, height: usize) -> Self {
        let window_width = (width - 48).min(896);
        let window_height = (height - 96).min(424);
        let x = (width - window_width) / 2;
        let y = (height - window_height) / 2;
        let window = Rect::new(x, y, window_width, window_height);
        let field = Rect::new(x + 28, y + 104, window_width - 188, 44);
        Self {
            window,
            field,
            button: Rect::new(x + window_width - 148, y + 104, 120, 44),
            output: Rect::new(x + 28, y + 196, window_width - 56, window_height - 244),
            agent: Rect::new(width - 176, height - 44, 148, 30),
            text_x: field.x + 12,
            text_y: field.y + 15,
            visible: ((field.width - 24) / CELL).saturating_sub(1).max(1),
        }
    }
}

pub struct Ui {
    width: usize,
    height: usize,
    layout: Layout,
    editor: Editor,
    input_focused: bool,
    agent_focused: bool,
    agent_status: AgentStatus,
    notice: bool,
    pointer_x: usize,
    pointer_y: usize,
    buttons: u8,
    clicks_armed: bool,
}

impl Ui {
    pub fn new(width: usize, height: usize) -> Self {
        require(width >= 480 && height >= 360, 84);
        Self {
            width,
            height,
            layout: Layout::new(width, height),
            editor: Editor::new(),
            input_focused: true,
            agent_focused: false,
            agent_status: AgentStatus::Ready,
            notice: false,
            pointer_x: width / 2,
            pointer_y: height / 2,
            buttons: 0,
            clicks_armed: false,
        }
    }

    pub fn pointer(&self) -> (usize, usize, u8) {
        (self.pointer_x, self.pointer_y, self.buttons)
    }

    pub fn submitted(&self) -> &[u8] {
        self.editor.applied()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.editor.snapshot()
    }

    /// Agent requests enter the same editor dispatcher as keyboard and mouse
    /// operations. Transport authority and job lifetime belong to main.rs.
    pub fn dispatch_request(&mut self, request: Request) -> (Response, u8) {
        let (response, change) = self.editor.handle_request(request);
        (response, self.editor_changes(change))
    }

    pub fn set_agent_running(&mut self, running: bool) -> u8 {
        self.agent_status = if running {
            AgentStatus::Running
        } else {
            AgentStatus::Ready
        };
        AGENT
    }

    pub fn set_agent_result(&mut self, status: u8) -> u8 {
        self.agent_status = match status {
            0 => AgentStatus::Complete,
            1 => AgentStatus::Conflict,
            _ => AgentStatus::Failed,
        };
        AGENT
    }

    pub fn set_agent_unavailable(&mut self) -> u8 {
        self.agent_status = AgentStatus::Unavailable;
        AGENT
    }

    fn run_agent(&mut self) -> u8 {
        if matches!(
            self.agent_status,
            AgentStatus::Running | AgentStatus::Unavailable
        ) {
            return 0;
        }
        self.set_agent_running(true) | RUN_AGENT
    }

    fn editor_changes(&mut self, change: Change) -> u8 {
        let mut flags = 0;
        if change.edited {
            flags |= EDIT | NOTICE;
        }
        if change.applied {
            flags |= OUTPUT | SUBMIT | NOTICE;
        }
        if flags != 0 {
            self.notice = false;
        }
        flags
    }

    fn dispatch_human(&mut self, operation: Operation<'_>) -> u8 {
        let change = self.editor.dispatch(None, operation);
        require(change.is_ok(), 85);
        self.editor_changes(change.unwrap())
    }

    fn input_start(&self) -> usize {
        self.editor.caret().saturating_sub(self.layout.visible)
    }

    fn submit(&mut self) -> u8 {
        self.dispatch_human(Operation::Apply)
    }

    pub fn process(&mut self, event: Event) -> u8 {
        match event {
            Event::Lost => {
                self.dispatch_human(Operation::ClearDraft);
                self.buttons = 0;
                self.clicks_armed = false;
                self.notice = true;
                EDIT | NOTICE | POINTER
            }
            Event::Key { flags, kind, ascii } => {
                if flags & 1 == 0 || flags & (8 | 16) != 0 {
                    return 0;
                }
                if kind == 5 && flags & 2 == 0 {
                    return EXIT;
                }
                if kind == 4 && flags & 2 == 0 {
                    if self.input_focused {
                        self.input_focused = false;
                    } else if self.agent_focused {
                        self.agent_focused = false;
                        self.input_focused = true;
                    } else {
                        self.agent_focused = true;
                    }
                    return EDIT | AGENT;
                }
                if kind == 2 && flags & 2 == 0 {
                    return if self.agent_focused {
                        self.run_agent()
                    } else {
                        self.submit()
                    };
                }
                if !self.input_focused {
                    return if kind == 1 && ascii == b' ' && flags & 2 == 0 {
                        if self.agent_focused {
                            self.run_agent()
                        } else {
                            self.submit()
                        }
                    } else {
                        0
                    };
                }
                let caret = self.editor.caret();
                let length = self.editor.draft().len();
                match kind {
                    1 if (b' '..=b'~').contains(&ascii) && length < CAPACITY => self
                        .dispatch_human(Operation::Replace {
                            start: caret,
                            end: caret,
                            text: &[ascii],
                        }),
                    3 if caret != 0 => self.dispatch_human(Operation::Replace {
                        start: caret - 1,
                        end: caret,
                        text: &[],
                    }),
                    6 => self.dispatch_human(Operation::SetCaret(caret.saturating_sub(1))),
                    7 => self.dispatch_human(Operation::SetCaret((caret + 1).min(length))),
                    8 => self.dispatch_human(Operation::SetCaret(0)),
                    9 => self.dispatch_human(Operation::SetCaret(length)),
                    10 if caret < length => self.dispatch_human(Operation::Replace {
                        start: caret,
                        end: caret + 1,
                        text: &[],
                    }),
                    _ => 0,
                }
            }
            Event::Mouse { dx, dy, buttons } => {
                let old = self.pointer();
                self.pointer_x = self
                    .pointer_x
                    .saturating_add_signed(dx as isize)
                    .min(self.width - 1);
                self.pointer_y = self
                    .pointer_y
                    .saturating_add_signed(dy as isize)
                    .min(self.height - 1);
                let left_pressed = self.clicks_armed && self.buttons & 1 == 0 && buttons & 1 != 0;
                if buttons == 0 {
                    self.clicks_armed = true;
                }
                self.buttons = buttons;
                let mut change = if old != self.pointer() { POINTER } else { 0 };
                if left_pressed {
                    if self.layout.field.contains(self.pointer_x, self.pointer_y) {
                        let cell = self
                            .pointer_x
                            .saturating_sub(self.layout.text_x)
                            .saturating_add(CELL / 2)
                            / CELL;
                        let caret = (self.input_start() + cell.min(self.layout.visible))
                            .min(self.editor.draft().len());
                        change |= self.dispatch_human(Operation::SetCaret(caret));
                        self.input_focused = true;
                        self.agent_focused = false;
                        change |= EDIT | AGENT;
                    } else if self.layout.button.contains(self.pointer_x, self.pointer_y) {
                        self.input_focused = false;
                        self.agent_focused = false;
                        change |= EDIT | AGENT | self.submit();
                    } else if self.layout.agent.contains(self.pointer_x, self.pointer_y) {
                        self.input_focused = false;
                        self.agent_focused = true;
                        change |= EDIT | AGENT | self.run_agent();
                    }
                }
                change
            }
        }
    }

    pub fn paint_initial(&self, renderer: &mut Renderer) {
        renderer.fill(Rect::new(0, 0, self.width, self.height), render::DESKTOP);
        renderer.fill(Rect::new(0, 0, self.width, 48), render::TOPBAR);
        renderer.fill(Rect::new(0, 47, self.width, 1), render::BORDER);
        renderer.text(28, 17, b"MIRROR", 2, render::TEXT);
        renderer.text(self.width - 104, 21, b"DESKTOP", 1, render::ACCENT);
        // Keep the footer below the panel shadow, including the smallest layout.
        renderer.fill(Rect::new(28, self.height - 38, 4, 20), render::ACCENT_DARK);
        renderer.text(
            44,
            self.height - 32,
            b"Press Esc to return to the console.",
            1,
            render::MUTED,
        );
        let window = self.layout.window;
        renderer.fill(
            Rect::new(window.x + 8, window.y + 8, window.width, window.height),
            render::SHADOW,
        );
        renderer.fill(window, render::PANEL);
        renderer.border(window, render::BORDER);
        renderer.fill(
            Rect::new(window.x + 1, window.y + 1, window.width - 2, 63),
            render::RAISED,
        );
        renderer.text(window.x + 28, window.y + 23, b"Scratchpad", 3, render::TEXT);
        renderer.fill(
            Rect::new(window.x + window.width - 24, window.y + 28, 6, 6),
            render::ACCENT,
        );
        renderer.text(window.x + 28, window.y + 83, b"YOUR NOTE", 1, render::MUTED);
        renderer.text(
            window.x + 28,
            window.y + 178,
            b"APPLIED NOTE",
            1,
            render::MUTED,
        );
        self.paint_edit(renderer);
        self.paint_output(renderer);
        self.paint_notice(renderer);
        self.paint_agent(renderer);
    }

    pub fn paint_changes(&self, renderer: &mut Renderer, changes: u8) {
        if changes & EDIT != 0 {
            self.paint_edit(renderer);
        }
        if changes & OUTPUT != 0 {
            self.paint_output(renderer);
        }
        if changes & NOTICE != 0 {
            self.paint_notice(renderer);
        }
        if changes & POINTER != 0 {
            renderer.move_cursor(self.pointer_x, self.pointer_y);
        }
        if changes & AGENT != 0 {
            self.paint_agent(renderer);
        }
    }

    fn paint_edit(&self, renderer: &mut Renderer) {
        let field = self.layout.field;
        renderer.fill(field, render::FIELD);
        renderer.border(
            field,
            if self.input_focused {
                render::ACCENT
            } else {
                render::BORDER
            },
        );
        let start = self.input_start();
        let end = self.editor.draft().len().min(start + self.layout.visible);
        renderer.text(
            self.layout.text_x,
            self.layout.text_y,
            &self.editor.draft()[start..end],
            2,
            render::TEXT,
        );
        if self.editor.draft().is_empty() && !self.input_focused {
            renderer.text(
                self.layout.text_x,
                self.layout.text_y,
                b"Write a note",
                2,
                render::MUTED,
            );
        }
        if self.input_focused {
            renderer.fill(
                Rect::new(
                    self.layout.text_x + (self.editor.caret() - start) * CELL,
                    field.y + 12,
                    2,
                    20,
                ),
                render::ACCENT,
            );
        }
        let button = self.layout.button;
        renderer.fill(button, render::BUTTON);
        renderer.border(
            button,
            if !self.input_focused && !self.agent_focused {
                render::TEXT
            } else {
                render::BUTTON
            },
        );
        renderer.text(button.x + 30, button.y + 15, b"Apply", 2, render::DESKTOP);
    }

    fn paint_output(&self, renderer: &mut Renderer) {
        let output = self.layout.output;
        renderer.fill(output, render::TOPBAR);
        renderer.border(output, render::BORDER);
        let padding_y = if output.height < 38 { 3 } else { 12 };
        let columns = (output.width - 24) / CELL;
        let rows = output.height.saturating_sub(padding_y * 2) / 18;
        let bytes = if self.snapshot().has_applied {
            self.submitted()
        } else {
            b"Your applied note will appear here."
        };
        for (row, line) in bytes.chunks(columns).take(rows.max(1)).enumerate() {
            renderer.text(
                output.x + 12,
                output.y + padding_y + row * 18,
                line,
                2,
                if self.snapshot().has_applied {
                    render::TEXT
                } else {
                    render::MUTED
                },
            );
        }
    }

    fn paint_notice(&self, renderer: &mut Renderer) {
        let window = self.layout.window;
        renderer.fill(
            Rect::new(
                window.x + 28,
                window.y + window.height - 28,
                window.width - 56,
                12,
            ),
            render::PANEL,
        );
        let (notice, color) = if self.notice {
            (
                &b"Input lost. Release keys and try again."[..],
                render::WARNING,
            )
        } else {
            (
                &b"Session only. Run agent replaces your note. Tab changes focus."[..],
                render::MUTED,
            )
        };
        renderer.text(
            window.x + 28,
            window.y + window.height - 26,
            &notice[..notice.len().min((window.width - 56) / 6)],
            1,
            color,
        );
    }

    fn paint_agent(&self, renderer: &mut Renderer) {
        let button = self.layout.agent;
        let disabled = matches!(
            self.agent_status,
            AgentStatus::Running | AgentStatus::Unavailable
        );
        let label: &[u8] = match self.agent_status {
            AgentStatus::Ready => b"Run agent",
            AgentStatus::Running => b"Running",
            AgentStatus::Complete => b"Done - run",
            AgentStatus::Conflict => b"Conflict",
            AgentStatus::Failed => b"Failed",
            AgentStatus::Unavailable => b"Unavailable",
        };
        renderer.fill(
            button,
            if disabled {
                render::RAISED
            } else {
                render::BUTTON
            },
        );
        renderer.border(
            button,
            if self.agent_focused {
                render::TEXT
            } else {
                render::BORDER
            },
        );
        renderer.text(
            button.x + (button.width - label.len() * CELL) / 2,
            button.y + 8,
            label,
            2,
            if disabled {
                render::MUTED
            } else {
                render::DESKTOP
            },
        );
    }
}

pub fn self_test() {
    fn key(kind: u32, ascii: u8) -> Event {
        Event::Key {
            flags: 1,
            kind,
            ascii,
        }
    }
    let mut ui = Ui::new(1280, 800);
    for byte in b"abcd" {
        ui.process(key(1, *byte));
    }
    require(ui.editor.caret() == 4 && ui.editor.draft() == b"abcd", 90);
    ui.process(Event::Mouse {
        dx: -384,
        dy: -86,
        buttons: 0,
    });
    ui.process(Event::Mouse {
        dx: 0,
        dy: 0,
        buttons: 1,
    });
    require(ui.editor.caret() == 2, 91);
    ui.process(key(1, b'x'));
    require(ui.editor.draft() == b"abxcd", 92);
    ui.process(Event::Mouse {
        dx: 744,
        dy: 0,
        buttons: 0,
    });
    let submitted = ui.process(Event::Mouse {
        dx: 0,
        dy: 0,
        buttons: 1,
    });
    require(submitted & SUBMIT != 0 && ui.submitted() == b"abxcd", 93);
    require(
        ui.process(Event::Mouse {
            dx: 0,
            dy: 0,
            buttons: 1,
        }) & SUBMIT
            == 0,
        94,
    );
    ui.process(Event::Lost);
    require(
        ui.editor.draft().is_empty() && !ui.clicks_armed && ui.submitted() == b"abxcd",
        95,
    );
    require(
        ui.process(Event::Mouse {
            dx: 0,
            dy: 0,
            buttons: 1,
        }) & SUBMIT
            == 0,
        96,
    );
    ui.process(key(4, 0));
    ui.process(key(4, 0));
    for _ in 0..CAPACITY + 4 {
        ui.process(key(1, b'a'));
    }
    require(
        ui.editor.draft().len() == CAPACITY && ui.editor.caret() == CAPACITY,
        97,
    );
    ui.process(key(8, 0));
    ui.process(key(10, 0));
    require(
        ui.editor.draft().len() == CAPACITY - 1 && ui.editor.caret() == 0,
        98,
    );
    ui.process(key(9, 0));
    ui.process(key(3, 0));
    require(
        ui.editor.draft().len() == CAPACITY - 2 && ui.editor.caret() == CAPACITY - 2,
        99,
    );
    let old = ui.editor.draft().len();
    ui.process(Event::Key {
        flags: 1 | 8,
        kind: 1,
        ascii: b'z',
    });
    ui.process(Event::Key {
        flags: 0,
        kind: 1,
        ascii: b'z',
    });
    require(ui.editor.draft().len() == old, 100);
    ui.process(Event::Mouse {
        dx: i32::MAX,
        dy: i32::MIN,
        buttons: 0,
    });
    require(ui.pointer() == (1279, 0, 0), 101);
    require(ui.process(key(5, 0)) == EXIT, 102);

    // Real keyboard policy and typed IPC converge on the same semantic state.
    use mirror_desktop_protocol::{Chunk, Command, Status};
    let mut human = Ui::new(1280, 800);
    let mut agent = Ui::new(1280, 800);
    for byte in b"same note" {
        human.process(key(1, *byte));
    }
    human.process(key(2, 0));
    let (response, change) = agent.dispatch_request(Request {
        id: 1,
        command: Command::Replace {
            expected_revision: 0,
            start: 0,
            end: 0,
            text: Chunk::new(b"same note").unwrap(),
        },
    });
    require(response.status == Status::Ok && change & EDIT != 0, 103);
    let (response, change) = agent.dispatch_request(Request {
        id: 2,
        command: Command::Apply {
            expected_revision: response.snapshot.revision,
        },
    });
    require(response.status == Status::Ok && change & SUBMIT != 0, 104);
    require(
        human.editor.draft() == agent.editor.draft()
            && human.submitted() == agent.submitted()
            && human.editor.caret() == agent.editor.caret(),
        105,
    );
    let observed = agent.snapshot().revision;
    agent.process(key(1, b'!'));
    let before = agent.editor.clone();
    let (response, change) = agent.dispatch_request(Request {
        id: 3,
        command: Command::Apply {
            expected_revision: observed,
        },
    });
    require(
        response.status == Status::StaleRevision && change == 0 && agent.editor == before,
        106,
    );
    human.process(key(4, 0));
    human.process(key(4, 0));
    require(human.process(key(2, 0)) & RUN_AGENT != 0, 107);
    require(human.process(key(2, 0)) & RUN_AGENT == 0, 108);
    human.set_agent_result(0);
    require(human.process(key(1, b' ')) & RUN_AGENT != 0, 109);
    human.set_agent_unavailable();
    require(human.process(key(2, 0)) & RUN_AGENT == 0, 110);
}
