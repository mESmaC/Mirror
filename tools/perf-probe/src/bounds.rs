//! Monotonic operation totals and evidence/input caps from spec 0015c.
//!
//! Callers create one case/recovery deadline and derive child deadlines from
//! it. Completing a query, receiving a byte or reconnecting never extends it.

use std::io;
use std::time::{Duration, Instant};

pub const ACTIVE_CASE: Duration = Duration::from_secs(900);
pub const PACKAGE_STAGE: Duration = Duration::from_secs(120);
pub const STATE_PHASE: Duration = Duration::from_secs(15);
pub const STATE_QUERIES: usize = 32;
pub const LIFECYCLE_COMMAND: Duration = Duration::from_secs(60);
pub const READINESS: Duration = Duration::from_secs(120);
pub const CONNECT_ATTEMPT: Duration = Duration::from_secs(3);
pub const CONNECT_ATTEMPTS: usize = 8;
pub const TYPED_ACTION: Duration = Duration::from_secs(30);
pub const KEY_WRITE: Duration = Duration::from_millis(100);
pub const KEY_PAIR: Duration = Duration::from_millis(200);
pub const KEY_PAIRS_ACTION: usize = 32;
pub const KEY_PAIRS_CASE: usize = 64;
pub const FAILURE_EVIDENCE: Duration = Duration::from_secs(30);
pub const FAILURE_TOTAL: Duration = Duration::from_secs(105);
pub const RECOVER: Duration = Duration::from_secs(120);
pub const RECOVER_POWER_OFF: Duration = Duration::from_secs(300);
pub const PACKAGE_RECOVER: Duration = Duration::from_secs(120);
pub const D11_OBSERVATION: Duration = Duration::from_secs(120);
pub const D11_POLLS: usize = 240;
pub const NORMAL_CHECKPOINTS: usize = 12;
pub const FAILURE_CHECKPOINTS: usize = 4;
pub const COMMAND_STREAM_BYTES: usize = 1024 * 1024;
pub const COMMAND_TOTAL_BYTES: usize = 16 * 1024 * 1024;
pub const SERIAL_BYTES: usize = 64 * 1024 * 1024;
pub const VMWARE_LOG_BYTES: usize = 64 * 1024 * 1024;
pub const VMX_BYTES: usize = 1024 * 1024;
pub const RECORD_BYTES: usize = 256 * 1024;
pub const OWNER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn new(cap: Duration) -> io::Result<Self> {
        Self::at(Instant::now(), cap)
    }

    /// Construct against an injected monotonic clock reading.
    pub fn at(now: Instant, cap: Duration) -> io::Result<Self> {
        if cap.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "operation cap must be positive",
            ));
        }
        now.checked_add(cap).map(Self).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "operation deadline overflow")
        })
    }

    pub fn remaining(self) -> io::Result<Duration> {
        self.remaining_at(Instant::now())
    }

    pub fn remaining_at(self, now: Instant) -> io::Result<Duration> {
        self.0
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "operation deadline expired"))
    }

    pub fn child(self, cap: Duration) -> io::Result<Self> {
        self.child_at(Instant::now(), cap)
    }

    /// Derive a phase once; callers must retain this deadline while polling.
    pub fn child_at(self, now: Instant, cap: Duration) -> io::Result<Self> {
        Self::at(now, cap.min(self.remaining_at(now)?))
    }

    pub fn expires_at(self) -> Instant {
        self.0
    }
}

#[derive(Debug)]
pub struct CountBudget {
    cap: usize,
    used: usize,
}

impl CountBudget {
    pub const fn new(cap: usize) -> Self {
        Self { cap, used: 0 }
    }

    /// Charge before dispatch, including attempts that later fail.
    pub fn consume(&mut self) -> io::Result<()> {
        if self.used == self.cap {
            return Err(io::Error::other("operation count cap exceeded"));
        }
        self.used += 1;
        Ok(())
    }

    pub fn used(&self) -> usize {
        self.used
    }
}

#[derive(Debug)]
pub struct Checkpoints {
    normal: CountBudget,
    failure: CountBudget,
}

