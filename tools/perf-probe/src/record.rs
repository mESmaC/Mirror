//! Small, bounded JSON records. Duplicate keys and ambiguous numbers are rejected.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const RECORD_LIMIT: usize = 256 * 1024;
pub const OWNER_LIMIT: usize = 16 * 1024;
pub const VMX_LIMIT: usize = 1024 * 1024;
static TEMP: AtomicU64 = AtomicU64::new(0);

pub fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(u64),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}
impl Value {
    pub fn object(fields: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Self {
        Self::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
    pub fn text(v: impl Into<String>) -> Self {
        Self::String(v.into())
    }
    pub fn get(&self, key: &str) -> io::Result<&Value> {
        match self {
            Self::Object(o) => o.get(key).ok_or_else(|| invalid(format!("missing {key}"))),
            _ => Err(invalid("expected object")),
        }
    }
    pub fn string(&self) -> io::Result<&str> {
        match self {
            Self::String(s) => Ok(s),
            _ => Err(invalid("expected string")),
        }
    }
    pub fn number(&self) -> io::Result<u64> {
        match self {
            Self::Number(n) => Ok(*n),
            _ => Err(invalid("expected unsigned integer")),
        }
    }
    pub fn boolean(&self) -> io::Result<bool> {
        match self {
            Self::Bool(b) => Ok(*b),
            _ => Err(invalid("expected boolean")),
        }
    }
    pub fn array(&self) -> io::Result<&[Value]> {
        match self {
            Self::Array(v) => Ok(v),
            _ => Err(invalid("expected array")),
        }
    }
    pub fn fields(&self, expected: &[&str]) -> io::Result<()> {
        let Self::Object(o) = self else {
            return Err(invalid("expected object"));
        };
        if o.len() != expected.len() || expected.iter().any(|k| !o.contains_key(*k)) {
            return Err(invalid("unknown or missing record field"));
        }
        Ok(())
    }
    pub fn set(&mut self, key: &str, value: Value) -> io::Result<()> {
        let Self::Object(o) = self else {
            return Err(invalid("expected object"));
        };
        o.insert(key.into(), value);
        Ok(())
    }
    pub fn encode(&self) -> String {
        match self {
            Self::Null => "null".into(),
            Self::Bool(b) => b.to_string(),
            Self::Number(n) => n.to_string(),
            Self::String(s) => quote(s),
            Self::Array(a) => format!(
                "[{}]",
                a.iter().map(Self::encode).collect::<Vec<_>>().join(",")
            ),
            Self::Object(o) => format!(
                "{{{}}}",
                o.iter()
                    .map(|(k, v)| format!("{}:{}", quote(k), v.encode()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }
}
fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}
impl Parser<'_> {
    fn ws(&mut self) {
        while self
            .input
            .get(self.pos)
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.pos += 1;
        }
    }
    fn byte(&mut self) -> io::Result<u8> {
        let b = *self
            .input
            .get(self.pos)
            .ok_or_else(|| invalid("truncated JSON"))?;
        self.pos += 1;
        Ok(b)
    }
    fn expect(&mut self, b: u8) -> io::Result<()> {
        if self.byte()? != b {
            return Err(invalid("unexpected JSON byte"));
        }
        Ok(())
    }
    fn hex(&mut self) -> io::Result<u16> {
        let mut n = 0;
        for _ in 0..4 {
            let b = self.byte()?;
            let d = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(invalid("invalid unicode escape")),
            };
            n = (n << 4) | u16::from(d);
        }
        Ok(n)
    }
    fn string(&mut self) -> io::Result<String> {
        self.expect(b'"')?;
        let mut bytes = Vec::new();
        loop {
            match self.byte()? {
                b'"' => {
                    return String::from_utf8(bytes).map_err(|_| invalid("invalid UTF8 string"));
                }
                b'\\' => match self.byte()? {
                    b'"' => bytes.push(b'"'),
                    b'\\' => bytes.push(b'\\'),
                    b'/' => bytes.push(b'/'),
                    b'b' => bytes.push(8),
                    b'f' => bytes.push(12),
                    b'n' => bytes.push(b'\n'),
                    b'r' => bytes.push(b'\r'),
                    b't' => bytes.push(b'\t'),
                    b'u' => {
                        let first = self.hex()?;
                        let code = if (0xd800..=0xdbff).contains(&first) {
                            self.expect(b'\\')?;
                            self.expect(b'u')?;
                            let low = self.hex()?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err(invalid("invalid surrogate pair"));
                            }
                            0x10000 + ((u32::from(first) - 0xd800) << 10) + u32::from(low) - 0xdc00
                        } else {
                            u32::from(first)
                        };
                        let c = char::from_u32(code)
                            .ok_or_else(|| invalid("invalid Unicode scalar"))?;
                        let mut tmp = [0; 4];
                        bytes.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    }
                    _ => return Err(invalid("invalid JSON escape")),
                },
                0..=31 => return Err(invalid("control byte in JSON string")),
                b => bytes.push(b),
            }
        }
    }
    fn value(&mut self, depth: usize) -> io::Result<Value> {
        if depth > 16 {
            return Err(invalid("JSON nesting exceeds 16"));
        }
        self.ws();
        match self
            .input
            .get(self.pos)
            .copied()
            .ok_or_else(|| invalid("missing JSON value"))?
        {
            b'"' => Ok(Value::String(self.string()?)),
            b'0'..=b'9' => {
                let start = self.pos;
                while self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
                    self.pos += 1;
                }
                let b = &self.input[start..self.pos];
                if b.len() > 1 && b[0] == b'0' {
                    return Err(invalid("leading zero"));
                }
                let n = std::str::from_utf8(b)
                    .unwrap()
                    .parse()
                    .map_err(|_| invalid("integer overflow"))?;
                Ok(Value::Number(n))
            }
            b'n' | b't' | b'f' => {
                let (literal, v): (&[u8], Value) = match self.input[self.pos] {
                    b'n' => (b"null", Value::Null),
                    b't' => (b"true", Value::Bool(true)),
                    _ => (b"false", Value::Bool(false)),
                };
                for b in literal {
                    self.expect(*b)?;
                }
                Ok(v)
            }
            b'[' => {
                self.pos += 1;
                self.ws();
                let mut a = Vec::new();
                if self.input.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(Value::Array(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    match self.byte()? {
                        b']' => break,
                        b',' => {}
                        _ => return Err(invalid("expected array separator")),
                    }
                }
                Ok(Value::Array(a))
            }
            b'{' => {
                self.pos += 1;
                self.ws();
                let mut o = BTreeMap::new();
                if self.input.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(Value::Object(o));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    self.ws();
                    self.expect(b':')?;
                    let value = self.value(depth + 1)?;
                    if o.insert(key, value).is_some() {
                        return Err(invalid("duplicate JSON key"));
                    }
                    self.ws();
                    match self.byte()? {
                        b'}' => break,
                        b',' => {}
                        _ => return Err(invalid("expected object separator")),
                    }
                }
                Ok(Value::Object(o))
            }
            _ => Err(invalid("unsupported JSON value")),
        }
    }
}
pub fn parse(bytes: &[u8]) -> io::Result<Value> {
    if bytes.len() > RECORD_LIMIT {
        return Err(invalid("record exceeds 256 KiB"));
    }
    let mut p = Parser {
        input: bytes,
        pos: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.pos != bytes.len() {
        return Err(invalid("trailing JSON data"));
    }
    Ok(v)
}
pub fn read_bounded(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > limit {
        return Err(invalid(format!("{} exceeds {limit} bytes", path.display())));
    }
    Ok(out)
}
pub fn read(path: &Path) -> io::Result<Value> {
    parse(&read_bounded(path, RECORD_LIMIT)?)
}
pub fn write(path: &Path, value: &Value) -> io::Result<()> {
    let mut text = value.encode();
    text.push('\n');
    if text.len() > RECORD_LIMIT {
        return Err(invalid("record exceeds 256 KiB"));
    }
    atomic_write(path, text.as_bytes())
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("record must have parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("record must have name"))?
        .to_string_lossy();
    let temporary = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        replace(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: Both buffers are live NUL-terminated UTF-16 paths; flags request
    // atomic same-volume replacement with write-through, without copying.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)?;
    File::open(to.parent().unwrap())?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_records() {
        for bad in [
            "{\"x\":1,\"x\":2}",
            "01",
            "1.0",
            "1e3",
            "-1",
            "[1,]",
            "{\"x\":true,}",
            "true false",
            "\"\\ud800\"",
            "18446744073709551616",
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad}");
        }
        let v = Value::object([
            ("path", Value::text("C:\\x\n中文🦀")),
            (
                "values",
                Value::Array(vec![
                    Value::Number(u64::MAX),
                    Value::Null,
                    Value::Bool(false),
                ]),
            ),
        ]);
        assert_eq!(parse(v.encode().as_bytes()).unwrap(), v);
        assert_eq!(parse(b"\"\\ud83e\\udd80\"").unwrap(), Value::text("🦀"));
        assert!(parse(&vec![b' '; RECORD_LIMIT + 1]).is_err());
        assert!(parse(format!("{}0{}", "[".repeat(18), "]".repeat(18)).as_bytes()).is_err());
    }
    #[test]
    fn atomic_replace_leaves_complete_valid_record() {
        let dir = std::env::temp_dir().join(format!(
            "mirror-record-{}-{}",
            std::process::id(),
            TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let p = dir.join("final.json");
        for i in 0..20 {
            let v = Value::object([
                ("schema", Value::Number(1)),
                ("generation", Value::Number(i)),
            ]);
            write(&p, &v).unwrap();
            assert_eq!(read(&p).unwrap(), v);
        }
        fs::remove_file(p).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
