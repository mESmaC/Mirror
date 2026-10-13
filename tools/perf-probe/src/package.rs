//! Package lifecycle shared by the Windows adapter and synthetic fault fixtures.
//! Publication alone never makes a package eligible for staging.
use crate::record::{self, Value};
use std::io;
use std::time::{Duration, Instant};

pub const SCHEMA: u64 = 1;
pub const CAPABILITIES: [(&str, &str); 3] = [
    ("ps2", "14cb7b21da14f76116cc3dd2503407bb15ec220d"),
    (
        "absolute_pointer",
        "3c87bcb581f54d1283d3a04ec25e5d9b81bb79c1",
    ),
    ("restart", "8708752ea6a99877eb772a47d2e0fb5c57dfa859"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub size: u64,
    pub sha256: String,
}
impl Artifact {
    pub fn record(&self) -> Value {
        Value::object([
            ("size", Value::Number(self.size)),
            ("sha256", Value::text(&self.sha256)),
        ])
    }
    pub fn parse(v: &Value) -> io::Result<Self> {
        v.fields(&["size", "sha256"])?;
        let hash = v.get("sha256")?.string()?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(record::invalid("invalid SHA256 identity"));
        }
        Ok(Self {
            size: v.get("size")?.number()?,
            sha256: hash.into(),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    pub path: String,
    pub artifact: Artifact,
}
impl SourceFile {
    pub fn record(&self) -> Value {
        Value::object([
            ("path", Value::text(&self.path)),
            ("artifact", self.artifact.record()),
        ])
    }
    pub fn parse(v: &Value) -> io::Result<Self> {
        v.fields(&["path", "artifact"])?;
        let path = v.get("path")?.string()?;
        validate_source_path(path)?;
        Ok(Self {
            path: path.into(),
            artifact: Artifact::parse(v.get("artifact")?)?,
        })
    }
}
pub fn validate_id(id: &str) -> io::Result<()> {
    if id.is_empty() || id.len() > 96 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(record::invalid("invalid run identifier"));
    }
    Ok(())
}
pub fn validate_source_path(path: &str) -> io::Result<()> {
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || path
            .split('/')
            .any(|p| matches!(p, "build" | "target" | ".tools" | ".git"))
    {
        return Err(record::invalid("unsafe or generated source inventory path"));
    }
    Ok(())
}
fn git_id(value: &str) -> io::Result<String> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(record::invalid("expected full Git object identity"));
    }
    Ok(value.into())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub package_id: String,
    pub commit: String,
    pub tree: String,
    pub release: bool,
    pub toolchain: String,
    pub capabilities: Vec<(String, String)>,
    pub inventory: Vec<SourceFile>,
    pub image: Artifact,
    pub descriptor: Artifact,
}
impl Manifest {
    pub fn record(&self) -> Value {
        Value::object([
            ("schema", Value::Number(SCHEMA)),
            ("package_id", Value::text(&self.package_id)),
            ("commit", Value::text(&self.commit)),
            ("tree", Value::text(&self.tree)),
            ("clean", Value::Bool(true)),
            (
                "profile",
                Value::text(if self.release { "release" } else { "debug" }),
            ),
            ("features", Value::Array(vec![])),
            ("toolchain", Value::text(&self.toolchain)),
            (
                "capabilities",
                Value::Array(
                    self.capabilities
                        .iter()
                        .map(|(name, commit)| {
                            Value::object([
                                ("name", Value::text(name)),
                                ("commit", Value::text(commit)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("oracle_version", Value::Number(1)),
            (
                "inventory",
                Value::Array(self.inventory.iter().map(SourceFile::record).collect()),
            ),
            ("image", self.image.record()),
            ("descriptor", self.descriptor.record()),
        ])
    }
    pub fn parse(v: &Value) -> io::Result<Self> {
        v.fields(&[
            "schema",
            "package_id",
            "commit",
            "tree",
            "clean",
            "profile",
            "features",
            "toolchain",
            "capabilities",
            "oracle_version",
            "inventory",
            "image",
            "descriptor",
        ])?;
        if v.get("schema")?.number()? != SCHEMA
            || v.get("oracle_version")?.number()? != 1
            || !v.get("clean")?.boolean()?
            || !v.get("features")?.array()?.is_empty()
        {
            return Err(record::invalid("unsupported or non-default package"));
        }
        let package_id = v.get("package_id")?.string()?.to_owned();
        validate_id(&package_id)?;
        let release = match v.get("profile")?.string()? {
            "debug" => false,
            "release" => true,
            _ => return Err(record::invalid("unknown package profile")),
        };
        let toolchain = v.get("toolchain")?.string()?.to_owned();
        if !toolchain.starts_with("rustc 1.99.0 ") {
            return Err(record::invalid("package toolchain does not match pin"));
        }
        let mut capabilities = Vec::new();
        for c in v.get("capabilities")?.array()? {
            c.fields(&["name", "commit"])?;
            capabilities.push((
                c.get("name")?.string()?.to_owned(),
                git_id(c.get("commit")?.string()?)?,
            ));
        }
        if capabilities.len() != CAPABILITIES.len()
            || capabilities.iter().zip(CAPABILITIES).any(
                |((name, commit), (expected, required_commit))| {
                    name != expected || commit != required_commit
                },
            )
        {
            return Err(record::invalid("missing or mismatched merged capabilities"));
        }
        let inventory = v
            .get("inventory")?
            .array()?
            .iter()
            .map(SourceFile::parse)
            .collect::<io::Result<Vec<_>>>()?;
        if inventory.is_empty() || inventory.windows(2).any(|w| w[0].path >= w[1].path) {
            return Err(record::invalid(
                "source inventory empty, duplicate or unsorted",
            ));
        }
        Ok(Self {
            package_id,
            commit: git_id(v.get("commit")?.string()?)?,
            tree: git_id(v.get("tree")?.string()?)?,
            release,
            toolchain,
            capabilities,
            inventory,
            image: Artifact::parse(v.get("image")?)?,
            descriptor: Artifact::parse(v.get("descriptor")?)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Incomplete,
    FinalizedValid,
    RecoveredInvalid,
    Failed,
    Blocked,
}
impl Verdict {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Incomplete => "incomplete",
            Self::FinalizedValid => "finalized-valid",
            Self::RecoveredInvalid => "recovered-invalid",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub phase: String,
    pub verdict: Verdict,
    pub first_failure: Option<String>,
    pub manifest: Option<Manifest>,
    pub abandoned: Vec<String>,
    pub no_live_build: bool,
    pub release_intent: bool,
    pub lock_release: Option<Value>,
}
impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: "preparing".into(),
            verdict: Verdict::Incomplete,
            first_failure: None,
            manifest: None,
            abandoned: vec![],
            no_live_build: false,
            release_intent: false,
            lock_release: None,
        }
    }
}
impl Progress {
    fn fail(&mut self, error: impl ToString) {
        if self.first_failure.is_none() {
            self.first_failure = Some(error.to_string());
        }
    }
    pub fn record(&self) -> Value {
        Value::object([
            ("schema", Value::Number(SCHEMA)),
            ("phase", Value::text(&self.phase)),
            ("verdict", Value::text(self.verdict.name())),
            (
                "first_failure",
                self.first_failure.as_ref().map_or(Value::Null, Value::text),
            ),
            (
                "manifest",
                self.manifest.as_ref().map_or(Value::Null, Manifest::record),
            ),
            (
                "abandoned_artifacts",
                Value::Array(self.abandoned.iter().map(Value::text).collect()),
            ),
            ("no_live_build", Value::Bool(self.no_live_build)),
            ("release_intent", Value::Bool(self.release_intent)),
            (
                "lock_release",
                self.lock_release.clone().unwrap_or(Value::Null),
            ),
        ])
    }
    pub fn parse(v: &Value) -> io::Result<Self> {
        v.fields(&[
            "schema",
            "phase",
            "verdict",
            "first_failure",
            "manifest",
            "abandoned_artifacts",
            "no_live_build",
            "release_intent",
            "lock_release",
        ])?;
        if v.get("schema")?.number()? != SCHEMA {
            return Err(record::invalid("unknown final schema"));
        }
        let verdict = match v.get("verdict")?.string()? {
            "incomplete" => Verdict::Incomplete,
            "finalized-valid" => Verdict::FinalizedValid,
            "recovered-invalid" => Verdict::RecoveredInvalid,
            "failed" => Verdict::Failed,
            "blocked" => Verdict::Blocked,
            _ => return Err(record::invalid("unknown verdict")),
        };
        let first_failure = match v.get("first_failure")? {
            Value::Null => None,
            x => Some(x.string()?.into()),
        };
        let manifest = match v.get("manifest")? {
            Value::Null => None,
            x => Some(Manifest::parse(x)?),
        };
        let p = Self {
            phase: v.get("phase")?.string()?.into(),
            verdict,
            first_failure,
            manifest,
            abandoned: v
                .get("abandoned_artifacts")?
                .array()?
                .iter()
                .map(|v| Ok(v.string()?.to_owned()))
                .collect::<io::Result<_>>()?,
            no_live_build: v.get("no_live_build")?.boolean()?,
            release_intent: v.get("release_intent")?.boolean()?,
            lock_release: match v.get("lock_release")? {
                Value::Null => None,
                x => Some(x.clone()),
            },
        };
        if !matches!(
            p.phase.as_str(),
            "preparing"
                | "validating"
                | "exporting"
                | "building"
                | "build-quiescent"
                | "publishing"
                | "published"
                | "stopping-workers"
                | "live-build-blocked"
                | "cleanup"
                | "recovering"
                | "recovery-blocked"
                | "finalized"
        ) {
            return Err(record::invalid("unknown package phase"));
        }
        if (p.phase == "finalized" && (!p.release_intent || !p.no_live_build))
            || (p.release_intent && p.phase != "finalized")
        {
            return Err(record::invalid("inconsistent package finalization state"));
        }
        if p.verdict == Verdict::FinalizedValid
            && (p.phase != "finalized"
                || !p.release_intent
                || p.manifest.is_none()
                || p.first_failure.is_some()
                || !p.no_live_build)
        {
            return Err(record::invalid("invalid finalized-valid proof"));
        }
        if let Some(proof) = &p.lock_release {
            if p.phase != "finalized" || !p.release_intent || !p.no_live_build {
                return Err(record::invalid(
                    "owner release without eligible final state",
                ));
            }
            validate_release_proof(proof, p.manifest.as_ref().map(|m| m.package_id.as_str()))?;
        }
        Ok(p)
    }
}

/// A journal write can precede its final.json publication. Only an existing,
/// well-formed durable final can retain validity; corrupt final data never falls
/// back to a more favorable journal snapshot.
pub fn load_prior(run_dir: &std::path::Path, journal_progress: &Value) -> io::Result<Progress> {
    match record::read(&run_dir.join("final.json")) {
        Ok(value) => Progress::parse(&value),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut p = Progress::parse(journal_progress)?;
            if p.verdict == Verdict::FinalizedValid {
                p.verdict = Verdict::Incomplete;
            }
            Ok(p)
        }
        Err(error) => Err(error),
    }
}

/// Validate retained evidence without reopening a possibly replaced lock path.
/// The native adapter additionally binds the run and slot to its root journal.
fn validate_release_proof(proof: &Value, package_id: Option<&str>) -> io::Result<()> {
    let state = proof.get("state")?.string()?;
    let mut fields = vec![
        "state",
        "slot",
        "directory",
        "owner",
        "run",
        "lease",
        "released",
    ];
    match state {
        "owner-record" => {}
        "detached" | "removed" => fields.extend(["detached_path", "detached_identity"]),
        _ => return Err(record::invalid("unknown owner release state")),
    }
    proof.fields(&fields)?;
    for key in [
        "state",
        "slot",
        "directory",
        "owner",
        "run",
        "lease",
        "released",
    ] {
        let value = proof.get(key)?.string()?;
        if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
            return Err(record::invalid("invalid owner release field"));
        }
    }
    let slot = std::path::Path::new(proof.get("slot")?.string()?);
    if !slot.is_absolute()
        || slot.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        || !matches!(
            slot.file_name().and_then(|s| s.to_str()),
            Some("qemu" | "qemu2")
        )
    {
        return Err(record::invalid("invalid owner release state or slot"));
    }
    if matches!(state, "detached" | "removed") {
        let detached_text = proof.get("detached_path")?.string()?;
        let detached = std::path::Path::new(detached_text);
        let suffix = detached
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| {
                s.strip_prefix(&format!(
                    "{}.released.",
                    slot.file_name().unwrap().to_string_lossy()
                ))
            })
            .ok_or_else(|| record::invalid("detached release must name a unique slot sibling"))?;
        let pieces: Vec<_> = suffix.split('-').collect();
        let slot_parent = slot
            .parent()
            .ok_or_else(|| record::invalid("slot has no parent"))?;
        let detached_parent = detached
            .parent()
            .ok_or_else(|| record::invalid("detached release has no parent"))?;
        if detached_text.len() > 4096
            || detached_text.chars().any(char::is_control)
            || !detached.is_absolute()
            || detached.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || !crate::paths::contains(slot_parent, detached_parent)
            || !crate::paths::contains(detached_parent, slot_parent)
            || pieces.len() != 3
            || pieces
                .iter()
                .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
            || proof.get("detached_identity")? != proof.get("directory")?
        {
            return Err(record::invalid(
                "detached release changes the verified generation",
            ));
        }
    }
    for key in ["directory", "owner"] {
        let identity = proof.get(key)?.string()?.as_bytes();
        if identity.len() != 25
            || identity[8] != b':'
            || !identity[..8]
                .iter()
                .chain(&identity[9..])
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(record::invalid("invalid owner release file identity"));
        }
    }
    let run = proof.get("run")?.string()?;
    validate_id(run)?;
    if package_id.is_some_and(|package| package != run)
        || !crate::paths::safe_id(proof.get("lease")?.string()?)
        || !crate::lease::utc(proof.get("released")?.string()?)
    {
        return Err(record::invalid(
            "owner release does not identify this package lease",
        ));
    }
    Ok(())
}

/// The Windows adapter owns the sole job and lease handles. A durable identity
/// journal exists before any worker is resumed. All deadlines are absolute.
pub trait Backend {
    fn now(&self) -> Instant;
    fn persist(&mut self, p: &Progress, final_record: bool) -> io::Result<()>;
    fn validate(&mut self, deadline: Instant) -> io::Result<()>;
    fn export(&mut self, deadline: Instant) -> io::Result<()>;
    fn build(&mut self, deadline: Instant) -> io::Result<()>;
    fn publish(&mut self, deadline: Instant) -> io::Result<Manifest>;
    fn terminate_workers(&mut self) -> io::Result<()>;
    fn prove_no_live_build(&mut self, recovery: bool, deadline: Instant) -> io::Result<()>;
    /// Only the canonical bound packages/<id>/source can be removed.
    fn cleanup(&mut self, deadline: Instant) -> io::Result<()>;
    fn scratch_path(&self) -> String;
    /// Recovery rechecks the durable manifest against retained published hashes;
    /// it never repairs/copies/publishes output in any checkout.
    fn revalidate(&mut self, manifest: &Manifest, deadline: Instant) -> io::Result<()>;
    /// Append+flush the release record through the held owner handle. The
    /// controller writes this returned proof to both journal and final before
    /// close/detach. On failure the owner record remains recoverable.
    fn publish_release(&mut self) -> io::Result<Value>;
    /// Close the released lease; optional identity-checked detach returns the
    /// same base proof with state=detached and its sibling path/identity.
    fn close_released(&mut self) -> io::Result<Option<Value>>;
    /// Called only after the detached proof is durable in journal and final.
    /// Remove only the verified sibling generation. Denial retains that proof
    /// and must not invalidate the package or block a fresh acquisition.
    fn cleanup_released(&mut self) -> io::Result<bool> {
        Ok(false)
    }
}
fn phase(b: &mut impl Backend, p: &mut Progress, name: &str) -> io::Result<()> {
    p.phase = name.into();
    b.persist(p, false)
}
fn deadline(b: &impl Backend, end: Instant, seconds: u64) -> io::Result<Instant> {
    let now = b.now();
    if now >= end {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "package deadline"));
    }
    Ok(end.min(now + Duration::from_secs(seconds)))
}

/// Executes the same production transition ordering in fake and real adapters.
pub fn run(b: &mut impl Backend) -> io::Result<Progress> {
    let end = b.now() + Duration::from_secs(900);
    let mut p = Progress::default();
    b.persist(&p, false)?;
    let operation = (|| {
        phase(b, &mut p, "validating")?;
        let d = deadline(b, end, 120)?;
        b.validate(d)?;
        phase(b, &mut p, "exporting")?;
        b.export(d)?;
        phase(b, &mut p, "building")?;
        b.build(deadline(b, end, 900)?)?;
        phase(b, &mut p, "build-quiescent")?;
        b.prove_no_live_build(false, deadline(b, end, 120)?)?;
        phase(b, &mut p, "publishing")?;
        p.manifest = Some(b.publish(deadline(b, end, 120)?)?);
        phase(b, &mut p, "published")
    })();
    if let Err(e) = operation {
        p.fail(e);
    }
    // Failure cleanup has its own total bound; an expired active budget must
    // still preserve the first failure and attempt worker termination/proof.
    let cleanup_end = b.now() + Duration::from_secs(105);
    phase(b, &mut p, "stopping-workers")?;
    if let Err(e) = b.terminate_workers() {
        p.fail(e);
    }
    // Cleanup never races even a failing worker or a retained descendant.
    if let Err(e) = b.prove_no_live_build(false, cleanup_end) {
        p.fail(e);
        p.verdict = Verdict::Blocked;
        phase(b, &mut p, "live-build-blocked")?;
        return Ok(p);
    }
    p.no_live_build = true;
    phase(b, &mut p, "cleanup")?;
    if let Err(e) = b.cleanup(cleanup_end) {
        p.fail(e);
        p.abandoned.push(b.scratch_path());
    }
    p.verdict = if p.first_failure.is_none() && p.manifest.is_some() {
        Verdict::FinalizedValid
    } else {
        Verdict::Failed
    };
    finalize(b, p)
}
/// The adapter has authenticated run/operator/root and taken a fresh or retained
/// same-run exclusive lease. It has loaded the previous durable final, if any.
pub fn recover(b: &mut impl Backend, previous: Option<Progress>) -> io::Result<Progress> {
    let end = b.now() + Duration::from_secs(120);
    let mut p = previous
        .map(|p| Progress::parse(&p.record()))
        .transpose()?
        .unwrap_or_default();
    let was_valid = p.verdict == Verdict::FinalizedValid;
    // Never inherit an old released lease's proof into the new lease record.
    p.lock_release = None;
    p.release_intent = false;
    p.no_live_build = false;
    // The previous final remains durable; this new recovery attempt has not yet
    // re-established its no-live/hash proof or committed a replacement final.
    if was_valid {
        p.verdict = Verdict::Incomplete;
    }
    phase(b, &mut p, "recovering")?;
    if let Err(e) = b.prove_no_live_build(true, end) {
        p.fail(e);
        p.verdict = Verdict::Blocked;
        phase(b, &mut p, "recovery-blocked")?;
        return Ok(p);
    }
    p.no_live_build = true;
    if was_valid {
        let manifest = p
            .manifest
            .clone()
            .ok_or_else(|| record::invalid("valid final without manifest"))?;
        if let Err(e) = b.revalidate(&manifest, end) {
            p.fail(e);
            p.verdict = Verdict::RecoveredInvalid;
        } else {
            p.verdict = Verdict::FinalizedValid;
        }
    } else {
        p.fail("interrupted package: publication cannot be resumed");
        p.verdict = Verdict::RecoveredInvalid;
    }
    if let Err(e) = b.cleanup(end) {
        p.abandoned.push(b.scratch_path());
        if !was_valid || p.verdict != Verdict::FinalizedValid {
            p.fail(e);
        }
    }
    finalize(b, p)
}
fn finalize(b: &mut impl Backend, mut p: Progress) -> io::Result<Progress> {
    if !p.no_live_build {
        return Err(record::invalid("release without no-live-build proof"));
    }
    p.phase = "finalized".into();
    p.release_intent = true;
    Progress::parse(&p.record())?;
    b.persist(&p, true)?;
    p.lock_release = Some(b.publish_release()?);
    Progress::parse(&p.record())?;
    b.persist(&p, true)?;
    if let Some(detached) = b.close_released()? {
        let owner = p.lock_release.as_ref().unwrap();
        if detached.get("state")?.string()? != "detached" {
            return Err(record::invalid(
                "close returned a non-detached release proof",
            ));
        }
        for key in ["slot", "directory", "owner", "run", "lease", "released"] {
            if detached.get(key)? != owner.get(key)? {
                return Err(record::invalid(
                    "detach changed the durable owner release identity",
                ));
            }
        }
        p.lock_release = Some(detached);
        Progress::parse(&p.record())?;
        // Failure here preserves the earlier valid owner-record final; the
        // adapter also journals detach intent before moving the directory.
        b.persist(&p, true)?;
        match b.cleanup_released() {
            Ok(true) => {
                p.lock_release
                    .as_mut()
                    .unwrap()
                    .set("state", Value::text("removed"))?;
                Progress::parse(&p.record())?;
                b.persist(&p, true)?;
            }
            Ok(false) => (),
            Err(error) => eprintln!("detached lease cleanup retained: {error}"),
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        now: Instant,
        fail: Option<&'static str>,
        calls: Vec<String>,
        published: bool,
        live: bool,
        finalized: bool,
        released: bool,
        expire_build: bool,
        detach: bool,
        remove_detached: bool,
        wrong_detach: bool,
        fail_detached_final: bool,
        durable_final: Option<Value>,
        phase_deadlines: Vec<Instant>,
    }
    impl Fake {
        fn new(fail: Option<&'static str>) -> Self {
            Self {
                now: Instant::now(),
                fail,
                calls: vec![],
                published: false,
                live: false,
                finalized: false,
                released: false,
                expire_build: false,
                detach: false,
                remove_detached: false,
                wrong_detach: false,
                fail_detached_final: false,
                durable_final: None,
                phase_deadlines: vec![],
            }
        }
        fn step(&mut self, s: &str) -> io::Result<()> {
            self.calls.push(s.into());
            if self.fail == Some(s) {
                Err(io::Error::other(s))
            } else {
                Ok(())
            }
        }
    }
    fn manifest() -> Manifest {
        Manifest {
            package_id: "test-1".into(),
            commit: "1".repeat(40),
            tree: "2".repeat(40),
            release: false,
            toolchain: "rustc 1.99.0 (fixture)".into(),
            capabilities: CAPABILITIES
                .iter()
                .map(|(n, c)| (n.to_string(), format!("{c:0<40}")))
                .collect(),
            inventory: vec![SourceFile {
                path: "Cargo.toml".into(),
                artifact: Artifact {
                    size: 1,
                    sha256: "a".repeat(64),
                },
            }],
            image: Artifact {
                size: 10,
                sha256: "b".repeat(64),
            },
            descriptor: Artifact {
                size: 1,
                sha256: "c".repeat(64),
            },
        }
    }
    impl Backend for Fake {
        fn now(&self) -> Instant {
            self.now
        }
        fn persist(&mut self, p: &Progress, final_record: bool) -> io::Result<()> {
            Progress::parse(&p.record()).unwrap();
            if final_record
                && self.fail_detached_final
                && p.lock_release
                    .as_ref()
                    .is_some_and(|v| v.get("state").unwrap().string().unwrap() == "detached")
            {
                return Err(io::Error::other("detached final write denied"));
            }
            self.step(if final_record { "final" } else { &p.phase })?;
            if final_record {
                assert!(p.no_live_build);
                self.finalized = true;
                self.durable_final = Some(p.record());
            }
            Ok(())
        }
        fn validate(&mut self, end: Instant) -> io::Result<()> {
            self.phase_deadlines.push(end);
            self.now += Duration::from_secs(1);
            self.step("validate")
        }
        fn export(&mut self, end: Instant) -> io::Result<()> {
            self.phase_deadlines.push(end);
            self.step("export")
        }
        fn build(&mut self, end: Instant) -> io::Result<()> {
            if self.expire_build {
                self.now = end;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "build exhausted active budget",
                ));
            }
            self.step("build")
        }
        fn publish(&mut self, _: Instant) -> io::Result<Manifest> {
            self.step("publish")?;
            self.published = true;
            Ok(manifest())
        }
        fn terminate_workers(&mut self) -> io::Result<()> {
            self.step("terminate")
        }
        fn prove_no_live_build(&mut self, _: bool, end: Instant) -> io::Result<()> {
            assert!(end > self.now, "cleanup receives a fresh bounded budget");
            self.step("proof")?;
            if self.live {
                Err(io::Error::other("live descendant"))
            } else {
                Ok(())
            }
        }
        fn cleanup(&mut self, _: Instant) -> io::Result<()> {
            assert!(!self.live);
            self.step("remove-scratch")
        }
        fn scratch_path(&self) -> String {
            "root/packages/test-1/source".into()
        }
        fn revalidate(&mut self, _: &Manifest, _: Instant) -> io::Result<()> {
            self.step("rehash")
        }
        fn publish_release(&mut self) -> io::Result<Value> {
            assert!(self.finalized);
            self.step("release")?;
            self.released = true;
            Ok(release_proof())
        }
        fn close_released(&mut self) -> io::Result<Option<Value>> {
            assert!(self.released);
            self.step("close")?;
            if self.detach {
                let mut proof = detached_proof();
                if self.wrong_detach {
                    proof.set("lease", Value::text("different-lease")).unwrap();
                }
                Ok(Some(proof))
            } else {
                Ok(None)
            }
        }
        fn cleanup_released(&mut self) -> io::Result<bool> {
            let durable = Progress::parse(self.durable_final.as_ref().unwrap()).unwrap();
            assert_eq!(
                durable
                    .lock_release
                    .unwrap()
                    .get("state")
                    .unwrap()
                    .string()
                    .unwrap(),
                "detached"
            );
            self.step("remove-detached")?;
            Ok(self.remove_detached)
        }
    }
    fn release_proof() -> Value {
        Value::object([
            ("state", Value::text("owner-record")),
            (
                "slot",
                Value::text(std::env::temp_dir().join("qemu").to_string_lossy()),
            ),
            ("directory", Value::text("0123abcd:0123456789abcdef")),
            ("owner", Value::text("0123abcd:0123456789abcdee")),
            ("run", Value::text("test-1")),
            ("lease", Value::text("test-lease-1")),
            ("released", Value::text("2026-10-13T03:00:00.1234567Z")),
        ])
    }
    fn detached_proof() -> Value {
        let mut proof = release_proof();
        proof.set("state", Value::text("detached")).unwrap();
        proof
            .set(
                "detached_path",
                Value::text(
                    std::env::temp_dir()
                        .join("qemu.released.1-2-3")
                        .to_string_lossy(),
                ),
            )
            .unwrap();
        proof
            .set("detached_identity", proof.get("directory").unwrap().clone())
            .unwrap();
        proof
    }
    #[test]
    fn failure_at_each_package_boundary_never_accepts_partial_publication() {
        for fail in [
            "validate",
            "export",
            "build",
            "publish",
            "published",
            "remove-scratch",
        ] {
            let mut b = Fake::new(Some(fail));
            let p = run(&mut b).unwrap();
            assert_eq!(p.verdict, Verdict::Failed, "{fail}");
            assert!(p.first_failure.is_some());
            assert!(b.released);
            assert!(
                b.calls.iter().position(|s| s == "proof").unwrap()
                    < b.calls.iter().position(|s| s == "remove-scratch").unwrap()
            );
        }
        let mut b = Fake::new(None);
        let p = run(&mut b).unwrap();
        assert_eq!(p.verdict, Verdict::FinalizedValid);
        assert_eq!(
            Progress::parse(&p.record()).unwrap().verdict,
            Verdict::FinalizedValid
        );
    }
    #[test]
    fn live_descendant_and_failed_finalization_retain_lease() {
        let mut b = Fake::new(None);
        b.live = true;
        let p = run(&mut b).unwrap();
        assert_eq!(p.verdict, Verdict::Blocked);
        assert!(!b.published, "live descendants must block publication");
        assert!(!b.calls.iter().any(|s| s == "remove-scratch"));
        assert!(!b.released);
        let mut b = Fake::new(Some("final"));
        assert!(run(&mut b).is_err());
        assert!(!b.released);
    }
    #[test]
    fn recovery_never_builds_and_preserves_only_revalidated_durable_validity() {
        let mut b = Fake::new(None);
        let p = recover(&mut b, None).unwrap();
        assert_eq!(p.verdict, Verdict::RecoveredInvalid);
        assert!(b.released);
        assert!(
            !b.calls
                .iter()
                .any(|s| matches!(s.as_str(), "build" | "publish" | "export"))
        );
        let mut valid = Fake::new(None);
        let p = run(&mut valid).unwrap();
        let mut recovered = Fake::new(None);
        assert_eq!(
            recover(&mut recovered, Some(p.clone())).unwrap().verdict,
            Verdict::FinalizedValid
        );
        let mut denied = Fake::new(Some("remove-scratch"));
        let q = recover(&mut denied, Some(p.clone())).unwrap();
        assert_eq!(q.verdict, Verdict::FinalizedValid);
        assert_eq!(q.abandoned.len(), 1);
        let mut corrupt = Fake::new(Some("rehash"));
        assert_eq!(
            recover(&mut corrupt, Some(p)).unwrap().verdict,
            Verdict::RecoveredInvalid
        );
    }
    #[test]
    fn parser_rejects_legacy_capability_and_invalid_source_paths() {
        let m = manifest();
        assert_eq!(Manifest::parse(&m.record()).unwrap(), m);
        let mut old = m.clone();
        old.capabilities.pop();
        assert!(Manifest::parse(&old.record()).is_err());
        let mut forged = m.clone();
        forged.capabilities[0].1 = format!("{}{}", &CAPABILITIES[0].1[..7], "0".repeat(33));
        assert!(Manifest::parse(&forged.record()).is_err());
        for path in ["../x", "C:/x", "/x", "a//b", "a/target/x", ".git/config"] {
            assert!(validate_source_path(path).is_err());
        }
    }
    #[test]
    fn finalized_valid_requires_finalization_but_allows_pre_release_crash() {
        let p = run(&mut Fake::new(None)).unwrap();
        let mut pending_release = p.clone();
        pending_release.lock_release = None;
        assert!(Progress::parse(&pending_release.record()).is_ok());
        for phase in ["building", "published", "recovering", "unknown"] {
            let mut changed = pending_release.clone();
            changed.phase = phase.into();
            assert!(Progress::parse(&changed.record()).is_err(), "{phase}");
        }
        let mut no_intent = pending_release;
        no_intent.release_intent = false;
        assert!(Progress::parse(&no_intent.record()).is_err());
        let mut recovering = Fake::new(None);
        assert!(recover(&mut recovering, Some(no_intent)).is_err());
        assert!(recovering.calls.is_empty());
    }
    #[test]
    fn owner_release_proof_is_strict_and_bound_to_package() {
        let p = run(&mut Fake::new(None)).unwrap();
        for (key, value) in [
            ("state", Value::text("removed")),
            ("slot", Value::text("relative/qemu")),
            ("directory", Value::text("0123abcd:short")),
            ("owner", Value::text("0123abcd:0123456789abcdeg")),
            ("run", Value::text("different-package")),
            ("lease", Value::text("../lease")),
            ("released", Value::text("2026-02-30T03:00:00Z")),
            ("unexpected", Value::Bool(true)),
        ] {
            let mut changed = p.clone();
            changed
                .lock_release
                .as_mut()
                .unwrap()
                .set(key, value)
                .unwrap();
            assert!(Progress::parse(&changed.record()).is_err(), "{key}");
        }
        let mut scalar = p.clone();
        scalar.lock_release = Some(Value::text("owner-record"));
        assert!(Progress::parse(&scalar.record()).is_err());
        let mut unfinished = p;
        unfinished.phase = "cleanup".into();
        unfinished.verdict = Verdict::Incomplete;
        unfinished.release_intent = false;
        assert!(Progress::parse(&unfinished.record()).is_err());
    }
    #[test]
    fn active_expiry_preserves_cause_and_still_cleans_up() {
        let mut b = Fake::new(None);
        b.expire_build = true;
        let p = run(&mut b).unwrap();
        assert_eq!(
            b.phase_deadlines[0], b.phase_deadlines[1],
            "validation/export share their total cap"
        );
        assert_eq!(p.verdict, Verdict::Failed);
        assert_eq!(
            p.first_failure.as_deref(),
            Some("build exhausted active budget")
        );
        assert!(!b.published);
        assert!(b.released);
        assert!(b.calls.iter().any(|s| s == "remove-scratch"));
        let mut recovered = Fake::new(None);
        let q = recover(&mut recovered, Some(p)).unwrap();
        assert_eq!(
            q.first_failure.as_deref(),
            Some("build exhausted active budget")
        );
        assert_eq!(q.verdict, Verdict::RecoveredInvalid);
    }
    #[test]
    fn detached_proof_keeps_exact_generation_and_owner_record_fallback() {
        let mut successful = Fake::new(None);
        successful.detach = true;
        let p = run(&mut successful).unwrap();
        assert_eq!(
            p.lock_release
                .as_ref()
                .unwrap()
                .get("state")
                .unwrap()
                .string()
                .unwrap(),
            "detached"
        );
        for field in ["detached_path", "detached_identity"] {
            let mut changed = p.clone();
            changed
                .lock_release
                .as_mut()
                .unwrap()
                .set(field, Value::text("different"))
                .unwrap();
            assert!(Progress::parse(&changed.record()).is_err());
        }
        let mut denied = Fake::new(None);
        denied.detach = true;
        denied.fail_detached_final = true;
        assert!(run(&mut denied).is_err());
        assert!(!denied.calls.iter().any(|s| s == "remove-detached"));
        let retained = Progress::parse(denied.durable_final.as_ref().unwrap()).unwrap();
        assert_eq!(retained.verdict, Verdict::FinalizedValid);
        assert_eq!(
            retained
                .lock_release
                .unwrap()
                .get("state")
                .unwrap()
                .string()
                .unwrap(),
            "owner-record"
        );
        let mut wrong = Fake::new(None);
        wrong.detach = true;
        wrong.wrong_detach = true;
        assert!(run(&mut wrong).is_err());
        assert_eq!(
            Progress::parse(wrong.durable_final.as_ref().unwrap())
                .unwrap()
                .verdict,
            Verdict::FinalizedValid
        );
    }
    #[test]
    fn detached_cleanup_requires_durable_proof_and_denial_preserves_validity() {
        for fail in [None, Some("remove-detached")] {
            let mut fake = Fake::new(fail);
            fake.detach = true;
            fake.remove_detached = true;
            let result = run(&mut fake).unwrap();
            assert_eq!(result.verdict, Verdict::FinalizedValid);
            assert!(result.first_failure.is_none());
            let proof = result.lock_release.as_ref().unwrap();
            assert_eq!(
                proof.get("state").unwrap().string().unwrap(),
                if fail.is_none() {
                    "removed"
                } else {
                    "detached"
                }
            );
            assert_eq!(
                Progress::parse(fake.durable_final.as_ref().unwrap())
                    .unwrap()
                    .record(),
                result.record()
            );
            let mut forged = result.clone();
            forged
                .lock_release
                .as_mut()
                .unwrap()
                .set("detached_identity", Value::text("wrong"))
                .unwrap();
            assert!(Progress::parse(&forged.record()).is_err());
        }
    }
}
