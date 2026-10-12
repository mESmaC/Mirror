#![no_std]
#![forbid(unsafe_code)]
//! Opt-in performance configuration and the closed 0019a serial protocol.
//!
//! Parsing and validation allocate nothing. Display produces the canonical
//! wire representation; parsing accepts only that field order and vocabulary.

use core::fmt;

pub const TOKEN_PREFIX: &str = "mirror.perf=";
pub const MAX_TOKEN_BYTES: usize = 64;
pub const MEMORY_INTERVAL_NS: u64 = 10_000_000_000;
/// More than the longest possible canonical 0019a record.
pub const MAX_RECORD_BYTES: usize = 192;

/// ASCII whitespace includes horizontal/vertical tab, LF, FF, CR and SPACE.
/// Keep this explicit: Rust's `is_ascii_whitespace` excludes vertical tab.
pub const fn is_ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t'..=b'\r')
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub mem: bool,
    pub input: bool,
    pub burn: bool,
    pub flip: bool,
    pub telemetry: bool,
    pub agent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NonAscii,
    TooLong,
    DuplicateToken,
    EmptyItem,
    UnknownItem,
    DuplicateItem,
    ConflictingItems,
    BurnWithoutInput,
    Schema,
    Number,
    Overflow,
    UnexpectedRecord,
    Order,
    Flags,
    Timestamp,
    MemoryBounds,
    MemorySpacing,
    MissingRecord,
}

/// Find the exact, case-sensitive token among ASCII-whitespace-separated boot
/// arguments. Unrelated arguments remain the boot policy parser's concern.
pub fn parse_command_line(command_line: &[u8]) -> Result<Option<Config>, Error> {
    let mut found = None;
    for token in command_line.split(|&byte| is_ascii_whitespace(byte)) {
        if !token.starts_with(TOKEN_PREFIX.as_bytes()) {
            continue;
        }
        if found.is_some() {
            return Err(Error::DuplicateToken);
        }
        if token.len() > MAX_TOKEN_BYTES {
            return Err(Error::TooLong);
        }
        if !token.is_ascii() {
            return Err(Error::NonAscii);
        }
        let items =
            core::str::from_utf8(&token[TOKEN_PREFIX.len()..]).map_err(|_| Error::NonAscii)?;
        found = Some(parse_items(items)?);
    }
    Ok(found)
}

/// Parse the public item list (without `mirror.perf=`). Every successful
/// configuration enables milestones; `1` additionally requests memory/input.
pub fn parse_items(items: &str) -> Result<Config, Error> {
    if items.len() > MAX_TOKEN_BYTES - TOKEN_PREFIX.len() {
        return Err(Error::TooLong);
    }
    if !items.is_ascii() {
        return Err(Error::NonAscii);
    }
    let mut seen = 0u8;
    for item in items.split(',') {
        let bit = match item {
            "boot" => 1,
            "1" => 2,
            "mem" => 4,
            "burn" => 8,
            "flip" => 16,
            "telemetry" => 32,
            "agent" => 64,
            "" => return Err(Error::EmptyItem),
            _ => return Err(Error::UnknownItem),
        };
        if seen & bit != 0 {
            return Err(Error::DuplicateItem);
        }
        seen |= bit;
    }
    if seen & 6 == 6 {
        return Err(Error::ConflictingItems);
    }
    if seen & 8 != 0 && seen & 2 == 0 {
        return Err(Error::BurnWithoutInput);
    }
    Ok(Config {
        mem: seen & 6 != 0,
        input: seen & 2 != 0,
        burn: seen & 8 != 0,
        flip: seen & 16 != 0,
        telemetry: seen & 32 != 0,
        agent: seen & 64 != 0,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Event {
    Clock,
    Display,
    Input,
    Userspace,
    Frame,
    ShellReady,
}

impl Event {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clock => "clock",
            Self::Display => "display",
            Self::Input => "input",
            Self::Userspace => "userspace",
            Self::Frame => "frame",
            Self::ShellReady => "shell_ready",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "clock" => Ok(Self::Clock),
            "display" => Ok(Self::Display),
            "input" => Ok(Self::Input),
            "userspace" => Ok(Self::Userspace),
            "frame" => Ok(Self::Frame),
            "shell_ready" => Ok(Self::ShellReady),
            _ => Err(Error::Schema),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Item {
    Input,
    Mem,
    Burn,
    Flip,
    Telemetry,
    Agent,
}

impl Item {
    pub const ALL: [Self; 6] = [
        Self::Input,
        Self::Mem,
        Self::Burn,
        Self::Flip,
        Self::Telemetry,
        Self::Agent,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Mem => "mem",
            Self::Burn => "burn",
            Self::Flip => "flip",
            Self::Telemetry => "telemetry",
            Self::Agent => "agent",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|item| item.as_str() == value)
            .ok_or(Error::Schema)
    }
}

impl Config {
    pub const fn requested(self, item: Item) -> bool {
        match item {
            Item::Input => self.input,
            Item::Mem => self.mem,
            Item::Burn => self.burn,
            Item::Flip => self.flip,
            Item::Telemetry => self.telemetry,
            Item::Agent => self.agent,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Smoke,
    AgentDemo,
    NoClock,
    NoSession,
    Unavailable,
}

impl SkipReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::AgentDemo => "agent_demo",
            Self::NoClock => "no_clock",
            Self::NoSession => "no_session",
            Self::Unavailable => "unavailable",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "smoke" => Ok(Self::Smoke),
            "agent_demo" => Ok(Self::AgentDemo),
            "no_clock" => Ok(Self::NoClock),
            "no_session" => Ok(Self::NoSession),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err(Error::Schema),
        }
    }
}

/// The 0019a records. Future input/window/burn schemas must explicitly extend
/// this enum; unknown `MIRROR_PERF_` families are never silently accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    On {
        mem: bool,
        input: bool,
        burn: bool,
    },
    Off,
    Time {
        event: Event,
        ns: Option<u64>,
    },
    Memory {
        ns: Option<u64>,
        available_kib: Option<u64>,
        pool_kib: Option<u64>,
    },
    Skipped {
        item: Item,
        reason: SkipReason,
    },
}