impl Default for Checkpoints {
    fn default() -> Self {
        Self {
            normal: CountBudget::new(NORMAL_CHECKPOINTS),
            failure: CountBudget::new(FAILURE_CHECKPOINTS),
        }
    }
}

impl Checkpoints {
    pub fn normal(&mut self) -> io::Result<()> {
        self.normal.consume()
    }

    pub fn failure(&mut self) -> io::Result<()> {
        self.failure.consume()
    }
}

/// A stream is minted by its EvidenceBudget. Command stdout and stderr each
/// need their own stream. Log epochs share their case-wide family total.
#[derive(Debug)]
pub struct EvidenceStream {
    kind: EvidenceKind,
    bytes: Vec<u8>,
}

impl EvidenceStream {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Copy, Debug)]
pub enum EvidenceKind {
    Command,
    Serial,
    VmwareLog,
}

#[derive(Debug, Default)]
pub struct EvidenceBudget {
    command: usize,
    serial: usize,
    vmware: usize,
    overflowed: bool,
}

impl EvidenceBudget {
    pub fn stream(&self, kind: EvidenceKind) -> EvidenceStream {
        EvidenceStream {
            kind,
            bytes: Vec::new(),
        }
    }

    /// Preserve exactly the available prefix on overflow, then return failure.
    /// Once overflowed, later cleanup cannot turn this evidence budget valid.
    /// Streams must remain with the budget that created them for the run.
    pub fn append(&mut self, stream: &mut EvidenceStream, bytes: &[u8]) -> io::Result<()> {
        if self.overflowed {
            return Err(io::Error::other("evidence byte cap was exceeded"));
        }
        let (total, total_cap, stream_cap) = match stream.kind {
            EvidenceKind::Command => (&mut self.command, COMMAND_TOTAL_BYTES, COMMAND_STREAM_BYTES),
            EvidenceKind::Serial => (&mut self.serial, SERIAL_BYTES, SERIAL_BYTES),
            EvidenceKind::VmwareLog => (&mut self.vmware, VMWARE_LOG_BYTES, VMWARE_LOG_BYTES),
        };
        let retained = bytes
            .len()
            .min(total_cap.saturating_sub(*total))
            .min(stream_cap.saturating_sub(stream.bytes.len()));
        stream
            .bytes
            .try_reserve(retained)
            .map_err(|_| io::Error::other("evidence prefix allocation could not be reserved"))?;
        stream.bytes.extend_from_slice(&bytes[..retained]);
        *total += retained;
        if retained != bytes.len() {
            self.overflowed = true;
            return Err(io::Error::other(
                "evidence byte cap exceeded; prefix retained",
            ));
        }
        Ok(())
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }
}

/// Store the initial operational error. Cleanup may add separate evidence but
/// may never replace that error with success or a later cleanup error.
#[derive(Debug)]
pub struct FirstFailure<T>(Option<T>);

impl<T> Default for FirstFailure<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> FirstFailure<T> {
    pub fn record(&mut self, error: T) {
        if self.0.is_none() {
            self.0 = Some(error);
        }
    }

    pub fn get(&self) -> Option<&T> {
        self.0.as_ref()
    }

    pub fn into_inner(self) -> Option<T> {
        self.0
    }
}

#[derive(Debug)]
pub struct KeyBudget {
    pairs: CountBudget,
    emergency: CountBudget,
}

impl Default for KeyBudget {
    fn default() -> Self {
        Self {
            pairs: CountBudget::new(KEY_PAIRS_CASE),
            emergency: CountBudget::new(1),
        }
    }
}

impl KeyBudget {
    pub fn action(&mut self, case: Deadline) -> io::Result<KeyAction<'_>> {
        self.action_at(case, Instant::now())
    }

    pub fn action_at(&mut self, case: Deadline, now: Instant) -> io::Result<KeyAction<'_>> {
        Ok(KeyAction {
            case: self,
            pairs: CountBudget::new(KEY_PAIRS_ACTION),
            deadline: case.child_at(now, TYPED_ACTION)?,
        })
    }

    pub fn emergency_release(&mut self) -> io::Result<()> {
        self.emergency.consume()
    }

    pub fn used(&self) -> usize {
        self.pairs.used()
    }
}

