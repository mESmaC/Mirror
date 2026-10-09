//! Allocation-free diagnostic console, processed only in normal kernel context.
//!
//! The keyboard editor and typed callers share the same bounded dispatcher.
//! These commands inspect counters or change console text; they do not execute
//! programs, interpret shell syntax, or access files/storage.

use core::fmt::{self, Write};

use crate::{
    drivers,
    keyboard::{Key, KeyEvent},
    pci,
};

pub const LINE_CAPACITY: usize = 96;
const HISTORY_ROWS: usize = 12;
const DEVICES_PER_PAGE: usize = 4;
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

    fn formatted(arguments: fmt::Arguments<'_>) -> Self {
        let mut text = Self::EMPTY;
        text.write_fmt(arguments).expect("bounded console format");
        text
    }
}

impl Write for Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > LINE_CAPACITY - self.length
            || !text.bytes().all(|byte| (32..=126).contains(&byte))
        {
            return Err(fmt::Error);
        }
        let end = self.length + text.len();
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Help,
    Status,
    /// Zero-based page. The textual command presents pages starting at one.
    Devices(u8),
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

pub struct Console<'a> {
    input: Text,
    cursor: usize,
    history: [HistoryLine; HISTORY_ROWS],
    history_length: usize,
    notice: &'static str,
    devices: Option<&'a pci::Inventory>,
}

impl<'a> Console<'a> {
    pub fn new() -> Self {
        let mut console = Self {
            input: Text::EMPTY,
            cursor: 0,
            history: [HistoryLine::EMPTY; HISTORY_ROWS],
            history_length: 0,
            notice: EDIT_NOTICE,
            devices: None,
        };
        console.output("Mirror kernel diagnostic console.");
        console.output("Type help for commands. No programs or file operations run here.");
        console
    }

