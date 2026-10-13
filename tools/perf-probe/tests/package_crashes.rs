//! Abrupt synthetic controller death followed by a new controller loading only
//! durable LaneRoot records. These exercise the production package state machine;
//! Windows job containment and lease identities have separate native fixtures.
use perf_probe::package::{
    self, Artifact, Backend, CAPABILITIES, Manifest, Progress, SourceFile, Verdict,
};
use perf_probe::{
    paths,
    record::{self, Value},
    sha256,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CRASH_EXIT: i32 = 73;
const IMAGE: &[u8] = b"synthetic image, never a bootable disk";
const DESCRIPTOR: &[u8] = b"synthetic descriptor";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(format!("mirror-package-crashes-{id}"));
        fs::create_dir(&root).unwrap();
        let root = paths::canonical_no_reparse(&root).unwrap();
        for relative in [
            "lane/packages/fixture",
            "old-checkout/build/dist",
            "old-checkout/build/logs",
            "new-checkout",
        ] {
            fs::create_dir_all(root.join(relative)).unwrap();
        }
        record::atomic_write(
            &root.join("old-checkout/Cargo.toml"),
            b"original checkout must not change",
        )
        .unwrap();
        record::atomic_write(
            &root.join("old-checkout/build/logs/bulk.txt"),
            b"disposable bulk evidence",
        )
        .unwrap();
        record::write(
            &root.join("lane/packages/fixture/binding.json"),
            &Value::object([
                ("schema", Value::Number(1)),
                ("run", Value::text("fixture")),
                ("root", Value::text(root.to_str().unwrap())),
                (
                    "original_checkout",
                    Value::text(root.join("old-checkout").to_str().unwrap()),
                ),
                (
                    "scratch",
                    Value::text(root.join("lane/packages/fixture/source").to_str().unwrap()),
                ),
                (
                    "output",
                    Value::text(root.join("old-checkout/build/dist").to_str().unwrap()),
                ),
            ]),
        )
        .unwrap();
        Self(root)
    }
    fn crash(&self, boundary: &str, fail_build: bool) {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "synthetic_controller_process",
                "--ignored",
                "--nocapture",
            ])
            .env("MIRROR_PACKAGE_CRASH_ROOT", &self.0)
            .env("MIRROR_PACKAGE_CRASH_BOUNDARY", boundary)
            .env(
                "MIRROR_PACKAGE_CRASH_BUILD_FAIL",
                if fail_build { "1" } else { "0" },
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut guard = ChildGuard(child);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = guard.0.try_wait().unwrap() {
                assert_eq!(
                    status.code(),
                    Some(CRASH_EXIT),
                    "boundary {boundary} was not reached"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "synthetic controller did not exit"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn backend(&self) -> DurableFake {
        DurableFake::open(&self.0, true).unwrap()
    }
    fn remove_old_checkout(&self) {
        // This is an invented checkout entirely within this fresh fixture root.
        let checkout = paths::canonical_no_reparse(&self.0.join("old-checkout")).unwrap();
        assert_eq!(checkout.parent(), Some(self.0.as_path()));
        fs::remove_dir_all(checkout).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Never touch a real checkout, lane or machine-lock acquisition path.
        let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
        assert_eq!(self.0.parent(), Some(temp.as_path()));
        assert!(
            self.0
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("mirror-package-crashes-")
        );
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DurableFake {
    root: PathBuf,
    run_dir: PathBuf,
    scratch: PathBuf,
    output: PathBuf,
    recovery: bool,
    lease: u64,
    boundary: String,
    build_failure: bool,
    deny_cleanup: bool,
    proof_error: Option<&'static str>,
    proof_observed: bool,
    calls: Vec<String>,
}
impl DurableFake {
    fn open(root: &Path, recovery: bool) -> io::Result<Self> {
        let root = paths::canonical_no_reparse(root)?;
        let run_dir = root.join("lane/packages/fixture");
        let binding = record::read(&run_dir.join("binding.json"))?;
        binding.fields(&[
            "schema",
            "run",
            "root",
            "original_checkout",
            "scratch",
            "output",
        ])?;
        assert_eq!(binding.get("root")?.string()?, root.to_str().unwrap());
        let scratch = PathBuf::from(binding.get("scratch")?.string()?);
        let output = PathBuf::from(binding.get("output")?.string()?);
        assert_eq!(scratch, run_dir.join("source"));
        assert_eq!(output, root.join("old-checkout/build/dist"));
        // A synthetic lease counter models a NEW exclusive lease after restart.
        // Native lease authentication/races are covered by their own tests.
        let lease = match record::read(&run_dir.join("owner.json")) {
            Ok(old) => old.get("lease")?.number()? + 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 1,
            Err(e) => return Err(e),
        };
        record::write(
            &run_dir.join("owner.json"),
            &Value::object([
                ("lease", Value::Number(lease)),
                ("released", Value::Bool(false)),
                ("proof", Value::Null),
            ]),
        )?;
        Ok(Self {
            root,
            run_dir,
            scratch,
            output,
            recovery,
            lease,
            boundary: String::new(),
            build_failure: false,
            deny_cleanup: false,
            proof_error: None,
            proof_observed: false,
            calls: vec![],
        })
    }
    fn event(&mut self, name: &str) -> io::Result<()> {
        self.calls.push(name.into());
        record::write(
            &self.run_dir.join(format!("calls-{}.json", self.lease)),
            &Value::Array(self.calls.iter().map(Value::text).collect()),
        )
    }
    fn crash(&self, boundary: &str) {
        if self.boundary == boundary {
            // Deliberately bypass Drop/finally/unwinding, as controller death does.
            std::process::exit(CRASH_EXIT);
        }
    }
    fn previous(&self) -> io::Result<Progress> {
        package::load_prior(
            &self.run_dir,
            &record::read(&self.run_dir.join("journal.json"))?,
        )
    }
    fn recover(&mut self) -> Progress {
        let previous = self.previous().unwrap();
        package::recover(self, Some(previous)).unwrap()
    }
    fn assert_no_rebuild(&self) {
        assert!(
            !self
                .calls
                .iter()
                .any(|s| matches!(s.as_str(), "validate" | "export" | "build" | "publish"))
        );
    }
    fn assert_released(&self, p: &Progress) {
        let owner = record::read(&self.run_dir.join("owner.json")).unwrap();
        assert!(owner.get("released").unwrap().boolean().unwrap());
        assert_eq!(owner.get("lease").unwrap().number().unwrap(), self.lease);
        assert_eq!(
            p.lock_release
                .as_ref()
                .unwrap()
                .get("lease")
                .unwrap()
                .string()
                .unwrap(),
            format!("lease-{}", self.lease)
        );
        for name in ["journal.json", "final.json"] {
            let durable =
                Progress::parse(&record::read(&self.run_dir.join(name)).unwrap()).unwrap();
            assert_eq!(durable.verdict, p.verdict);
            assert_eq!(durable.first_failure, p.first_failure);
            assert_eq!(durable.lock_release, p.lock_release);
        }
        assert_eq!(self.calls.last().unwrap(), "close");
    }
}

fn artifact(bytes: &[u8]) -> Artifact {
    Artifact {
        size: bytes.len() as u64,
        sha256: sha256::bytes(bytes),
    }
}
fn manifest() -> Manifest {
    Manifest {
        package_id: "fixture".into(),
        commit: "1".repeat(40),
        tree: "2".repeat(40),
        release: false,
        toolchain: "rustc 1.99.0 (synthetic fixture)".into(),
        capabilities: CAPABILITIES
            .into_iter()
            .map(|(n, c)| (n.into(), c.into()))
            .collect(),
        inventory: vec![SourceFile {
            path: "Cargo.toml".into(),
            artifact: artifact(b"synthetic source"),
        }],
        image: artifact(IMAGE),
        descriptor: artifact(DESCRIPTOR),
    }
}
impl Backend for DurableFake {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn persist(&mut self, p: &Progress, final_record: bool) -> io::Result<()> {
        self.event(if final_record { "final" } else { &p.phase })?;
        if self.recovery && p.phase == "recovering" {
            assert!(p.lock_release.is_none() && !p.release_intent && !p.no_live_build);
        }
        record::write(&self.run_dir.join("journal.json"), &p.record())?;
        if final_record {
            assert!(self.proof_observed && p.no_live_build && p.release_intent);
            self.crash("final-before");
            record::write(&self.run_dir.join("final.json"), &p.record())?;
            self.crash("final-after");
        }
        if p.phase == "stopping-workers" {
            self.crash("failure-recorded");
        }
        Ok(())
    }
    fn validate(&mut self, _: Instant) -> io::Result<()> {
        self.event("validate")
    }
    fn export(&mut self, _: Instant) -> io::Result<()> {
        self.event("export")?;
        fs::create_dir(&self.scratch)?;
        record::atomic_write(&self.scratch.join("partial-export"), b"partial source")?;
        self.crash("export");
        record::atomic_write(&self.scratch.join("Cargo.toml"), b"synthetic source")
    }
    fn build(&mut self, _: Instant) -> io::Result<()> {
        self.event("build")?;
        record::atomic_write(&self.scratch.join("mirror.img"), IMAGE)?;
        self.crash("build");
        if self.build_failure {
            return Err(io::Error::other("synthetic compiler failed first"));
        }
        record::atomic_write(&self.scratch.join("mirror.vmdk"), DESCRIPTOR)
    }
    fn publish(&mut self, _: Instant) -> io::Result<Manifest> {
        assert!(self.proof_observed);
        self.event("publish")?;
        record::atomic_write(
            &self.output.join("mirror.img"),
            &fs::read(self.scratch.join("mirror.img"))?,
        )?;
        self.crash("copy");
        record::atomic_write(
            &self.output.join("mirror.vmdk"),
            &fs::read(self.scratch.join("mirror.vmdk"))?,
        )?;
        let m = manifest();
        record::write(&self.output.join("mirror.package.json"), &m.record())?;
        self.crash("publication");
        Ok(m)
    }
    fn terminate_workers(&mut self) -> io::Result<()> {
        self.event("terminate")
    }
    fn prove_no_live_build(&mut self, recovery: bool, _: Instant) -> io::Result<()> {
        assert_eq!(recovery, self.recovery);
        self.event("proof")?;
        if let Some(error) = self.proof_error {
            return Err(io::Error::other(error));
        }
        self.proof_observed = true;
        Ok(())
    }
    fn cleanup(&mut self, _: Instant) -> io::Result<()> {
        assert!(self.proof_observed);
        self.event("cleanup-scratch")?;
        if self.deny_cleanup {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "synthetic scratch deletion denied",
            ));
        }
        if self.scratch.exists() {
            let verified = paths::canonical_no_reparse(&self.scratch)?;
            assert_eq!(verified, self.run_dir.join("source"));
            assert!(verified.starts_with(&self.root));
            fs::remove_dir_all(verified)?;
        }
        Ok(())
    }
    fn scratch_path(&self) -> String {
        self.scratch.to_str().unwrap().into()
    }
    fn revalidate(&mut self, m: &Manifest, _: Instant) -> io::Result<()> {
        self.event("rehash")?;
        if Manifest::parse(&record::read(&self.output.join("mirror.package.json"))?)? != *m {
            return Err(record::invalid("retained manifest mismatch"));
        }
        for (name, expected) in [("mirror.img", &m.image), ("mirror.vmdk", &m.descriptor)] {
            let (size, hash) = sha256::file(&self.output.join(name))?;
            if size != expected.size || hash != expected.sha256 {
                return Err(record::invalid("published hash mismatch"));
            }
        }
        Ok(())
    }
    fn publish_release(&mut self) -> io::Result<Value> {
        self.event("release")?;
        let final_record = Progress::parse(&record::read(&self.run_dir.join("final.json"))?)?;
        assert!(final_record.no_live_build && final_record.release_intent);
        self.crash("release-before");
        let proof = Value::object([
            ("state", Value::text("owner-record")),
            (
                "slot",
                Value::text(self.root.join("qemu").to_str().unwrap()),
            ),
            (
                "directory",
                Value::text(format!("0123abcd:{:016x}", self.lease)),
            ),
            (
                "owner",
                Value::text(format!("0123abcd:{:016x}", self.lease + 100)),
            ),
            ("run", Value::text("fixture")),
            ("lease", Value::text(format!("lease-{}", self.lease))),
            ("released", Value::text("2026-10-13T03:00:00Z")),
        ]);
        let released = Value::object([
            ("lease", Value::Number(self.lease)),
            ("released", Value::Bool(true)),
            ("proof", proof.clone()),
        ]);
        record::write(&self.run_dir.join("owner.json"), &released)?;
        self.crash("release-after");
        Ok(proof)
    }
    fn close_released(&mut self) -> io::Result<Option<Value>> {
        let p = Progress::parse(&record::read(&self.run_dir.join("final.json"))?)?;
        assert_eq!(
            p.lock_release,
            Some(
                record::read(&self.run_dir.join("owner.json"))?
                    .get("proof")?
                    .clone()
            )
        );
        self.event("close")?;
        Ok(None)
    }
}

#[test]
#[ignore = "subprocess entry; parent fixtures inject abrupt controller death"]
fn synthetic_controller_process() {
    let root = PathBuf::from(std::env::var_os("MIRROR_PACKAGE_CRASH_ROOT").unwrap());
    let mut backend = DurableFake::open(&root, false).unwrap();
    backend.boundary = std::env::var("MIRROR_PACKAGE_CRASH_BOUNDARY").unwrap();
    backend.build_failure = std::env::var("MIRROR_PACKAGE_CRASH_BUILD_FAIL").unwrap() == "1";
    let _ = package::run(&mut backend).unwrap();
    panic!("requested abrupt stop was not reached");
}

#[test]
fn death_during_export_build_copy_or_publication_recovers_invalid_without_repair() {
    for boundary in ["export", "build", "copy", "publication", "final-before"] {
        let f = Fixture::new();
        f.crash(boundary, false);
        let published_before = fs::read(f.0.join("old-checkout/build/dist/mirror.img")).ok();
        let mut backend = f.backend();
        let p = backend.recover();
        assert_eq!(p.verdict, Verdict::RecoveredInvalid, "{boundary}");
        assert!(
            p.first_failure
                .as_ref()
                .unwrap()
                .contains("interrupted package")
        );
        assert!(!backend.scratch.exists());
        assert_eq!(
            fs::read(backend.output.join("mirror.img")).ok(),
            published_before
        );
        assert_eq!(
            fs::read(f.0.join("old-checkout/Cargo.toml")).unwrap(),
            b"original checkout must not change"
        );
        backend.assert_no_rebuild();
        backend.assert_released(&p);
    }
}

#[test]
fn durable_final_survives_crashes_before_and_after_owner_release_publication() {
    for boundary in ["final-after", "release-before", "release-after"] {
        let f = Fixture::new();
        f.crash(boundary, false);
        let old_owner = record::read(&f.0.join("lane/packages/fixture/owner.json")).unwrap();
        assert_eq!(
            old_owner.get("released").unwrap().boolean().unwrap(),
            boundary == "release-after"
        );
        let mut backend = f.backend();
        let p = backend.recover();
        assert_eq!(p.verdict, Verdict::FinalizedValid, "{boundary}");
        assert!(p.first_failure.is_none());
        assert!(backend.calls.iter().any(|c| c == "rehash"));
        assert_eq!(
            backend.lease,
            old_owner.get("lease").unwrap().number().unwrap() + 1
        );
        backend.assert_no_rebuild();
        backend.assert_released(&p);
    }
}

#[test]
fn first_failure_survives_restart_and_deleted_checkout_and_bulk_evidence() {
    let f = Fixture::new();
    f.crash("failure-recorded", true);
    f.remove_old_checkout();
    let mut backend = f.backend();
    let p = backend.recover();
    assert_eq!(p.verdict, Verdict::RecoveredInvalid);
    assert_eq!(
        p.first_failure.as_deref(),
        Some("synthetic compiler failed first")
    );
    assert!(
        !f.0.join("old-checkout").exists(),
        "recovery must not recreate the old checkout"
    );
    assert!(!backend.scratch.exists());
    backend.assert_no_rebuild();
    backend.assert_released(&p);
}

#[test]
fn denied_scratch_is_invalid_on_normal_run_and_retained_with_exact_path_on_recovery() {
    let f = Fixture::new();
    let mut normal = DurableFake::open(&f.0, false).unwrap();
    normal.deny_cleanup = true;
    let p = package::run(&mut normal).unwrap();
    assert_eq!(p.verdict, Verdict::Failed);
    assert_eq!(p.abandoned, vec![normal.scratch_path()]);
    assert!(normal.scratch.exists());
    normal.assert_released(&p);
    let first_failure = p.first_failure.clone();
    drop(normal);
    let mut recovered = f.backend();
    recovered.deny_cleanup = true;
    let p = recovered.recover();
    assert_eq!(p.verdict, Verdict::RecoveredInvalid);
    assert_eq!(p.first_failure, first_failure);
    assert!(p.abandoned.iter().all(|p| p == &recovered.scratch_path()));
    assert!(!p.abandoned.is_empty() && recovered.scratch.exists());
    recovered.assert_no_rebuild();
    recovered.assert_released(&p);
}

#[test]
fn restart_refuses_cleanup_and_release_until_identity_and_empty_job_proof() {
    for error in [
        "old controller still alive",
        "PID creation time changed",
        "job has a live descendant",
    ] {
        let f = Fixture::new();
        f.crash("build", false);
        let mut backend = f.backend();
        backend.proof_error = Some(error);
        let p = backend.recover();
        assert_eq!(p.verdict, Verdict::Blocked);
        assert_eq!(p.first_failure.as_deref(), Some(error));
        assert!(backend.scratch.exists());
        assert!(
            !backend
                .calls
                .iter()
                .any(|c| matches!(c.as_str(), "cleanup-scratch" | "release" | "close"))
        );
        assert!(
            !record::read(&backend.run_dir.join("owner.json"))
                .unwrap()
                .get("released")
                .unwrap()
                .boolean()
                .unwrap()
        );
        backend.assert_no_rebuild();
    }
}

#[test]
fn valid_final_with_mutated_hash_or_missing_old_checkout_becomes_invalid_without_repair() {
    for remove_checkout in [false, true] {
        let f = Fixture::new();
        f.crash("final-after", false);
        if remove_checkout {
            f.remove_old_checkout();
        } else {
            record::atomic_write(&f.0.join("old-checkout/build/dist/mirror.vmdk"), b"changed")
                .unwrap();
        }
        let mut backend = f.backend();
        let p = backend.recover();
        assert_eq!(p.verdict, Verdict::RecoveredInvalid);
        assert!(p.first_failure.is_some());
        if remove_checkout {
            assert!(!f.0.join("old-checkout").exists());
        } else {
            assert_eq!(
                fs::read(backend.output.join("mirror.vmdk")).unwrap(),
                b"changed"
            );
        }
        backend.assert_no_rebuild();
        backend.assert_released(&p);
    }
}