pub struct KeyAction<'a> {
    case: &'a mut KeyBudget,
    pairs: CountBudget,
    deadline: Deadline,
}

impl KeyAction<'_> {
    pub fn deadline(&self) -> Deadline {
        self.deadline
    }

    /// Charge an attempted pair and capture its deadline before key-down.
    pub fn begin_pair(&mut self, now: Instant) -> io::Result<Deadline> {
        let pair = self.deadline.child_at(now, KEY_PAIR)?;
        if self.pairs.used == self.pairs.cap || self.case.pairs.used == self.case.pairs.cap {
            return Err(io::Error::other("key pair count cap exceeded"));
        }
        self.pairs.consume()?;
        self.case.pairs.consume()?;
        Ok(pair)
    }

    pub fn emergency_release(&mut self) -> io::Result<()> {
        self.case.emergency_release()
    }
}

/// The injected writer must return only after each bounded write completes.
/// No frame/query/wait can occur between successful down and immediate up.
/// If either write fails, attempt one bounded emergency release and preserve
/// the original error. Transport retirement may prevent that final write.
pub fn send_key_pair(
    action: &mut KeyAction<'_>,
    keysym: u32,
    mut now: impl FnMut() -> Instant,
    mut write: impl FnMut(u32, bool, Duration) -> io::Result<()>,
) -> io::Result<()> {
    let pair = action.begin_pair(now())?;
    let result = (|| {
        let down = pair.child_at(now(), KEY_WRITE)?;
        write(keysym, true, down.remaining_at(now())?)?;
        down.remaining_at(now())?;
        let up = pair.child_at(now(), KEY_WRITE)?;
        write(keysym, false, up.remaining_at(now())?)?;
        up.remaining_at(now())?;
        pair.remaining_at(now())?;
        Ok(())
    })();
    if result.is_err() && action.emergency_release().is_ok() {
        // This is bounded cleanup after a failed action, not another key pair.
        let _ = write(keysym, false, KEY_WRITE);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn progress_and_new_phases_do_not_extend_the_case_deadline() {
        let start = Instant::now();
        let case = Deadline::at(start, Duration::from_secs(20)).unwrap();
        let phase = case.child_at(start, STATE_PHASE).unwrap();
        assert_eq!(phase.remaining_at(start).unwrap(), STATE_PHASE);
        assert_eq!(
            phase.remaining_at(start + Duration::from_secs(14)).unwrap(),
            Duration::from_secs(1)
        );
        assert!(phase.remaining_at(start + STATE_PHASE).is_err());
        assert_eq!(
            case.child_at(start + Duration::from_secs(19), READINESS)
                .unwrap()
                .expires_at(),
            case.expires_at()
        );
        assert!(
            case.child_at(start + Duration::from_secs(20), READINESS)
                .is_err()
        );
    }

    #[test]
    fn command_overflow_preserves_prefix_and_failure_is_sticky() {
        let mut budget = EvidenceBudget::default();
        let mut stream = budget.stream(EvidenceKind::Command);
        let input = vec![7; COMMAND_STREAM_BYTES + 19];
        assert!(budget.append(&mut stream, &input).is_err());
        assert_eq!(stream.bytes(), &input[..COMMAND_STREAM_BYTES]);
        assert!(budget.overflowed());
        assert!(budget.append(&mut stream, &[]).is_err());
    }

    #[test]
    fn aggregate_command_and_log_epochs_have_independent_total_caps() {
        let mut budget = EvidenceBudget::default();
        let bytes = vec![4; COMMAND_STREAM_BYTES];
        for _ in 0..16 {
            let mut stream = budget.stream(EvidenceKind::Command);
            budget.append(&mut stream, &bytes).unwrap();
        }
        let mut overflow = budget.stream(EvidenceKind::Command);
        assert!(budget.append(&mut overflow, &[1]).is_err());
        assert!(overflow.bytes().is_empty());

        // Seed counters at the boundary rather than allocating 128 MiB.
        let mut budget = EvidenceBudget {
            serial: SERIAL_BYTES - 1,
            vmware: VMWARE_LOG_BYTES - 1,
            ..EvidenceBudget::default()
        };
        let mut serial = budget.stream(EvidenceKind::Serial);
        let mut log = budget.stream(EvidenceKind::VmwareLog);
        budget.append(&mut serial, &[1]).unwrap();
        budget.append(&mut log, &[2]).unwrap();
        let mut next_epoch = budget.stream(EvidenceKind::Serial);
        assert!(budget.append(&mut next_epoch, &[3]).is_err());
        assert!(next_epoch.bytes().is_empty());
    }

    #[test]
    fn checkpoint_and_query_budgets_never_wrap_or_share_failure_allowance() {
        let mut checkpoints = Checkpoints::default();
        for _ in 0..NORMAL_CHECKPOINTS {
            checkpoints.normal().unwrap();
        }
        assert!(checkpoints.normal().is_err());
        for _ in 0..FAILURE_CHECKPOINTS {
            checkpoints.failure().unwrap();
        }
        assert!(checkpoints.failure().is_err());
        let mut queries = CountBudget::new(STATE_QUERIES);
        for _ in 0..STATE_QUERIES {
            queries.consume().unwrap();
        }
        assert!(queries.consume().is_err());
        assert_eq!(queries.used(), STATE_QUERIES);
    }

    #[test]
    fn key_caps_count_failed_attempts_and_emergency_is_once_per_case() {
        let start = Instant::now();
        let case = Deadline::at(start, ACTIVE_CASE).unwrap();
        let mut keys = KeyBudget::default();
        for _ in 0..2 {
            let mut action = keys.action_at(case, start).unwrap();
            for _ in 0..32 {
                action.begin_pair(start).unwrap();
            }
            assert!(action.begin_pair(start).is_err());
        }
        let mut action = keys.action_at(case, start).unwrap();
        assert!(action.begin_pair(start).is_err());
        action.emergency_release().unwrap();
        assert!(action.emergency_release().is_err());
        assert_eq!(keys.used(), 64);
    }

    #[test]
    fn key_up_is_immediate_and_slow_write_is_failure_with_one_release() {
        let start = Instant::now();
        let clock = Cell::new(start);
        let case = Deadline::at(start, ACTIVE_CASE).unwrap();
        let mut keys = KeyBudget::default();
        let mut action = keys.action_at(case, start).unwrap();
        let mut calls = Vec::new();
        send_key_pair(
            &mut action,
            42,
            || clock.get(),
            |key, down, cap| {
                calls.push((key, down, cap));
                clock.set(clock.get() + Duration::from_millis(50));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls, [(42, true, KEY_WRITE), (42, false, KEY_WRITE)]);
        calls.clear();
        let error = send_key_pair(
            &mut action,
            43,
            || clock.get(),
            |key, down, cap| {
                calls.push((key, down, cap));
                if down {
                    clock.set(clock.get() + Duration::from_millis(101));
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls, [(43, true, KEY_WRITE), (43, false, KEY_WRITE)]);
        assert!(action.emergency_release().is_err());
    }

    #[test]
    fn failed_key_down_is_not_retried_and_cleanup_error_does_not_replace_it() {
        let start = Instant::now();
        let case = Deadline::at(start, ACTIVE_CASE).unwrap();
        let mut keys = KeyBudget::default();
        let mut action = keys.action_at(case, start).unwrap();
        let mut calls = Vec::new();
        let error = send_key_pair(
            &mut action,
            7,
            || start,
            |_, down, _| {
                calls.push(down);
                Err(io::Error::other(if down {
                    "original failure"
                } else {
                    "cleanup failure"
                }))
            },
        )
        .unwrap_err();
        assert_eq!(calls, [true, false]);
        assert_eq!(error.to_string(), "original failure");
        let mut first = FirstFailure::default();
        first.record("action failure");
        first.record("cleanup failure");
        assert_eq!(first.into_inner(), Some("action failure"));
    }
}