    pub(crate) fn with_devices(devices: &'a pci::Inventory) -> Self {
        let mut console = Self::new();
        console.devices = Some(devices);
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
                self.output("devices N  Show PCI IDs and driver candidates; first page is devices or devices 1.");
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
            Operation::Devices(page) => {
                self.device_page(page);
                ("devices", Text::counter("page=", u64::from(page) + 1))
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

    fn device_page(&mut self, page: u8) {
        let Some(inventory) = self.devices else {
            self.output("PCI inventory unavailable in this console.");
            return;
        };
        let summary = drivers::summary(inventory);
        let pages = summary.retained.div_ceil(DEVICES_PER_PAGE).max(1);
        if usize::from(page) >= pages {
            self.output_formatted(format_args!(
                "Device page out of range. Use devices 1 through devices {}.",
                pages
            ));
            return;
        }
        self.output_formatted(format_args!(
            "PCI devices: page {}/{}; candidates are matches, not active drivers.",
            usize::from(page) + 1,
            pages
        ));
        self.output_formatted(format_args!(
            "Retained: {}  Matched: {}  Unsupported: {}  Omitted: {}",
            summary.retained, summary.matched, summary.unsupported, summary.omitted
        ));
        for device in drivers::devices(inventory)
            .skip(usize::from(page) * DEVICES_PER_PAGE)
            .take(DEVICES_PER_PAGE)
        {
            self.output_formatted(format_args!(
                "{:02x}:{:02x}.{} id={:04x}:{:04x} class={:02x}:{:02x}:{:02x}",
                device.bdf.bus,
                device.bdf.slot,
                device.bdf.function,
                device.vendor,
                device.device,
                device.class,
                device.subclass,
                device.interface
            ));
            match drivers::candidate(device) {
                Some(candidate) => self.output_formatted(format_args!(
                    "  Candidate: {} ({} match)",
                    candidate.name(),
                    candidate.match_kind()
                )),
                None => self.output("  Unsupported: no native driver candidate."),
            }
        }
        if summary.retained == 0 {
            self.output("No retained PCI devices.");
        }
        self.output("Use devices N for page N. IDs remain visible when no driver matches.");
    }

    fn output_formatted(&mut self, arguments: fmt::Arguments<'_>) {
        self.push(HistoryLine {
            prompt: false,
            text: Text::formatted(arguments),
        });
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
        "devices" => Operation::Devices(0),
        "clear" => Operation::Clear,
        "echo" => Operation::Echo(Text::EMPTY),
        _ => match text.strip_prefix("echo ") {
            Some(argument) => Operation::Echo(
                Text::new(argument.trim_start_matches(' ')).expect("bounded ASCII substring"),
            ),
            None => match text.strip_prefix("devices ") {
                Some(argument) => {
                    let page = argument.trim_start_matches(' ');
                    page.bytes()
                        .all(|byte| byte.is_ascii_digit())
                        .then(|| page.parse::<u8>().ok())
                        .flatten()
                        .filter(|page| *page > 0)
                        .map_or(Operation::Unknown(line), |page| {
                            Operation::Devices(page - 1)
                        })
                }
                None => Operation::Unknown(line),
            },
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
    let mut bounded = Text::new("unchanged").unwrap();
    assert_eq!(bounded.write_str("\n"), Err(fmt::Error));
    assert_eq!(bounded.as_str(), "unchanged");
    assert_eq!(
        bounded.write_str(core::str::from_utf8(&[b'x'; LINE_CAPACITY]).unwrap()),
        Err(fmt::Error)
    );
    assert_eq!(bounded.as_str(), "unchanged");

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
    for (command, page) in [
        ("devices", 0),
        ("devices 1", 0),
        ("  devices  2  ", 1),
        ("devices 255", 254),
    ] {
        assert_eq!(
            parse(Text::new(command).unwrap()),
            Some(Operation::Devices(page))
        );
    }
    for command in [
        "devices 0",
        "devices 256",
        "devices +1",
        "devices -1",
        "devices 1 2",
        "devices one",
        "devices1",
        "devices 9999999999999999999999999999999",
    ] {
        assert!(matches!(
            parse(Text::new(command).unwrap()),
            Some(Operation::Unknown(_))
        ));
    }
    console.dispatch(Operation::Devices(0));
    assert_eq!(
        console.history().last().unwrap().text.as_str(),
        "PCI inventory unavailable in this console."
    );
    let inventory = drivers::test_inventory();
    let mut device_console = Console::with_devices(&inventory);
    device_console.dispatch(Operation::Clear);
    assert_eq!(
        device_console.dispatch(Operation::Devices(0)),
        Action::Submitted {
            operation: "devices",
            text: Text::new("page=1").unwrap(),
        }
    );
    assert_eq!(device_console.history().len(), 11);
    assert!(
        device_console.history()[0]
            .text
            .as_str()
            .contains("page 1/2")
    );
    assert_eq!(
        device_console.history()[1].text.as_str(),
        "Retained: 5  Matched: 2  Unsupported: 3  Omitted: 3"
    );
    assert!(
        device_console
            .history()
            .iter()
            .any(|line| { line.text.as_str() == "  Candidate: virtio-blk-legacy (exact match)" })
    );
    assert!(
        device_console
            .history()
            .iter()
            .any(|line| { line.text.as_str() == "  Candidate: ahci-sata (class match)" })
    );
    assert!(
        device_console
            .history()
            .iter()
            .any(|line| { line.text.as_str().contains("id=1af4:1042") })
    );
    assert_eq!(
        device_console
            .history()
            .iter()
            .filter(|line| { line.text.as_str() == "  Unsupported: no native driver candidate." })
            .count(),
        2
    );
    device_console.dispatch(Operation::Clear);
    device_console.dispatch(Operation::Devices(1));
    assert_eq!(device_console.history().len(), 5);
    assert!(
        device_console.history()[0]
            .text
            .as_str()
            .contains("page 2/2")
    );
    assert!(
        device_console.history()[2]
            .text
            .as_str()
            .contains("id=abcd:9876")
    );
    assert_eq!(
        device_console.history()[3].text.as_str(),
        "  Unsupported: no native driver candidate."
    );
    for page in [2, u8::MAX] {
        device_console.dispatch(Operation::Devices(page));
        assert_eq!(
            device_console.history().last().unwrap().text.as_str(),
            "Device page out of range. Use devices 1 through devices 2."
        );
    }
    let mut empty = drivers::test_inventory();
    empty.devices.fill(None);
    empty.retained = 0;
    empty.omitted = 0;
    let mut empty_console = Console::with_devices(&empty);
    empty_console.dispatch(Operation::Devices(0));
    assert!(
        empty_console
            .history()
            .iter()
            .any(|line| { line.text.as_str() == "No retained PCI devices." })
    );
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