impl Record {
    pub const fn on(config: Config) -> Self {
        Self::On {
            mem: config.mem,
            input: config.input,
            burn: config.burn,
        }
    }

    /// Parse a single record without a line terminator. Exact field order,
    /// single spaces, and unsigned ASCII decimal values are required.
    pub fn parse(line: &str) -> Result<Self, Error> {
        if line.len() > MAX_RECORD_BYTES {
            return Err(Error::TooLong);
        }
        if !line.is_ascii() {
            return Err(Error::NonAscii);
        }
        let mut words = line.split(' ');
        let record = match words.next() {
            Some("MIRROR_PERF_ON") => {
                if field(&mut words, "v=")? != "1" {
                    return Err(Error::Schema);
                }
                let mem = flag(field(&mut words, "mem=")?)?;
                let input = flag(field(&mut words, "input=")?)?;
                let burn = flag(field(&mut words, "burn=")?)?;
                if (input && !mem) || (burn && !input) {
                    return Err(Error::Flags);
                }
                Self::On { mem, input, burn }
            }
            Some("MIRROR_PERF_OFF") => {
                if field(&mut words, "reason=")? != "invalid" {
                    return Err(Error::Schema);
                }
                Self::Off
            }
            Some("MIRROR_PERF_T") => {
                let event = Event::parse(field(&mut words, "event=")?)?;
                let ns = optional_number(field(&mut words, "ns=")?)?;
                if event == Event::Clock && field(&mut words, "since_entry_ns=")? != "none" {
                    return Err(Error::Schema);
                }
                Self::Time { event, ns }
            }
            Some("MIRROR_PERF_MEM") => {
                let ns = optional_number(field(&mut words, "ns=")?)?;
                let available_kib = optional_number(field(&mut words, "available_kib=")?)?;
                let pool_kib = optional_number(field(&mut words, "pool_kib=")?)?;
                memory_bounds(available_kib, pool_kib)?;
                Self::Memory {
                    ns,
                    available_kib,
                    pool_kib,
                }
            }
            Some("MIRROR_PERF_SKIPPED") => Self::Skipped {
                item: Item::parse(field(&mut words, "item=")?)?,
                reason: SkipReason::parse(field(&mut words, "reason=")?)?,
            },
            _ => return Err(Error::Schema),
        };
        if words.next().is_some() {
            return Err(Error::Schema);
        }
        Ok(record)
    }
}

fn field<'a>(words: &mut core::str::Split<'a, char>, prefix: &str) -> Result<&'a str, Error> {
    words
        .next()
        .and_then(|word| word.strip_prefix(prefix))
        .ok_or(Error::Schema)
}

fn flag(value: &str) -> Result<bool, Error> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(Error::Number),
    }
}

