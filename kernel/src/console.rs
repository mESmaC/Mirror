//! Allocation-free diagnostic console, processed only in normal kernel context.
//!
//! The keyboard editor and typed callers share the same bounded dispatcher.
//! These commands inspect counters or change console text; they do not execute
//! programs, interpret shell syntax, or access files/storage.

use crate::keyboard::{Key, KeyEvent};

pub const LINE_CAPACITY: usize = 96;
const HISTORY_ROWS: usize = 12;
const EDIT_NOTICE: &str =
    "Enter runs a command. Escape clears input. Click or arrow keys move the caret.";
const LOST_NOTICE: &str = "Input lost. Current line discarded; type the command again.";

/// Owned, bounded printable ASCII. Construction rejects controls and truncation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Text {
    bytes: [u8; LINE_CAPACITY],
    length: usize,
}

impl Text {
    const EMPTY: Self = Self {
        bytes: [0; LINE_CAPACITY],
        length: 0,
    };

    pub fn new(text: &str) -> Option<Self> {
        if text.len() > LINE_CAPACITY || !text.bytes().all(|byte| (32..=126).contains(&byte)) {
            return None;
        }
        let mut result = Self::EMPTY;
        result.bytes[..text.len()].copy_from_slice(text.as_bytes());
        result.length = text.len();
        Some(result)
    }

    pub fn as_str(&self) -> &str {
        // Every constructor and editor insertion accepts printable ASCII only.
        core::str::from_utf8(&self.bytes[..self.length]).expect("console ASCII invariant")
    }

