//! Bounded raw CSV samples and explicitly selected statistical calculations.
//!
//! CSV may begin with a UTF-8 BOM and contain `#` metadata lines between
//! records. Fields follow RFC 4180 quoting, including escaped quotes and
//! quoted newlines; both LF and CRLF record endings are accepted. Physical
//! lines and logical records are limited to 64 KiB, excluding record endings.
//! Warmups are validated but excluded from both samples and timeout counts.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_RECORDS: usize = 1_000_000;

/// A named calculation, not an implicit measurement-protocol default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PercentileMethod {
    /// The observation at one-based rank ceil(0.95 * N), without interpolation.
    NearestRank,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub min: u64,
    pub max: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Samples {
    /// Successful measured values, in file order.
    pub values: Vec<u64>,
    /// Measured timeout records, excluded from `values` but never discarded.
    pub timeouts: usize,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Median uses the middle value or the arithmetic mean of the two middle
/// values. Floating results may round integers above f64's exact range.
/// Callers must select a percentile convention explicitly.
pub fn summarize(values: &[u64], method: PercentileMethod) -> io::Result<Stats> {
    if values.is_empty() || values.len() > MAX_RECORDS {
        return Err(invalid("summary requires between 1 and 1000000 samples"));
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let count = sorted.len();
    let middle = count / 2;
    let median = if count.is_multiple_of(2) {
        let lower = sorted[middle - 1];
        let upper = sorted[middle];
        // Subtract before converting instead of adding two possibly large u64s.
        lower as f64 + (upper - lower) as f64 / 2.0
    } else {
        sorted[middle] as f64
    };
    let p95 = match method {
        // count is capped at one million, so the multiplication cannot overflow.
        PercentileMethod::NearestRank => sorted[(count * 95).div_ceil(100) - 1] as f64,
    };
    Ok(Stats {
        count,
        median,
        p95,
        min: sorted[0],
        max: sorted[count - 1],
    })
}

/// Read a complete regular CSV file, rejecting oversized or malformed input.
/// If `phase` exists it must be `measured` or `warmup`; if `status` exists it
/// must be `ok` or `timeout`. A blank numeric value is allowed only on timeout
/// rows. `duration_ns`, when present, is validated even if selecting another
/// column. Unknown statuses and malformed warmups are errors, not omissions.
pub fn samples(path: &Path, column: &str) -> io::Result<Samples> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(invalid("CSV must be a regular file no larger than 16 MiB"));
    }
    let mut bytes = Vec::new();
    // The extra byte detects growth after metadata was read; no suffix can be
    // silently ignored at the size limit.
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("CSV is not UTF-8"))?;
    parse_samples(text, column)
}

