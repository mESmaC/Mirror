//! Generic 0015c VM orchestration. Only synthetic adapters are admitted until
//! 0015d supplies and validates its service-completion and VM-worker separation
//! rules. A synthetic result cannot activate either live entry or live recovery.

use crate::bounds::{self, Deadline};
use crate::package::validate_id;
use crate::record::{self, Value};
use std::io;
use std::time::{Duration, Instant};

pub const LIVE_VM_ENABLED: bool = false;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendKind {
    Synthetic,
    Live,
}

/// Shared by the future live case and recovery entry points.
pub fn require_live_entry() -> io::Result<()> {
    if !LIVE_VM_ENABLED {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "VM cases and recovery are disabled together until 0015d D11 proof",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Off,
    Running,
    Suspended,
    Transitioning,
    Unknown,
}
impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Transitioning => "transitioning",
            Self::Unknown => "unknown",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Case {
    Boot,
    SuspendResume,
    Restart,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Start,
    Suspend,
    Stop,
    GuestRestartEnter,
}
impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Start => "start-nogui",
            Self::Suspend => "suspend-hard",
            Self::Stop => "stop-hard",
            Self::GuestRestartEnter => "guest-restart-enter",
        }
    }
    fn resulting_state(self) -> State {
        match self {
            Self::Start | Self::GuestRestartEnter => State::Running,
            Self::Suspend => State::Suspended,
            Self::Stop => State::Off,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestStep {
    BootShell,
    EnterConsole,
    ConsoleReady,
    Status,
    Echo,
    PreserveBeforeSuspend,
    ResumeReady,
    ResumeStatus,
    RestartEdit,
    RestartedShell,
    FinalEvidence,
}
impl GuestStep {
    fn cap(self) -> Duration {
        match self {
            Self::BootShell | Self::ConsoleReady | Self::ResumeReady | Self::RestartedShell => {
                bounds::READINESS
            }
            Self::PreserveBeforeSuspend | Self::FinalEvidence => bounds::FAILURE_EVIDENCE,
            _ => bounds::TYPED_ACTION,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Intent {
    pub id: String,
    pub action: Action,
    pub dispatched_utc: String,
    pub reconciled: bool,
    pub completion_rule: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Incomplete,
    Passed,
    Failed,
}
impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Self::Incomplete => "incomplete",
            Self::Passed => "pass",
            Self::Failed => "fail",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Run {
    pub id: String,
    pub operator: String,
    pub vmx: String,
    pub lane_root: String,
    /// Durable controller PID/start, job name and worker PID/start records. The
    /// adapter validates these against its OS proof before reconciliation.
    pub worker_provenance: Value,
    pub phase: String,
    pub state: State,
    pub state_proof: Option<String>,
    pub intents: Vec<Intent>,
    pub first_failure: Option<String>,
    pub verdict: Verdict,
    pub package_and_artifacts: Value,
    pub evidence: Value,
    pub release_intent: bool,
    pub lock_release: Option<Value>,
}
impl Run {
    pub fn new(
        id: &str,
        operator: &str,
        vmx: &str,
        lane_root: &str,
        worker_provenance: Value,
    ) -> io::Result<Self> {
        validate_id(id)?;
        validate_worker_provenance(&worker_provenance)?;
        if [operator, vmx, lane_root]
            .iter()
            .any(|s| s.is_empty() || s.len() > 32767 || s.contains('\0'))
        {
            return Err(record::invalid("missing run operator or canonical binding"));
        }
        Ok(Self {
            id: id.into(),
            operator: operator.into(),
            vmx: vmx.into(),
            lane_root: lane_root.into(),
            worker_provenance,
            phase: "preparing".into(),
            state: State::Unknown,
            state_proof: None,
            intents: Vec::new(),
            first_failure: None,
            verdict: Verdict::Incomplete,
            package_and_artifacts: Value::Null,
            evidence: Value::Null,
            release_intent: false,
            lock_release: None,
        })
    }
    fn fail(&mut self, error: impl ToString) {
        if self.first_failure.is_none() {
            self.first_failure = Some(error.to_string());
        }
        self.verdict = Verdict::Failed;
    }
    pub fn record(&self) -> Value {
        Value::object([
            ("schema", Value::Number(1)),
            ("run", Value::text(&self.id)),
            ("operator", Value::text(&self.operator)),
            ("vmx", Value::text(&self.vmx)),
            ("lane_root", Value::text(&self.lane_root)),
            ("workers", self.worker_provenance.clone()),
            ("phase", Value::text(&self.phase)),
            ("state", Value::text(self.state.name())),
            (
                "state_proof",
                self.state_proof.as_ref().map_or(Value::Null, Value::text),
            ),
            (
                "intents",
                Value::Array(
                    self.intents
                        .iter()
                        .map(|i| {
                            Value::object([
                                ("id", Value::text(&i.id)),
                                ("action", Value::text(i.action.name())),
                                ("dispatched_utc", Value::text(&i.dispatched_utc)),
                                ("reconciled", Value::Bool(i.reconciled)),
                                (
                                    "completion_rule",
                                    i.completion_rule.as_ref().map_or(Value::Null, Value::text),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "first_failure",
                self.first_failure.as_ref().map_or(Value::Null, Value::text),
            ),
            ("verdict", Value::text(self.verdict.name())),
            ("package_and_artifacts", self.package_and_artifacts.clone()),
            ("evidence", self.evidence.clone()),
            ("release_intent", Value::Bool(self.release_intent)),
            (
                "lock_release",
                self.lock_release.clone().unwrap_or(Value::Null),
            ),
        ])
    }
    pub fn parse(value: &Value) -> io::Result<Self> {
        if value.encode().len() > bounds::RECORD_BYTES {
            return Err(record::invalid("VM record cap"));
        }
        value.fields(&[
            "schema",
            "run",
            "operator",
            "vmx",
            "lane_root",
            "workers",
            "phase",
            "state",
            "state_proof",
            "intents",
            "first_failure",
            "verdict",
            "package_and_artifacts",
            "evidence",
            "release_intent",
            "lock_release",
        ])?;
        if value.get("schema")?.number()? != 1 {
            return Err(record::invalid("unknown VM record schema"));
        }
        let mut run = Self::new(
            value.get("run")?.string()?,
            value.get("operator")?.string()?,
            value.get("vmx")?.string()?,
            value.get("lane_root")?.string()?,
            value.get("workers")?.clone(),
        )?;
        run.phase = value.get("phase")?.string()?.into();
        if !matches!(
            run.phase.as_str(),
            "preparing"
                | "recovering"
                | "power-intent"
                | "finalized-off"
                | "suspended-needs-recovery"
        ) {
            return Err(record::invalid("unknown VM phase"));
        }
        run.state = match value.get("state")?.string()? {
            "off" => State::Off,
            "running" => State::Running,
            "suspended" => State::Suspended,
            "transitioning" => State::Transitioning,
            "unknown" => State::Unknown,
            _ => return Err(record::invalid("unknown VM state")),
        };
        run.state_proof = optional_text(value.get("state_proof")?, 4096)?;
        run.first_failure = optional_text(value.get("first_failure")?, 16384)?;
        run.verdict = match value.get("verdict")?.string()? {
            "incomplete" => Verdict::Incomplete,
            "pass" => Verdict::Passed,
            "fail" => Verdict::Failed,
            _ => return Err(record::invalid("unknown VM verdict")),
        };
        let intents = value.get("intents")?.array()?;
        if intents.len() > 16 {
            return Err(record::invalid("VM intent count exceeds cap"));
        }
        for (n, value) in intents.iter().enumerate() {
            value.fields(&[
                "id",
                "action",
                "dispatched_utc",
                "reconciled",
                "completion_rule",
            ])?;
            let id = value.get("id")?.string()?.to_owned();
            if id != format!("{}-intent-{}", run.id, n + 1) {
                return Err(record::invalid("VM intent identity/order mismatch"));
            }
            let action = match value.get("action")?.string()? {
                "start-nogui" => Action::Start,
                "suspend-hard" => Action::Suspend,
                "stop-hard" => Action::Stop,
                "guest-restart-enter" => Action::GuestRestartEnter,
                _ => return Err(record::invalid("unknown VM power action")),
            };
            let dispatched_utc = required_text(value.get("dispatched_utc")?, 128)?;
            let reconciled = value.get("reconciled")?.boolean()?;
            let completion_rule = optional_text(value.get("completion_rule")?, 4096)?;
            if reconciled != completion_rule.is_some() || (!reconciled && n + 1 != intents.len()) {
                return Err(record::invalid("invalid reconciled intent sequence"));
            }
            run.intents.push(Intent {
                id,
                action,
                dispatched_utc,
                reconciled,
                completion_rule,
            });
        }
        run.package_and_artifacts = value.get("package_and_artifacts")?.clone();
        run.evidence = value.get("evidence")?.clone();
        run.release_intent = value.get("release_intent")?.boolean()?;
        run.lock_release = match value.get("lock_release")? {
            Value::Null => None,
            x => Some(x.clone()),
        };
        let final_phase = matches!(
            run.phase.as_str(),
            "finalized-off" | "suspended-needs-recovery"
        );
        if (run.verdict == Verdict::Passed && run.first_failure.is_some())
            || (run.verdict == Verdict::Failed && run.first_failure.is_none())
            || (run.state == State::Off && run.phase == "suspended-needs-recovery")
            || (run.state != State::Off && run.phase == "finalized-off")
            || (run.state != State::Suspended && run.phase == "suspended-needs-recovery")
            || (final_phase
                && (run.state_proof.is_none()
                    || run.intents.iter().any(|i| !i.reconciled)
                    || run.verdict == Verdict::Incomplete))
            || (run.release_intent && !final_phase)
            || (run.lock_release.is_some() && !run.release_intent)
        {
            return Err(record::invalid("inconsistent VM final/release proof"));
        }
        Ok(run)
    }
}

fn required_text(value: &Value, cap: usize) -> io::Result<String> {
    let text = value.string()?;
    if text.is_empty() || text.len() > cap || text.contains('\0') {
        return Err(record::invalid("empty or oversized VM record field"));
    }
    Ok(text.into())
}
fn optional_text(value: &Value, cap: usize) -> io::Result<Option<String>> {
    if value == &Value::Null {
        Ok(None)
    } else {
        required_text(value, cap).map(Some)
    }
}

fn validate_worker_provenance(value: &Value) -> io::Result<()> {
    fn process(value: &Value) -> io::Result<(u64, u64)> {
        value.fields(&["pid", "start_time"])?;
        let pid = value.get("pid")?.number()?;
        let start = value.get("start_time")?.number()?;
        if pid == 0 || pid > u64::from(u32::MAX) || start == 0 {
            return Err(record::invalid("invalid recorded process identity"));
        }
        Ok((pid, start))
    }
    value.fields(&["controller", "job", "workers"])?;
    process(value.get("controller")?)?;
    let job = value.get("job")?.string()?;
    if !job.starts_with("Local\\MirrorLane-")
        || job.len() <= 17
        || job.len() > 200
        || !job[17..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(record::invalid("invalid recorded job identity"));
    }
    let workers = value.get("workers")?.array()?;
    if workers.len() > 4096 {
        return Err(record::invalid("worker identity count cap"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for worker in workers {
        if !seen.insert(process(worker)?) {
            return Err(record::invalid("duplicate worker identity"));
        }
    }
    Ok(())
}

/// A positive state proof comes from the eventual D11 state adapter, never just
/// absence from vmrun list. Completing an intent additionally names the verified
/// terminal rule that excludes future work already accepted by VMware service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub state: State,
    pub state_proof: String,
    pub completed_intent: Option<String>,
    pub terminal_rule: Option<String>,
    pub excludes_late_service_work: bool,
}
impl Observation {
    fn valid(&self) -> bool {
        !self.state_proof.is_empty()
            && self.state_proof.len() <= 4096
            && !matches!(self.state, State::Unknown | State::Transitioning)
    }
    fn completes(&self, intent: &Intent) -> bool {
        self.valid()
            && self.completed_intent.as_deref() == Some(&intent.id)
            && self
                .terminal_rule
                .as_ref()
                .is_some_and(|s| !s.is_empty() && s.len() <= 4096)
            && self.excludes_late_service_work
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WorkerProof {
    pub recorded_identities_match: bool,
    pub job_empty: bool,
    pub prior_controller_exited: bool,
}

/// Adapters must fail closed on inaccessible/mismatched records, check all I/O
/// against the passed absolute deadline, and preserve partial evidence. No real
/// VMware implementation of this trait is provided by 0015c.
pub trait Adapter {
    fn kind(&self) -> BackendKind;
    fn now(&self) -> Instant;
    fn utc(&self) -> String;
    fn wait(&mut self, duration: Duration) -> io::Result<()>;
    /// Check canonical binding, same operator or recorded named authorization,
    /// retained lane journal, capability/staging provenance, and fresh/exclusive
    /// lease. Recovery may reconcile a lagging release field with owner history;
    /// it never retakes a released lease or depends on the old checkout.
    /// Preserve the prior lease/release record in durable acquisition history;
    /// recover() clears its old release fields before recording the new holder.
    fn acquire(&mut self, run: &mut Run, recovering: bool, deadline: Instant) -> io::Result<()>;
    fn journal(&mut self, run: &Run) -> io::Result<()>;
    fn stage(&mut self, run: &Run, deadline: Instant) -> io::Result<Value>;
    fn verify_artifacts(&mut self, run: &Run, deadline: Instant) -> io::Result<()>;
    fn workers(
        &mut self,
        run: &Run,
        prior_controller: bool,
        deadline: Instant,
    ) -> io::Result<WorkerProof>;
    fn observe(&mut self, run: &Run, deadline: Instant) -> io::Result<Observation>;
    /// The run already contains a durably journaled intent. GuestRestartEnter
    /// sends its single Enter key; it must never map to a host reset/start.
    /// Append each suspended worker's PID/start to worker_provenance and flush
    /// journal(run) from Job::spawn's before_resume callback before execution.
    fn dispatch(&mut self, run: &mut Run, action: Action, deadline: Instant) -> io::Result<()>;
    fn guest(&mut self, run: &Run, step: GuestStep, deadline: Instant) -> io::Result<()>;
    /// Copy and durably finish the complete current serial epoch. Failure must
    /// leave the suspended VM untouched because start could replace that epoch.
    fn preserve_suspended_serial(&mut self, run: &Run, deadline: Instant) -> io::Result<()>;
    fn release_keys_and_evidence(&mut self, run: &Run, deadline: Instant) -> io::Result<Value>;
    fn final_record(&mut self, run: &Run) -> io::Result<()>;
    /// Through the still-held owner handle, publish run/lease + released=UTC and
    /// flush. Returned identity/time is then recorded in both durable records.
    fn publish_owner_release(&mut self, run: &Run) -> io::Result<Value>;
    fn close_lease(&mut self) -> io::Result<()>;
}

fn admitted(b: &impl Adapter) -> io::Result<()> {
    if b.kind() == BackendKind::Live {
        require_live_entry()?;
    }
    Ok(())
}
fn phase(b: &impl Adapter, end: Deadline, cap: Duration) -> io::Result<Deadline> {
    end.child_at(b.now(), cap)
}
fn check(b: &impl Adapter, end: Deadline) -> io::Result<()> {
    end.remaining_at(b.now()).map(|_| ())
}
fn journal(b: &mut impl Adapter, run: &Run) -> io::Result<()> {
    if run.record().encode().len() > bounds::RECORD_BYTES {
        return Err(record::invalid("VM journal exceeds record cap"));
    }
    b.journal(run)
}

fn proven_state(
    b: &mut impl Adapter,
    run: &Run,
    pending: Option<&Intent>,
    end: Deadline,
) -> io::Result<Observation> {
    let end = phase(b, end, bounds::STATE_PHASE)?;
    let mut previous = None;
    for _ in 0..bounds::STATE_QUERIES {
        check(b, end)?;
        let current = b.observe(run, end.expires_at())?;
        check(b, end)?;
        let usable = pending.map_or_else(|| current.valid(), |intent| current.completes(intent));
        if usable {
            if previous.as_ref() == Some(&current) {
                return Ok(current);
            }
            previous = Some(current);
        } else {
            previous = None;
        }
        let pause = Duration::from_millis(500).min(end.remaining_at(b.now())?);
        b.wait(pause)?;
    }
    Err(io::Error::other(
        "no two verified state/completion indicators within state query cap",
    ))
}

fn accept_observation(run: &mut Run, observation: &Observation) {
    run.state = observation.state;
    run.state_proof = Some(observation.state_proof.clone());
}

fn command(
    b: &mut impl Adapter,
    run: &mut Run,
    action: Action,
    end: Deadline,
    local_dispatch: &mut Option<Instant>,
) -> io::Result<()> {
    if run.intents.iter().any(|i| !i.reconciled) || run.intents.len() >= 16 {
        return Err(io::Error::other(
            "unresolved power intent or intent cap; refusing another action",
        ));
    }
    let command_end = phase(
        b,
        end,
        if action == Action::GuestRestartEnter {
            bounds::TYPED_ACTION
        } else {
            bounds::LIFECYCLE_COMMAND
        },
    )?;
    let intent = Intent {
        id: format!("{}-intent-{}", run.id, run.intents.len() + 1),
        action,
        dispatched_utc: b.utc(),
        reconciled: false,
        completion_rule: None,
    };
    run.intents.push(intent);
    run.state = State::Transitioning;
    run.state_proof = None;
    run.phase = "power-intent".into();
    journal(b, run)?;
    *local_dispatch = Some(b.now());
    b.dispatch(run, action, command_end.expires_at())?;
    check(b, command_end)?;
    let workers = b.workers(run, false, command_end.expires_at())?;
    check(b, command_end)?;
    if !workers.recorded_identities_match || !workers.job_empty {
        return Err(io::Error::other("command workers have not completed"));
    }
    let observation = proven_state(b, run, run.intents.last(), end)?;
    if observation.state != action.resulting_state() {
        return Err(io::Error::other(
            "power operation completed in unexpected state",
        ));
    }
    let intent = run.intents.last_mut().unwrap();
    intent.reconciled = true;
    intent.completion_rule = observation.terminal_rule.clone();
    accept_observation(run, &observation);
    *local_dispatch = None;
    journal(b, run)
}

fn reconcile(
    b: &mut impl Adapter,
    run: &mut Run,
    restarted: bool,
    local_dispatch: Option<Instant>,
    end: Deadline,
) -> io::Result<()> {
    let proof = b.workers(run, restarted, end.expires_at())?;
    check(b, end)?;
    if !proof.recorded_identities_match
        || !proof.job_empty
        || (restarted && !proof.prior_controller_exited)
    {
        return Err(io::Error::other(
            "prior controller/worker PID-start or empty-job proof missing",
        ));
    }
    let unresolved: Vec<usize> = run
        .intents
        .iter()
        .enumerate()
        .filter_map(|(n, i)| (!i.reconciled).then_some(n))
        .collect();
    if unresolved.len() > 1 {
        return Err(record::invalid("multiple unreconciled intents"));
    }
    if let Some(&index) = unresolved.first() {
        let elapsed = if restarted {
            Duration::ZERO
        } else {
            local_dispatch
                .and_then(|start| b.now().checked_duration_since(start))
                .unwrap_or(Duration::ZERO)
        };
        let delay = bounds::LIFECYCLE_COMMAND.saturating_sub(elapsed);
        if delay >= end.remaining_at(b.now())? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "insufficient intent reconciliation budget",
            ));
        }
        if !delay.is_zero() {
            b.wait(delay)?;
        }
        check(b, end)?;
        let observation = proven_state(b, run, Some(&run.intents[index]), end)?;
        run.intents[index].reconciled = true;
        run.intents[index].completion_rule = observation.terminal_rule.clone();
        accept_observation(run, &observation);
    } else {
        let observation = proven_state(b, run, None, end)?;
        accept_observation(run, &observation);
    }
    journal(b, run)
}

fn guest(b: &mut impl Adapter, run: &Run, step: GuestStep, end: Deadline) -> io::Result<()> {
    let end = phase(b, end, step.cap())?;
    b.guest(run, step, end.expires_at())?;
    check(b, end)
}

fn finalize(b: &mut impl Adapter, run: &mut Run, end: Deadline) -> io::Result<()> {
    if run.intents.iter().any(|i| !i.reconciled)
        || run.state_proof.is_none()
        || !matches!(run.state, State::Off | State::Suspended)
    {
        return Err(io::Error::other(
            "unknown/running/unresolved VM retains its lease",
        ));
    }
    if run.state == State::Off {
        let verify_end = phase(b, end, bounds::PACKAGE_STAGE)?;
        if let Err(error) = b
            .verify_artifacts(run, verify_end.expires_at())
            .and_then(|_| check(b, verify_end))
        {
            run.fail(error);
        }
        run.phase = "finalized-off".into();
    } else {
        run.fail("suspended-needs-recovery");
        run.phase = "suspended-needs-recovery".into();
    }
    if run.verdict == Verdict::Incomplete {
        run.verdict = Verdict::Passed;
    }
    check(b, end)?;
    // A crash at any of these boundaries remains recoverable from LaneRoot.
    b.final_record(run)?;
    run.release_intent = true;
    journal(b, run)?;
    b.final_record(run)?;
    run.lock_release = Some(b.publish_owner_release(run)?);
    journal(b, run)?;
    b.final_record(run)?;
    b.close_lease()
}

fn failed_cleanup(
    b: &mut impl Adapter,
    run: &mut Run,
    local_dispatch: Option<Instant>,
) -> io::Result<()> {
    let end = Deadline::at(b.now(), bounds::FAILURE_TOTAL)?;
    let evidence_end = phase(b, end, bounds::FAILURE_EVIDENCE)?;
    match b
        .release_keys_and_evidence(run, evidence_end.expires_at())
        .and_then(|value| {
            check(b, evidence_end)?;
            Ok(value)
        }) {
        Ok(evidence) => run.evidence = evidence,
        Err(error) => run.fail(error),
    }
    reconcile(b, run, false, local_dispatch, end)?;
    // A preflight failure can discover an already-running VM before this run
    // has dispatched any power intent. That is not this run's cleanup stop.
    if run.state == State::Running && !run.intents.is_empty() {
        command(b, run, Action::Stop, end, &mut None)?;
    }
    finalize(b, run, end)
}

/// Execute the generic lifecycle; exact 0015d scene/keyboard behavior belongs to
/// the guest adapter and is bounded by the step-specific deadlines above.
pub fn run_case(b: &mut impl Adapter, run: &mut Run, case: Case) -> io::Result<()> {
    admitted(b)?;
    if run.phase != "preparing"
        || !run.intents.is_empty()
        || run.verdict != Verdict::Incomplete
        || run.release_intent
        || run.lock_release.is_some()
    {
        return Err(record::invalid(
            "new cases require a fresh run; use same-run recovery",
        ));
    }
    let end = Deadline::at(b.now(), bounds::ACTIVE_CASE)?;
    b.acquire(run, false, end.expires_at())?;
    run.phase = "preparing".into();
    journal(b, run)?;
    let mut local_dispatch = None;
    let result = (|| {
        let off = proven_state(b, run, None, end)?;
        if off.state != State::Off {
            return Err(io::Error::other("new case requires proven off state"));
        }
        accept_observation(run, &off);
        let stage_end = phase(b, end, bounds::PACKAGE_STAGE)?;
        run.package_and_artifacts = b.stage(run, stage_end.expires_at())?;
        check(b, stage_end)?;
        journal(b, run)?;
        command(b, run, Action::Start, end, &mut local_dispatch)?;
        for step in [
            GuestStep::BootShell,
            GuestStep::EnterConsole,
            GuestStep::ConsoleReady,
            GuestStep::Status,
        ] {
            guest(b, run, step, end)?;
        }
        match case {
            Case::Boot => (),
            Case::SuspendResume => {
                guest(b, run, GuestStep::Echo, end)?;
                guest(b, run, GuestStep::PreserveBeforeSuspend, end)?;
                command(b, run, Action::Suspend, end, &mut local_dispatch)?;
                b.wait(Duration::from_secs(2))?;
                check(b, end)?;
                command(b, run, Action::Start, end, &mut local_dispatch)?;
                guest(b, run, GuestStep::ResumeReady, end)?;
                guest(b, run, GuestStep::ResumeStatus, end)?;
            }
            Case::Restart => {
                guest(b, run, GuestStep::RestartEdit, end)?;
                command(b, run, Action::GuestRestartEnter, end, &mut local_dispatch)?;
                for step in [
                    GuestStep::RestartedShell,
                    GuestStep::EnterConsole,
                    GuestStep::ConsoleReady,
                    GuestStep::Status,
                ] {
                    guest(b, run, step, end)?;
                }
            }
        }
        guest(b, run, GuestStep::FinalEvidence, end)?;
        command(b, run, Action::Stop, end, &mut local_dispatch)?;
        finalize(b, run, end)
    })();
    if let Err(error) = result {
        run.fail(error);
        journal(b, run)?;
        // If finalization already published an eligible release, never issue
        // cleanup power actions or reacquire that lease after a journal failure.
        if run.release_intent {
            return Err(io::Error::other(
                "finalization interrupted; use same-run recovery",
            ));
        }
        failed_cleanup(b, run, local_dispatch)?;
    }
    Ok(())
}

pub fn recover(b: &mut impl Adapter, run: &mut Run, power_off: bool) -> io::Result<()> {
    admitted(b)?;
    let end = Deadline::at(
        b.now(),
        if power_off {
            bounds::RECOVER_POWER_OFF
        } else {
            bounds::RECOVER
        },
    )?;
    b.acquire(run, true, end.expires_at())?;
    run.release_intent = false;
    run.lock_release = None;
    run.phase = "recovering".into();
    if run.verdict == Verdict::Incomplete {
        run.fail("interrupted VM run recovered");
    }
    journal(b, run)?;
    reconcile(b, run, true, None, end)?;
    if !power_off
        && matches!(
            run.state,
            State::Running | State::Unknown | State::Transitioning
        )
    {
        journal(b, run)?;
        return Err(io::Error::other(
            "VM retains lease; explicit PowerOff recovery required",
        ));
    }
    if power_off {
        let mut local_dispatch = None;
        if run.state == State::Suspended {
            let copy_end = phase(b, end, bounds::FAILURE_EVIDENCE)?;
            b.preserve_suspended_serial(run, copy_end.expires_at())?;
            check(b, copy_end)?;
            command(b, run, Action::Start, end, &mut local_dispatch)?;
        }
        if run.state == State::Running {
            command(b, run, Action::Stop, end, &mut local_dispatch)?;
        }
    }
    finalize(b, run, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone)]
    struct Fake {
        now: Instant,
        kind: BackendKind,
        state: State,
        terminal: bool,
        held: bool,
        trace: Vec<String>,
        actions: Vec<Action>,
        saved: Vec<Run>,
        final_run: Option<Run>,
        fail: Option<&'static str>,
        workers: WorkerProof,
        artifact_ok: bool,
        evidence_available: bool,
    }
    impl Fake {
        fn new(state: State) -> Self {
            Self {
                now: Instant::now(),
                kind: BackendKind::Synthetic,
                state,
                terminal: true,
                held: false,
                trace: Vec::new(),
                actions: Vec::new(),
                saved: Vec::new(),
                final_run: None,
                fail: None,
                workers: WorkerProof {
                    recorded_identities_match: true,
                    job_empty: true,
                    prior_controller_exited: true,
                },
                artifact_ok: true,
                evidence_available: true,
            }
        }
        fn event(&mut self, name: &'static str) -> io::Result<()> {
            self.trace.push(name.into());
            if self.fail == Some(name) {
                return Err(io::Error::other(format!("injected {name}")));
            }
            Ok(())
        }
    }
    impl Adapter for Fake {
        fn kind(&self) -> BackendKind {
            self.kind
        }
        fn now(&self) -> Instant {
            self.now
        }
        fn utc(&self) -> String {
            "2026-10-13T00:00:00Z".into()
        }
        fn wait(&mut self, d: Duration) -> io::Result<()> {
            self.now += d;
            Ok(())
        }
        fn acquire(&mut self, run: &mut Run, _: bool, _: Instant) -> io::Result<()> {
            self.event("acquire")?;
            if run.lock_release.is_some() {
                self.trace.push("retained-release-history".into());
                self.saved.push(run.clone());
            }
            self.held = true;
            Ok(())
        }
        fn journal(&mut self, run: &Run) -> io::Result<()> {
            self.event("journal")?;
            self.saved.push(run.clone());
            Ok(())
        }
        fn stage(&mut self, _: &Run, _: Instant) -> io::Result<Value> {
            assert!(self.held);
            assert_eq!(self.state, State::Off);
            self.event("stage")?;
            Ok(Value::text("verified-generation"))
        }
        fn verify_artifacts(&mut self, _: &Run, _: Instant) -> io::Result<()> {
            self.event("hash-check")?;
            if self.artifact_ok {
                Ok(())
            } else {
                Err(io::Error::other("staged image changed"))
            }
        }
        fn workers(&mut self, _: &Run, _: bool, _: Instant) -> io::Result<WorkerProof> {
            self.event("worker-proof")?;
            Ok(self.workers)
        }
        fn observe(&mut self, run: &Run, _: Instant) -> io::Result<Observation> {
            self.event("state")?;
            Ok(Observation {
                state: self.state,
                state_proof: "synthetic positive state proof".into(),
                completed_intent: run.intents.last().map(|i| i.id.clone()),
                terminal_rule: self
                    .terminal
                    .then(|| "synthetic terminal service cancellation".into()),
                excludes_late_service_work: self.terminal,
            })
        }
        fn dispatch(&mut self, run: &mut Run, action: Action, _: Instant) -> io::Result<()> {
            assert_eq!(
                self.saved.last().unwrap().intents.last(),
                run.intents.last()
            );
            assert!(!run.intents.last().unwrap().reconciled);
            self.actions.push(action);
            self.event("dispatch")?;
            self.state = action.resulting_state();
            Ok(())
        }
        fn guest(&mut self, _: &Run, step: GuestStep, _: Instant) -> io::Result<()> {
            self.trace.push(format!("guest-{step:?}"));
            self.event("guest")
        }
        fn preserve_suspended_serial(&mut self, _: &Run, _: Instant) -> io::Result<()> {
            assert_eq!(self.state, State::Suspended);
            self.event("serial-durable")
        }
        fn release_keys_and_evidence(&mut self, _: &Run, _: Instant) -> io::Result<Value> {
            self.event("failure-evidence")?;
            Ok(Value::Bool(self.evidence_available))
        }
        fn final_record(&mut self, run: &Run) -> io::Result<()> {
            self.event("final")?;
            self.final_run = Some(run.clone());
            Ok(())
        }
        fn publish_owner_release(&mut self, run: &Run) -> io::Result<Value> {
            assert!(run.release_intent);
            assert!(self.final_run.is_some());
            self.event("owner-release")?;
            Ok(Value::text("synthetic-run-lease-utc"))
        }
        fn close_lease(&mut self) -> io::Result<()> {
            self.event("close-lease")?;
            self.held = false;
            Ok(())
        }
    }
    fn run() -> Run {
        Run::new(
            "synthetic-vm-1",
            "codex/task",
            "C:/fixture/Mirror.vmx",
            "C:/fixture/lane",
            Value::object([
                (
                    "controller",
                    Value::object([
                        ("pid", Value::Number(1)),
                        ("start_time", Value::Number(100)),
                    ]),
                ),
                ("job", Value::text("Local\\MirrorLane-synthetic-vm-1")),
                ("workers", Value::Array(vec![])),
            ]),
        )
        .unwrap()
    }
    fn pending(run: &mut Run, action: Action) {
        run.intents.push(Intent {
            id: "synthetic-vm-1-intent-1".into(),
            action,
            dispatched_utc: "untrusted clock after restart".into(),
            reconciled: false,
            completion_rule: None,
        });
    }

    #[test]
    fn live_entry_and_recovery_stay_disabled_even_after_fake_success() {
        let mut fake = Fake::new(State::Off);
        run_case(&mut fake, &mut run(), Case::Boot).unwrap();
        fake.kind = BackendKind::Live;
        fake.trace.clear();
        assert!(run_case(&mut fake, &mut run(), Case::Boot).is_err());
        assert!(recover(&mut fake, &mut run(), true).is_err());
        assert!(fake.trace.is_empty());
        assert!(require_live_entry().is_err());
    }
    #[test]
    fn cases_journal_every_power_intent_including_restart_and_keep_suspend_lease() {
        for (case, expected) in [
            (Case::Boot, vec![Action::Start, Action::Stop]),
            (
                Case::SuspendResume,
                vec![Action::Start, Action::Suspend, Action::Start, Action::Stop],
            ),
            (
                Case::Restart,
                vec![Action::Start, Action::GuestRestartEnter, Action::Stop],
            ),
        ] {
            let mut fake = Fake::new(State::Off);
            let mut run = run();
            run_case(&mut fake, &mut run, case).unwrap();
            assert_eq!(fake.actions, expected);
            assert_eq!(run.verdict, Verdict::Passed);
            assert!(!fake.held);
            assert!(run.intents.iter().all(|i| i.reconciled));
            assert_eq!(
                fake.trace.iter().filter(|s| *s == "owner-release").count(),
                1
            );
        }
    }
    #[test]
    fn quiet_off_snapshots_after_full_wait_never_prove_delayed_start_completed() {
        let mut fake = Fake::new(State::Off);
        fake.terminal = false;
        let start = fake.now;
        let mut run = run();
        pending(&mut run, Action::Start);
        assert!(recover(&mut fake, &mut run, true).is_err());
        assert!(fake.now.duration_since(start) >= Duration::from_secs(60));
        assert!(fake.now.duration_since(start) <= Duration::from_secs(75));
        assert!(fake.held);
        assert!(fake.actions.is_empty());
        assert!(fake.final_run.is_none());
        assert!(!run.intents[0].reconciled);
    }
    #[test]
    fn recovery_requires_controller_identity_and_empty_descendant_job_before_waiting() {
        for proof in [
            WorkerProof {
                prior_controller_exited: false,
                job_empty: true,
                recorded_identities_match: true,
            },
            WorkerProof {
                prior_controller_exited: true,
                job_empty: false,
                recorded_identities_match: true,
            },
            WorkerProof {
                prior_controller_exited: true,
                job_empty: true,
                recorded_identities_match: false,
            },
        ] {
            let mut fake = Fake::new(State::Off);
            fake.workers = proof;
            let start = fake.now;
            let mut run = run();
            pending(&mut run, Action::Start);
            assert!(recover(&mut fake, &mut run, true).is_err());
            assert_eq!(fake.now, start);
            assert!(fake.held);
            assert!(fake.actions.is_empty());
        }
    }
    #[test]
    fn plain_recovery_off_and_suspended_release_running_retains() {
        for state in [State::Off, State::Suspended, State::Running] {
            let mut fake = Fake::new(state);
            let mut run = run();
            let result = recover(&mut fake, &mut run, false);
            assert_eq!(result.is_ok(), state != State::Running);
            assert_eq!(fake.held, state == State::Running);
            assert!(fake.actions.is_empty());
            if state == State::Suspended {
                assert_eq!(run.phase, "suspended-needs-recovery");
            }
        }
    }
    #[test]
    fn poweroff_suspended_copies_serial_before_one_start_then_one_stop() {
        let mut fake = Fake::new(State::Suspended);
        let mut run = run();
        recover(&mut fake, &mut run, true).unwrap();
        assert_eq!(fake.actions, [Action::Start, Action::Stop]);
        assert!(
            fake.trace
                .iter()
                .position(|s| s == "serial-durable")
                .unwrap()
                < fake.trace.iter().position(|s| s == "dispatch").unwrap()
        );
        assert_eq!(run.state, State::Off);
        assert_eq!(run.verdict, Verdict::Failed);
        assert!(!fake.held);
        let mut fake = Fake::new(State::Suspended);
        fake.fail = Some("serial-durable");
        assert!(recover(&mut fake, &mut super::tests::run(), true).is_err());
        assert!(fake.actions.is_empty());
        assert!(fake.held);
    }
    #[test]
    fn running_poweroff_stops_once_and_unresolved_stop_never_repeats() {
        let mut fake = Fake::new(State::Running);
        let mut run = run();
        recover(&mut fake, &mut run, true).unwrap();
        assert_eq!(fake.actions, [Action::Stop]);
        let mut fake = Fake::new(State::Running);
        fake.terminal = false;
        let mut run = super::tests::run();
        pending(&mut run, Action::Stop);
        assert!(recover(&mut fake, &mut run, true).is_err());
        assert!(fake.actions.is_empty());
    }
    #[test]
    fn first_failure_survives_cleanup_and_disk_mutation_never_passes() {
        let mut fake = Fake::new(State::Off);
        fake.fail = Some("guest");
        fake.artifact_ok = false;
        let mut run = run();
        run_case(&mut fake, &mut run, Case::Boot).unwrap();
        assert_eq!(run.first_failure.as_deref(), Some("injected guest"));
        assert_eq!(run.verdict, Verdict::Failed);
        assert_eq!(run.state, State::Off);
        assert!(!fake.held);
    }
    #[test]
    fn crashes_before_final_or_owner_release_do_not_close_lease() {
        for failure in ["final", "owner-release"] {
            let mut fake = Fake::new(State::Off);
            fake.fail = Some(failure);
            let mut run = run();
            assert!(recover(&mut fake, &mut run, false).is_err());
            assert!(fake.held);
            assert!(!fake.trace.iter().any(|s| s == "close-lease"));
            if failure == "owner-release" {
                assert!(fake.final_run.as_ref().unwrap().release_intent);
            }
        }
    }
    #[test]
    fn durable_verdict_survives_later_recovery_without_bulk_evidence() {
        let mut fake = Fake::new(State::Off);
        let mut run = run();
        run.verdict = Verdict::Failed;
        run.first_failure = Some("original failure".into());
        run.evidence = Value::text("deleted old worktree: bulk evidence unavailable");
        fake.evidence_available = false;
        recover(&mut fake, &mut run, false).unwrap();
        assert_eq!(run.first_failure.as_deref(), Some("original failure"));
        assert_eq!(run.verdict, Verdict::Failed);
        assert!(!fake.held);
    }
    #[test]
    fn durable_root_roundtrip_recovers_without_original_checkout_or_bulk_files() {
        let mut fake = Fake::new(State::Running);
        let mut original = run();
        pending(&mut original, Action::Start);
        original.first_failure = Some("original boot failed".into());
        original.verdict = Verdict::Failed;
        original.evidence = Value::text("old checkout removed; evidence unavailable");
        let serialized = original.record();
        let mut recovered = Run::parse(&serialized).unwrap();
        assert_eq!(recovered.record(), serialized);
        recover(&mut fake, &mut recovered, true).unwrap();
        assert_eq!(
            recovered.first_failure.as_deref(),
            Some("original boot failed")
        );
        assert_eq!(fake.actions, [Action::Stop]);
        assert_eq!(
            Run::parse(&recovered.record()).unwrap().record(),
            recovered.record()
        );
    }
    #[test]
    fn strict_vm_records_reject_unknown_schema_fields_and_inconsistent_release() {
        let mut record = run().record();
        record.set("extra", Value::Bool(true)).unwrap();
        assert!(Run::parse(&record).is_err());
        let mut record = run().record();
        record.set("schema", Value::Number(2)).unwrap();
        assert!(Run::parse(&record).is_err());
        let mut record = run().record();
        record.set("release_intent", Value::Bool(true)).unwrap();
        assert!(Run::parse(&record).is_err());
        let mut record = run().record();
        record.set("workers", Value::Null).unwrap();
        assert!(Run::parse(&record).is_err());
        let mut value = run();
        pending(&mut value, Action::Start);
        value.intents[0].reconciled = true;
        assert!(Run::parse(&value.record()).is_err());
        let mut record = run().record();
        record
            .set("evidence", Value::text("x".repeat(bounds::RECORD_BYTES)))
            .unwrap();
        assert!(Run::parse(&record).is_err());
        let mut value = run();
        pending(&mut value, Action::Start);
        value.intents = vec![value.intents[0].clone(); 17];
        assert!(Run::parse(&value.record()).is_err());
    }

    #[test]
    fn new_case_on_already_running_vm_never_stops_or_stages_it() {
        let mut fake = Fake::new(State::Running);
        let mut run = run();
        assert!(run_case(&mut fake, &mut run, Case::Boot).is_err());
        assert!(fake.actions.is_empty());
        assert!(
            !fake
                .trace
                .iter()
                .any(|s| s == "stage" || s == "owner-release")
        );
        assert!(fake.held);
    }

    #[test]
    fn recovering_released_suspension_records_new_exclusive_lease_before_proof() {
        let mut first = Fake::new(State::Suspended);
        let mut run = run();
        recover(&mut first, &mut run, false).unwrap();
        assert!(run.release_intent && run.lock_release.is_some());
        let mut run = Run::parse(&run.record()).unwrap();
        let mut second = Fake::new(State::Suspended);
        second.workers.job_empty = false;
        assert!(recover(&mut second, &mut run, true).is_err());
        assert!(second.held);
        assert!(second.trace.iter().any(|x| x == "retained-release-history"));
        let durable_new_holder = second.saved.last().unwrap();
        assert_eq!(durable_new_holder.phase, "recovering");
        assert!(!durable_new_holder.release_intent);
        assert!(durable_new_holder.lock_release.is_none());
        assert!(Run::parse(&durable_new_holder.record()).is_ok());
        assert!(second.actions.is_empty());
    }
}