    fn counter(prefix: &str, mut value: u64) -> Self {
        let mut result = Self::new(prefix).expect("short static counter label");
        let mut digits = [b'0'; 20];
        let mut first = digits.len();
        loop {
            first -= 1;
            digits[first] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let end = result.length + digits.len() - first;
        assert!(end <= LINE_CAPACITY);
        result.bytes[result.length..end].copy_from_slice(&digits[first..]);
        result.length = end;
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Help,
    Status,
    Echo(Text),
    Clear,
    Unknown(Text),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Edited,
    Submitted { operation: &'static str, text: Text },
}

impl Action {
    /// Call outside IRQ handlers after processing a real queued input event.
    #[cfg(not(feature = "boot-smoke"))]
    pub fn log(&self) {
        if let Self::Submitted { operation, text } = self {
            crate::serial::println!(
                "MIRROR_CONSOLE_COMMAND operation={} text={}",
                operation,
                text.as_str()
            );
            crate::serial::println!("MIRROR_CONSOLE_INPUT irq={}", crate::keyboard::irq_count());
        }
    }
}

#[derive(Clone, Copy)]
pub struct HistoryLine {
    pub prompt: bool,
    pub text: Text,
}

impl HistoryLine {
    const EMPTY: Self = Self {
        prompt: false,
        text: Text::EMPTY,
    };
}

pub struct Console {
    input: Text,
    cursor: usize,
    history: [HistoryLine; HISTORY_ROWS],
    history_length: usize,
    notice: &'static str,
}

impl Console {
    pub fn new() -> Self {
        let mut console = Self {
            input: Text::EMPTY,
            cursor: 0,
            history: [HistoryLine::EMPTY; HISTORY_ROWS],
            history_length: 0,
            notice: EDIT_NOTICE,
        };
        console.output("Mirror kernel diagnostic console.");
        console.output("Type help for commands. No programs or file operations run here.");
        console
    }

    pub fn input(&self) -> &str {
        self.input.as_str()
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    /// Place the insertion point through the same bounded editor state used by
    /// keyboard navigation. Return true only if a valid index actually moves it.
    pub fn place_cursor(&mut self, index: usize) -> bool {
        if index > self.input.length || index == self.cursor {
            return false;
        }
        self.cursor = index;
        true
    }
    pub fn history(&self) -> &[HistoryLine] {
        &self.history[..self.history_length]
    }
    pub fn notice(&self) -> &'static str {
        self.notice
    }

    /// Discard incomplete input after a queue/decoder loss. Modifier recovery
    /// belongs to the keyboard driver; partial commands must not be submitted.
    pub fn input_lost(&mut self) {
        self.clear_input();
        self.output(LOST_NOTICE);
        self.notice = LOST_NOTICE;
    }

    /// Process one copied event outside IRQ context. A returned action means
    /// the display changed; only Submitted actions produce command log records.
    pub fn process(&mut self, event: KeyEvent) -> Option<Action> {
        if !event.pressed || event.modifiers.control || event.modifiers.alt {
            return None;
        }
        match event.key {
            Key::Enter => {
                if event.repeat {
                    return None;
                }
                let submitted = self.input;
                self.clear_input();
                let Some(operation) = parse(submitted) else {
                    return Some(Action::Edited);
                };
                self.push(HistoryLine {
                    prompt: true,
                    text: submitted,
                });
                Some(self.dispatch(operation))
            }
            Key::Escape => {
                self.clear_input();
                Some(Action::Edited)
            }
            Key::Backspace => {
                if self.cursor == 0 {
                    return None;
                }
                self.input
                    .bytes
                    .copy_within(self.cursor..self.input.length, self.cursor - 1);
                self.cursor -= 1;
                self.input.length -= 1;
                self.input.bytes[self.input.length] = 0;
                self.notice = EDIT_NOTICE;
                Some(Action::Edited)
            }
            Key::Delete => {
                if self.cursor == self.input.length {
                    return None;
                }
                self.input
                    .bytes
                    .copy_within(self.cursor + 1..self.input.length, self.cursor);
                self.input.length -= 1;
                self.input.bytes[self.input.length] = 0;
                self.notice = EDIT_NOTICE;
                Some(Action::Edited)
            }
            Key::Left => {
                if self.cursor == 0 {
                    return None;
                }
                self.cursor -= 1;
                Some(Action::Edited)
            }
            Key::Right => {
                if self.cursor == self.input.length {
                    return None;
                }
                self.cursor += 1;
                Some(Action::Edited)
            }
            Key::Home => {
                self.cursor = 0;
                Some(Action::Edited)
            }
            Key::End => {
                self.cursor = self.input.length;
                Some(Action::Edited)
            }
            Key::Character(_) => {
                let byte = event.ascii().filter(|byte| (32..=126).contains(byte))?;
                if self.input.length == LINE_CAPACITY {
                    self.notice = "Input limit: 96 characters. Backspace or Escape to continue.";
                    return Some(Action::Edited);
                }
                self.input
                    .bytes
                    .copy_within(self.cursor..self.input.length, self.cursor + 1);
                self.input.bytes[self.cursor] = byte;
                self.input.length += 1;
                self.cursor += 1;
                self.notice = EDIT_NOTICE;
                Some(Action::Edited)
            }
            _ => None,
        }
    }

    /// Shared typed command boundary. It accepts owned, validated text and
    /// changes only this console's state; Status reads actual live counters.
    pub fn dispatch(&mut self, operation: Operation) -> Action {
        self.notice = EDIT_NOTICE;
        let (name, text) = match operation {
            Operation::Help => {
                self.output("help       Show these commands.");
                self.output("status     Show live timer and keyboard IRQ counters.");
                self.output("echo TEXT  Print text without interpreting it.");
                self.output("clear      Clear console history and current input.");
                ("help", Text::EMPTY)
            }
            Operation::Status => {
                self.push(HistoryLine {
                    prompt: false,
                    text: Text::counter("Timer ticks: ", crate::timer::ticks()),
                });
                self.push(HistoryLine {
                    prompt: false,
                    text: Text::counter("Keyboard IRQs: ", crate::keyboard::irq_count()),
                });
                ("status", Text::EMPTY)
            }
            Operation::Echo(text) => {
                self.push(HistoryLine {
                    prompt: false,
                    text,
                });
                ("echo", text)
            }
            Operation::Clear => {
                self.history.fill(HistoryLine::EMPTY);
                self.history_length = 0;
                self.clear_input();
                ("clear", Text::EMPTY)
            }
            Operation::Unknown(text) => {
                self.output("Unknown command. Type help for available commands.");
                ("unknown", text)
            }
        };
        Action::Submitted {
            operation: name,
            text,
        }
    }

    fn clear_input(&mut self) {
        self.input = Text::EMPTY;
        self.cursor = 0;
        self.notice = EDIT_NOTICE;
    }

    fn output(&mut self, text: &str) {
        self.push(HistoryLine {
            prompt: false,
            text: Text::new(text).expect("short static console text"),
        });
    }

    fn push(&mut self, line: HistoryLine) {
        if self.history_length == HISTORY_ROWS {
            self.history.copy_within(1..HISTORY_ROWS, 0);
            self.history_length -= 1;
        }
        self.history[self.history_length] = line;
        self.history_length += 1;
    }
}

fn parse(line: Text) -> Option<Operation> {
    let text = line.as_str().trim_matches(' ');
    Some(match text {
        "" => return None,
        "help" => Operation::Help,
        "status" => Operation::Status,
        "clear" => Operation::Clear,
        "echo" => Operation::Echo(Text::EMPTY),
        _ => match text.strip_prefix("echo ") {
            Some(argument) => Operation::Echo(
                Text::new(argument.trim_start_matches(' ')).expect("bounded ASCII substring"),
            ),
            None => Operation::Unknown(line),
        },
    })
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    use crate::keyboard::Modifiers;
    fn key(key: Key) -> KeyEvent {
        KeyEvent {
            key,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        }
    }
    fn type_text(console: &mut Console, text: &str) {
        for byte in text.bytes() {
            let mut event = key(Key::Character(byte.to_ascii_lowercase()));
            event.modifiers.shift = byte.is_ascii_uppercase();
            assert_eq!(console.process(event), Some(Action::Edited));
        }
    }
    let mut console = Console::new();
    assert!(Text::new("\n").is_none());
    assert!(Text::new("\x1b").is_none());
    assert!(Text::new("\u{e9}").is_none());
    assert!(Text::new(core::str::from_utf8(&[b'x'; LINE_CAPACITY + 1]).unwrap()).is_none());
    assert_eq!(Text::counter("", u64::MAX).as_str(), "18446744073709551615");

    type_text(&mut console, "echo MirXor");
    console.process(key(Key::Home));
    assert_eq!(console.process(key(Key::Backspace)), None);
    for _ in 0..8 {
        console.process(key(Key::Right));
    }
    console.process(key(Key::Delete));
    type_text(&mut console, "r");
    console.process(key(Key::End));
    assert_eq!(console.process(key(Key::Delete)), None);
    console.process(key(Key::Backspace));
    type_text(&mut console, "r");
    assert_eq!(console.input(), "echo Mirror");
    assert!(!console.place_cursor(usize::MAX));
    assert!(!console.place_cursor(console.input().len() + 1));
    assert_eq!(console.cursor(), console.input().len());
    assert!(console.place_cursor(0));
    assert!(!console.place_cursor(0));
    assert!(console.place_cursor(console.input().len()));
    console.process(key(Key::Left));
    assert_eq!(console.cursor(), 10);
    console.process(key(Key::Right));
    assert_eq!(console.cursor(), 11);
    assert_eq!(console.process(key(Key::Right)), None);

    let mut ignored = key(Key::Enter);
    ignored.pressed = false;
    assert_eq!(console.process(ignored), None);
    ignored.key = Key::Character(b'z');
    assert_eq!(console.process(ignored), None);
    ignored.key = Key::Enter;
    ignored.pressed = true;
    ignored.repeat = true;
    assert_eq!(console.process(ignored), None);
    for command in [
        Key::Enter,
        Key::Escape,
        Key::Home,
        Key::Delete,
        Key::Character(b'z'),
    ] {
        for control in [false, true] {
            let mut event = key(command);
            event.modifiers.control = control;
            event.modifiers.alt = !control;
            assert_eq!(console.process(event), None);
        }
    }
    assert_eq!(console.input(), "echo Mirror");
    assert_eq!(console.cursor(), 11);
    assert_eq!(console.process(key(Key::Tab)), None);
    let Some(Action::Submitted { operation, text }) = console.process(key(Key::Enter)) else {
        panic!("enter did not submit");
    };
    assert_eq!(operation, "echo");
    assert_eq!(text.as_str(), "Mirror");
    assert_eq!(console.history().last().unwrap().text.as_str(), "Mirror");
    assert_eq!(console.input(), "");
    assert_eq!(console.cursor(), 0);

    for _ in 0..LINE_CAPACITY {
        console.process(key(Key::Character(b'x')));
    }
    let full = console.input;
    console.process(key(Key::Character(b'y')));
    assert_eq!(console.input, full);
    assert_eq!(console.cursor(), LINE_CAPACITY);
    assert!(console.notice().starts_with("Input limit:"));
    console.process(key(Key::Escape));
    assert_eq!(console.input(), "");
    assert_eq!(console.cursor(), 0);
    type_text(&mut console, "echo partial");
    console.input_lost();
    assert_eq!(console.input(), "");
    assert_eq!(console.cursor(), 0);
    assert_eq!(console.notice(), LOST_NOTICE);
    let count = console.history().len();
    assert_eq!(console.process(key(Key::Enter)), Some(Action::Edited));
    assert_eq!(console.history().len(), count);

    assert_eq!(parse(Text::new("  help  ").unwrap()), Some(Operation::Help));
    assert_eq!(
        parse(Text::new("echo").unwrap()),
        Some(Operation::Echo(Text::EMPTY))
    );
    assert!(matches!(
        parse(Text::new("echoes help").unwrap()),
        Some(Operation::Unknown(_))
    ));
    console.dispatch(Operation::Help);
    console.dispatch(Operation::Status);
    assert!(
        console
            .history()
            .last()
            .unwrap()
            .text
            .as_str()
            .starts_with("Keyboard IRQs: ")
    );
    assert!(matches!(
        console.dispatch(Operation::Unknown(Text::new("nope").unwrap())),
        Action::Submitted {
            operation: "unknown",
            ..
        }
    ));
    for _ in 0..HISTORY_ROWS + 4 {
        console.dispatch(Operation::Echo(Text::new("bounded").unwrap()));
    }
    assert_eq!(console.history().len(), HISTORY_ROWS);
    assert!(
        console
            .history()
            .iter()
            .all(|line| line.text.as_str() == "bounded")
    );
    type_text(&mut console, "discard me");
    console.dispatch(Operation::Clear);
    assert!(console.history().is_empty());
    assert_eq!(console.input(), "");
    assert_eq!(console.cursor(), 0);
    crate::serial::println!("MIRROR_CONSOLE_TESTS_OK");
}