fn parse_samples(text: &str, column: &str) -> io::Result<Samples> {
    if text.len() > MAX_FILE_BYTES {
        return Err(invalid("CSV exceeds 16 MiB"));
    }
    if column.is_empty() {
        return Err(invalid("sample column must not be empty"));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.contains('\0') {
        return Err(invalid("CSV contains a NUL byte"));
    }
    for line in text.split('\n') {
        if line.strip_suffix('\r').unwrap_or(line).len() > MAX_RECORD_BYTES {
            return Err(invalid("CSV physical line exceeds 64 KiB"));
        }
    }
    let mut records = Records {
        bytes: text.as_bytes(),
        cursor: 0,
    };
    let header = records
        .next()?
        .ok_or_else(|| invalid("CSV has no header"))?;
    let mut names = HashSet::new();
    for name in &header {
        if name.is_empty() || !names.insert(name.as_str()) {
            return Err(invalid("CSV has an empty or duplicate column name"));
        }
    }
    let value_index = header
        .iter()
        .position(|name| name == column)
        .ok_or_else(|| invalid("CSV does not contain the requested sample column"))?;
    let phase_index = header.iter().position(|name| name == "phase");
    let status_index = header.iter().position(|name| name == "status");
    let duration_index = header.iter().position(|name| name == "duration_ns");
    let mut result = Samples {
        values: Vec::new(),
        timeouts: 0,
    };
    let mut count = 0;
    while let Some(record) = records.next()? {
        count += 1;
        if count > MAX_RECORDS {
            return Err(invalid("CSV exceeds 1000000 data records"));
        }
        if record.len() != header.len() {
            return Err(invalid(
                "CSV row has a different field count from its header",
            ));
        }
        let measured = match phase_index.map(|index| record[index].as_str()) {
            None | Some("measured") => true,
            Some("warmup") => false,
            Some(_) => return Err(invalid("unknown CSV sample phase")),
        };
        let timeout = match status_index.map(|index| record[index].as_str()) {
            None | Some("ok") => false,
            Some("timeout") => true,
            Some(_) => return Err(invalid("unknown CSV sample status")),
        };
        let value = number(&record[value_index], timeout)?;
        if let Some(index) = duration_index
            && index != value_index
        {
            let _ = number(&record[index], timeout)?;
        }
        if measured {
            if timeout {
                result.timeouts += 1;
            } else {
                result
                    .values
                    .push(value.ok_or_else(|| invalid("successful sample is blank"))?);
            }
        }
    }
    Ok(result)
}

fn number(text: &str, timeout: bool) -> io::Result<Option<u64>> {
    if text.is_empty() && timeout {
        return Ok(None);
    }
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("sample must be an unsigned decimal integer"));
    }
    text.parse()
        .map(Some)
        .map_err(|_| invalid("sample exceeds u64 range"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldState {
    Start,
    Unquoted,
    Quoted,
    AfterQuote,
}

struct Records<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl Records<'_> {
    fn next(&mut self) -> io::Result<Option<Vec<String>>> {
        // Metadata is recognized only at the start of a record, never inside
        // a quoted field whose continuation happens to begin with '#'.
        while self.bytes.get(self.cursor) == Some(&b'#') {
            while self
                .bytes
                .get(self.cursor)
                .is_some_and(|byte| *byte != b'\n')
            {
                self.cursor += 1;
            }
            if self.cursor < self.bytes.len() {
                self.cursor += 1;
            }
        }
        if self.cursor == self.bytes.len() {
            return Ok(None);
        }
        let start = self.cursor;
        let mut fields = Vec::new();
        let mut field = Vec::new();
        let mut state = FieldState::Start;
        while let Some(&byte) = self.bytes.get(self.cursor) {
            let terminator = state != FieldState::Quoted && matches!(byte, b'\r' | b'\n');
            if !terminator && self.cursor - start >= MAX_RECORD_BYTES {
                return Err(invalid("CSV logical record exceeds 64 KiB"));
            }
            self.cursor += 1;
            if state != FieldState::Quoted && (byte == b',' || terminator) {
                fields.push(
                    String::from_utf8(std::mem::take(&mut field))
                        .map_err(|_| invalid("CSV field is not UTF-8"))?,
                );
                if byte == b',' {
                    state = FieldState::Start;
                    continue;
                }
                if byte == b'\r' {
                    if self.bytes.get(self.cursor) != Some(&b'\n') {
                        return Err(invalid("bare CR outside a quoted CSV field"));
                    }
                    self.cursor += 1;
                }
                return Ok(Some(fields));
            }
            match (state, byte) {
                (FieldState::Start, b'"') => state = FieldState::Quoted,
                (FieldState::Unquoted, b'"') => {
                    return Err(invalid("quote inside an unquoted CSV field"));
                }
                (FieldState::Quoted, b'"') => state = FieldState::AfterQuote,
                (FieldState::AfterQuote, b'"') => {
                    field.push(b'"');
                    state = FieldState::Quoted;
                }
                (FieldState::AfterQuote, _) => {
                    return Err(invalid("unexpected data after a closing CSV quote"));
                }
                (FieldState::Quoted, _) => field.push(byte),
                (_, _) => {
                    field.push(byte);
                    state = FieldState::Unquoted;
                }
            }
        }
        if state == FieldState::Quoted {
            return Err(invalid("unterminated quoted CSV field"));
        }
        fields.push(String::from_utf8(field).map_err(|_| invalid("CSV field is not UTF-8"))?);
        Ok(Some(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_medians_and_nearest_ranks() {
        let method = PercentileMethod::NearestRank;
        assert_eq!(
            summarize(&[9, 1, 4], method).unwrap(),
            Stats {
                count: 3,
                median: 4.0,
                p95: 9.0,
                min: 1,
                max: 9,
            }
        );
        assert_eq!(summarize(&[4, 1, 3, 2], method).unwrap().median, 2.5);
        assert_eq!(summarize(&[7], method).unwrap().p95, 7.0);
        let twenty: Vec<u64> = (1..=20).rev().collect();
        assert_eq!(summarize(&twenty, method).unwrap().p95, 19.0);
        let twenty_one: Vec<u64> = (1..=21).collect();
        assert_eq!(summarize(&twenty_one, method).unwrap().p95, 20.0);
        let hundred: Vec<u64> = (1..=100).collect();
        let stats = summarize(&hundred, method).unwrap();
        assert_eq!((stats.median, stats.p95), (50.5, 95.0));
        assert_eq!(
            summarize(&[u64::MAX, u64::MAX], method).unwrap().median,
            u64::MAX as f64
        );
        assert!(summarize(&[], method).is_err());
    }

    #[test]
    fn timeouts_remain_counted_and_warmups_are_excluded() {
        let csv = "# method example\nphase,status,duration_ns\n\
                   warmup,ok,999\nmeasured,ok,10\nmeasured,timeout,\n\
                   # more metadata\nmeasured,ok,20\nmeasured,timeout,400000000\n\
                   warmup,timeout,\n";
        let parsed = parse_samples(csv, "duration_ns").unwrap();
        assert_eq!(
            parsed,
            Samples {
                values: vec![10, 20],
                timeouts: 2,
            }
        );
        assert_eq!(
            summarize(&parsed.values, PercentileMethod::NearestRank)
                .unwrap()
                .median,
            15.0
        );
        let only_timeout = parse_samples("status,n\ntimeout,\n", "n").unwrap();
        assert!(only_timeout.values.is_empty());
        assert_eq!(only_timeout.timeouts, 1);
    }

    #[test]
    fn quoted_fields_multiline_comments_bom_and_crlf() {
        let csv = "\u{feff}# metadata\r\n\"duration_ns\",note\r\n\
                   \"42\",\"comma, doubled \"\"quote\"\"\r\n# retained text\"\r\n\
                   7,ordinary";
        assert_eq!(parse_samples(csv, "duration_ns").unwrap().values, [42, 7]);
        let mut records = Records {
            bytes: b"\"a,b\",\"c\"\"d\"\n",
            cursor: 0,
        };
        assert_eq!(records.next().unwrap().unwrap(), ["a,b", "c\"d"]);
        assert!(records.next().unwrap().is_none());
    }

    #[test]
    fn malformed_csv_and_numbers_fail_closed() {
        for csv in [
            "",
            "# metadata only",
            "n,n\n1,2\n",
            "n,\n1,2\n",
            "n\n1,2\n",
            "n,note\n1\n",
            "n\n\"1\n",
            "n\n\"1\"x\n",
            "n\n1\"\n",
            "n\n1\r2\n",
            "n\n\n",
            "n\n+1\n",
            "n\n-1\n",
            "n\n 1\n",
            "n\n1.0\n",
            "n\n18446744073709551616\n",
            "n\n1\0\n",
            "n\nn\n",
        ] {
            assert!(parse_samples(csv, "n").is_err(), "accepted {csv:?}");
        }
        assert!(parse_samples("n\n1\n", "missing").is_err());
        assert!(parse_samples("n\n1\n", "").is_err());
        assert_eq!(
            parse_samples("n\n18446744073709551615\n", "n")
                .unwrap()
                .values,
            [u64::MAX]
        );
    }

    #[test]
    fn failed_and_malformed_skipped_samples_are_not_hidden() {
        for csv in [
            "phase,status,n\nmeasured,error,1\n",
            "phase,status,n\nwarmup,error,1\n",
            "phase,status,n\nunknown,ok,1\n",
            "phase,status,n\nmeasured,ok,\n",
            "phase,status,n\nwarmup,ok,\n",
            "phase,status,n\nwarmup,timeout,invalid\n",
            "phase,status,n\nmeasured,timeout,invalid\n",
            "phase,status,n,duration_ns\nwarmup,ok,1,\n",
            "phase,status,n,duration_ns\nmeasured,ok,1,bad\n",
        ] {
            assert!(parse_samples(csv, "n").is_err(), "accepted {csv:?}");
        }
    }

    #[test]
    fn file_line_record_and_count_bounds() {
        let file = "#".repeat(MAX_FILE_BYTES + 1);
        assert!(parse_samples(&file, "n").is_err());
        let line = format!("n,note\n1,{}\n", "x".repeat(MAX_RECORD_BYTES));
        assert!(parse_samples(&line, "n").is_err());
        let record = format!(
            "n,note\n1,\"{}\n{}\"\n",
            "x".repeat(MAX_RECORD_BYTES / 2),
            "x".repeat(MAX_RECORD_BYTES / 2)
        );
        assert!(parse_samples(&record, "n").is_err());
        let records = format!("n\n{}", "1\n".repeat(MAX_RECORDS + 1));
        assert!(parse_samples(&records, "n").is_err());
        assert!(summarize(&vec![1; MAX_RECORDS + 1], PercentileMethod::NearestRank).is_err());
    }
}