fn optional_number(value: &str) -> Result<Option<u64>, Error> {
    if value == "none" {
        return Ok(None);
    }
    if value.is_empty() {
        return Err(Error::Number);
    }
    let mut number = 0u64;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return Err(Error::Number);
        }
        number = number
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(byte - b'0')))
            .ok_or(Error::Overflow)?;
    }
    if number == u64::MAX {
        return Err(Error::Overflow);
    }
    Ok(Some(number))
}

struct Number(Option<u64>);

impl fmt::Display for Number {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(number) => write!(formatter, "{number}"),
            None => formatter.write_str("none"),
        }
    }
}

impl fmt::Display for Record {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::On { mem, input, burn } => write!(
                formatter,
                "MIRROR_PERF_ON v=1 mem={} input={} burn={}",
                u8::from(mem),
                u8::from(input),
                u8::from(burn)
            ),
            Self::Off => formatter.write_str("MIRROR_PERF_OFF reason=invalid"),
            Self::Time { event, ns } => {
                write!(
                    formatter,
                    "MIRROR_PERF_T event={} ns={}",
                    event.as_str(),
                    Number(ns)
                )?;
                if event == Event::Clock {
                    formatter.write_str(" since_entry_ns=none")?;
                }
                Ok(())
            }
            Self::Memory {
                ns,
                available_kib,
                pool_kib,
            } => write!(
                formatter,
                "MIRROR_PERF_MEM ns={} available_kib={} pool_kib={}",
                Number(ns),
                Number(available_kib),
                Number(pool_kib)
            ),
            Self::Skipped { item, reason } => write!(
                formatter,
                "MIRROR_PERF_SKIPPED item={} reason={}",
                item.as_str(),
                reason.as_str()
            ),
        }
    }
}

/// Convert the allocator's frame counts to the wire values, checking addition
/// and multiplication rather than allowing wrapping or saturated measurements.
pub fn memory_kib(recycled: u64, never_allocated: u64, total: u64) -> Result<(u64, u64), Error> {
    let available = recycled
        .checked_add(never_allocated)
        .and_then(|n| n.checked_mul(4))
        .ok_or(Error::Overflow)?;
    let pool = total.checked_mul(4).ok_or(Error::Overflow)?;
    memory_bounds(Some(available), Some(pool))?;
    Ok((available, pool))
}

fn memory_bounds(available: Option<u64>, pool: Option<u64>) -> Result<(), Error> {
    match (available, pool) {
        (None, None) => Ok(()),
        (Some(available), Some(pool))
            if available != u64::MAX
                && pool != u64::MAX
                && available <= pool
                && available.is_multiple_of(4)
                && pool.is_multiple_of(4) =>
        {
            Ok(())
        }
        _ => Err(Error::MemoryBounds),
    }
}

/// Availability at the interactive-session decision point. Input and burn
/// remain unavailable in 0019a; later slices extend their execution protocols.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub smoke: bool,
    pub agent_demo: bool,
    pub has_clock: bool,
    pub has_display: bool,
    pub has_session: bool,
}

