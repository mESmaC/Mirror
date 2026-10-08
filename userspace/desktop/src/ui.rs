//! Bounded application state and drawing policy for the native scratchpad.

use crate::render::{self, Rect, Renderer};
use crate::{Event, require};

const CAPACITY: usize = 96;
const CELL: usize = 12;
pub const EDIT: u8 = 1;
pub const OUTPUT: u8 = 2;
pub const POINTER: u8 = 4;
pub const SUBMIT: u8 = 8;
pub const EXIT: u8 = 16;
const NOTICE: u8 = 32;

pub struct Layout {
    window: Rect,
    field: Rect,
    button: Rect,
    output: Rect,
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
    line: [u8; CAPACITY],
    length: usize,
    caret: usize,
    submitted: [u8; CAPACITY],
    submitted_length: usize,
    has_submission: bool,
    input_focused: bool,
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
            line: [0; CAPACITY],
            length: 0,
            caret: 0,
            submitted: [0; CAPACITY],
            submitted_length: 0,
            has_submission: false,
            input_focused: true,
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
        &self.submitted[..self.submitted_length]
    }

    fn input_start(&self) -> usize {
        self.caret.saturating_sub(self.layout.visible)
    }

    fn submit(&mut self) -> u8 {
        self.submitted = self.line;
        self.submitted_length = self.length;
        self.has_submission = true;
        self.notice = false;
        OUTPUT | SUBMIT | NOTICE
    }

    pub fn process(&mut self, event: Event) -> u8 {
        match event {
            Event::Lost => {
                self.line.fill(0);
                self.length = 0;
                self.caret = 0;
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
                    self.input_focused = !self.input_focused;
                    return EDIT;
                }
                if kind == 2 && flags & 2 == 0 {
                    return self.submit();
                }
                if !self.input_focused {
                    return if kind == 1 && ascii == b' ' && flags & 2 == 0 {
                        self.submit()
                    } else {
                        0
                    };
                }
                let before = (self.caret, self.length);
                match kind {
                    1 if (b' '..=b'~').contains(&ascii) && self.length < CAPACITY => {
                        self.line
                            .copy_within(self.caret..self.length, self.caret + 1);
                        self.line[self.caret] = ascii;
                        self.caret += 1;
                        self.length += 1;
                    }
                    3 if self.caret != 0 => {
                        self.line
                            .copy_within(self.caret..self.length, self.caret - 1);
                        self.caret -= 1;
                        self.length -= 1;
                        self.line[self.length] = 0;
                    }
                    6 => self.caret = self.caret.saturating_sub(1),
                    7 => self.caret = (self.caret + 1).min(self.length),
                    8 => self.caret = 0,
                    9 => self.caret = self.length,
                    10 if self.caret < self.length => {
                        self.line
                            .copy_within(self.caret + 1..self.length, self.caret);
                        self.length -= 1;
                        self.line[self.length] = 0;
                    }
                    _ => {}
                }
                if before != (self.caret, self.length) {
                    self.notice = false;
                    EDIT | NOTICE
                } else {
                    0
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
                        self.caret =
                            (self.input_start() + cell.min(self.layout.visible)).min(self.length);
                        self.input_focused = true;
                        change |= EDIT;
                    } else if self.layout.button.contains(self.pointer_x, self.pointer_y) {
                        self.input_focused = false;
                        change |= EDIT | self.submit();
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
        let end = self.length.min(start + self.layout.visible);
        renderer.text(
            self.layout.text_x,
            self.layout.text_y,
            &self.line[start..end],
            2,
            render::TEXT,
        );
        if self.length == 0 && !self.input_focused {
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
                    self.layout.text_x + (self.caret - start) * CELL,
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
            if self.input_focused {
                render::BUTTON
            } else {
                render::TEXT
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
        let bytes = if self.has_submission {
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
                if self.has_submission {
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
                &b"This note stays in this session. Tab changes focus."[..],
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
    require(ui.caret == 4 && &ui.line[..ui.length] == b"abcd", 90);
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
    require(ui.caret == 2, 91);
    ui.process(key(1, b'x'));
    require(&ui.line[..ui.length] == b"abxcd", 92);
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
        ui.length == 0 && !ui.clicks_armed && ui.submitted() == b"abxcd",
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
    for _ in 0..CAPACITY + 4 {
        ui.process(key(1, b'a'));
    }
    require(ui.length == CAPACITY && ui.caret == CAPACITY, 97);
    ui.process(key(8, 0));
    ui.process(key(10, 0));
    require(ui.length == CAPACITY - 1 && ui.caret == 0, 98);
    ui.process(key(9, 0));
    ui.process(key(3, 0));
    require(ui.length == CAPACITY - 2 && ui.caret == CAPACITY - 2, 99);
    let old = ui.length;
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
    require(ui.length == old, 100);
    ui.process(Event::Mouse {
        dx: i32::MAX,
        dy: i32::MIN,
        buttons: 0,
    });
    require(ui.pointer() == (1279, 0, 0), 101);
    require(ui.process(key(5, 0)) == EXIT, 102);
}