impl Context {
    pub const fn skip(self, config: Config, item: Item) -> Option<SkipReason> {
        if !config.requested(item) {
            return None;
        }
        if matches!(item, Item::Flip | Item::Telemetry | Item::Agent) {
            return Some(SkipReason::Unavailable);
        }
        if self.smoke {
            return Some(SkipReason::Smoke);
        }
        if self.agent_demo && matches!(item, Item::Input | Item::Burn) {
            return Some(SkipReason::AgentDemo);
        }
        if !self.has_clock {
            return Some(SkipReason::NoClock);
        }
        if !self.has_session {
            return Some(SkipReason::NoSession);
        }
        if matches!(item, Item::Input | Item::Burn) {
            return Some(SkipReason::Unavailable);
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expectation {
    Tokenless,
    Invalid,
    Enabled(Config),
}

/// Streaming validation of perf records extracted from a boot log. Other
/// serial families are filtered by the caller. A failed push leaves validation
/// state unchanged. `finish_prefix` supports expected fatal endpoints; `finish`
/// requires all records for the supplied boot/session context.
#[derive(Clone, Copy, Debug)]
pub struct Validator {
    expected: Expectation,
    context: Context,
    started: bool,
    events: u8,
    skips: u8,
    last_time: Option<u64>,
    memory_samples: u64,
    last_memory: Option<u64>,
}

impl Validator {
    pub const fn new(expected: Expectation, context: Context) -> Self {
        Self {
            expected,
            context,
            started: false,
            events: 0,
            skips: 0,
            last_time: None,
            memory_samples: 0,
            last_memory: None,
        }
    }

    pub fn push_line(&mut self, line: &str) -> Result<(), Error> {
        self.push(Record::parse(line)?)
    }

    pub fn push(&mut self, record: Record) -> Result<(), Error> {
        let mut next = *self;
        next.accept(record)?;
        *self = next;
        Ok(())
    }

    fn wanted_events(self) -> u8 {
        let early = (1 << Event::Clock as u8)
            | (1 << Event::Input as u8)
            | if self.context.has_display {
                1 << Event::Display as u8
            } else {
                0
            };
        early
            | if self.context.has_session && !self.context.smoke {
                0b111000
            } else {
                0
            }
    }

    fn wanted_skips(self, config: Config) -> u8 {
        Item::ALL.into_iter().fold(0, |bits, item| {
            bits | if self.context.skip(config, item).is_some() {
                1 << item as u8
            } else {
                0
            }
        })
    }

    fn timestamp(self, ns: Option<u64>) -> Result<(), Error> {
        if ns.is_some() != self.context.has_clock || ns == Some(u64::MAX) {
            return Err(Error::Timestamp);
        }
        Ok(())
    }

    fn accept(&mut self, record: Record) -> Result<(), Error> {
        let config = match self.expected {
            Expectation::Tokenless => return Err(Error::UnexpectedRecord),
            Expectation::Invalid => {
                if self.started || record != Record::Off {
                    return Err(Error::UnexpectedRecord);
                }
                self.started = true;
                return Ok(());
            }
            Expectation::Enabled(config) => config,
        };
        if !self.started {
            if record != Record::on(config) {
                return Err(Error::Flags);
            }
            self.started = true;
            return Ok(());
        }
        match record {
            Record::Time { event, ns } => {
                let remaining = self.wanted_events() & !self.events;
                if remaining == 0 || remaining.trailing_zeros() != event as u32 {
                    return Err(Error::Order);
                }
                if event as u8 >= Event::Userspace as u8 && self.skips != self.wanted_skips(config)
                {
                    return Err(Error::MissingRecord);
                }
                self.timestamp(ns)?;
                if let (Some(previous), Some(now)) = (self.last_time, ns)
                    && now < previous
                {
                    return Err(Error::Timestamp);
                }
                self.last_time = ns;
                self.events |= 1 << event as u8;
            }
            Record::Skipped { item, reason } => {
                if self.events & (1 << Event::Input as u8) == 0
                    || self.events & (1 << Event::Userspace as u8) != 0
                {
                    return Err(Error::Order);
                }
                let remaining = self.wanted_skips(config) & !self.skips;
                if remaining == 0
                    || remaining.trailing_zeros() != item as u32
                    || self.context.skip(config, item) != Some(reason)
                {
                    return Err(Error::Order);
                }
                self.skips |= 1 << item as u8;
            }
            Record::Memory {
                ns,
                available_kib,
                pool_kib,
            } => {
                if !config.mem
                    || self.context.smoke
                    || !self.context.has_session
                    || self.events & (1 << Event::Userspace as u8) == 0
                {
                    return Err(Error::UnexpectedRecord);
                }
                self.timestamp(ns)?;
                memory_bounds(available_kib, pool_kib)?;
                if self.memory_samples != 0 {
                    match (self.last_memory, ns) {
                        (Some(previous), Some(now))
                            if now
                                .checked_sub(previous)
                                .is_some_and(|delta| delta >= MEMORY_INTERVAL_NS) => {}
                        _ => return Err(Error::MemorySpacing),
                    }
                }
                self.memory_samples = self.memory_samples.checked_add(1).ok_or(Error::Overflow)?;
                self.last_memory = ns;
            }
            Record::On { .. } | Record::Off => return Err(Error::UnexpectedRecord),
        }
        Ok(())
    }

    /// Require just a valid reached prefix, for a separately verified fatal
    /// endpoint. An enabled/invalid token must still have its ON/OFF record.
    pub fn finish_prefix(self) -> Result<(), Error> {
        if self.expected != Expectation::Tokenless && !self.started {
            return Err(Error::MissingRecord);
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), Error> {
        self.finish_prefix()?;
        if let Expectation::Enabled(config) = self.expected {
            if self.events != self.wanted_events() || self.skips != self.wanted_skips(config) {
                return Err(Error::MissingRecord);
            }
            if config.mem
                && self.context.has_session
                && !self.context.smoke
                && self.memory_samples == 0
            {
                return Err(Error::MissingRecord);
            }
        }
        Ok(())
    }

    pub const fn memory_samples(self) -> u64 {
        self.memory_samples
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;
