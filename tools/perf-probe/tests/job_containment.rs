#![cfg(windows)]

use perf_probe::windows_job::{
    CapturePaths, CommandSpec, Job, ProcessIdentity, ProcessState, STREAM_LIMIT,
    current_process_identity, process_state, prove_recovery,
};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Controller(std::process::Child);
impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    root: PathBuf,
    name: String,
}
impl Fixture {
    fn new() -> Self {
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            current_process_identity().unwrap().start_time,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(format!("mirror-lane-job-test-{id}"));
        fs::create_dir(&root).unwrap();
        Self {
            root,
            name: format!("Local\\MirrorLane-{id}"),
        }
    }
    fn spec(&self, args: Vec<OsString>) -> CommandSpec {
        CommandSpec {
            program: PathBuf::from(env!("CARGO_BIN_EXE_job_fixture")),
            args,
            cwd: self.root.clone(),
            env: vec![("SystemRoot".into(), std::env::var_os("SystemRoot").unwrap())],
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this fresh synthetic fixture path is removed; shared lane/lock
        // paths are never touched by these host tests.
        let expected = std::env::temp_dir().join(self.root.file_name().unwrap());
        assert_eq!(self.root, expected);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "synthetic process deadline");
        thread::sleep(Duration::from_millis(10));
    }
}

fn identity(path: &Path) -> ProcessIdentity {
    let value = fs::read_to_string(path).unwrap();
    let mut fields = value.split_whitespace();
    ProcessIdentity {
        pid: fields.next().unwrap().parse().unwrap(),
        start_time: fields.next().unwrap().parse().unwrap(),
    }
}

#[test]
fn controller_death_kills_worker_and_descendant_without_inherited_job_handle() {
    let fixture = Fixture::new();
    let mut controller = Controller(
        Command::new(env!("CARGO_BIN_EXE_job_fixture"))
            .arg("controller")
            .arg(&fixture.root)
            .arg(&fixture.name)
            .spawn()
            .unwrap(),
    );
    wait_for(|| fixture.root.join("leaf.identity").exists());
    let controller_id = identity(&fixture.root.join("controller.identity"));
    let worker_id = identity(&fixture.root.join("worker.identity"));
    let leaf_id = identity(&fixture.root.join("leaf.identity"));
    assert!(prove_recovery(controller_id, &[worker_id], &fixture.name).is_err());
    controller.0.kill().unwrap();
    controller.0.wait().unwrap();
    wait_for(|| {
        process_state(worker_id).unwrap() == ProcessState::Exited
            && process_state(leaf_id).unwrap() == ProcessState::Exited
    });
    prove_recovery(controller_id, &[worker_id, leaf_id], &fixture.name).unwrap();
}

#[test]
fn controller_death_during_durable_callback_never_executes_suspended_worker() {
    let fixture = Fixture::new();
    let mut controller = Controller(
        Command::new(env!("CARGO_BIN_EXE_job_fixture"))
            .arg("controller-before-resume")
            .arg(&fixture.root)
            .arg(&fixture.name)
            .spawn()
            .unwrap(),
    );
    wait_for(|| fixture.root.join("before-resume").exists());
    let worker_id = identity(&fixture.root.join("worker.identity"));
    assert!(!fixture.root.join("worker.executed").exists());
    controller.0.kill().unwrap();
    controller.0.wait().unwrap();
    wait_for(|| process_state(worker_id).unwrap() == ProcessState::Exited);
    assert!(!fixture.root.join("worker.executed").exists());
}

#[test]
fn journal_callback_is_durable_before_execution_and_failure_never_resumes() {
    let fixture = Fixture::new();
    let job = Job::new(&fixture.name).unwrap();
    let marker = fixture.root.join("executed");
    let spec = fixture.spec(vec!["touch".into(), marker.as_os_str().to_owned()]);
    let mut recorded = None;
    assert!(
        job.spawn(&spec, |id| {
            recorded = Some(id);
            assert!(!marker.exists());
            let mut journal = fs::File::create(fixture.root.join("journal")).unwrap();
            write!(journal, "{} {}", id.pid, id.start_time).unwrap();
            journal.sync_all().unwrap();
            Err(io::Error::other("injected durable journal failure"))
        })
        .is_err()
    );
    job.wait_empty(Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        process_state(recorded.unwrap()).unwrap(),
        ProcessState::Exited
    );
    assert!(!marker.exists());
    let worker = job
        .spawn(&spec, |_| {
            assert!(!marker.exists());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        job.wait(worker, Instant::now() + Duration::from_secs(10))
            .unwrap()
            .exit_code,
        0
    );
    assert!(marker.exists());
}

#[test]
fn command_quotes_empty_spaces_quotes_and_trailing_backslashes_round_trip() {
    let fixture = Fixture::new();
    let job = Job::new(&fixture.name).unwrap();
    let arguments = [
        "",
        "space here",
        "a\"quote",
        "slash\\",
        "two\\\\\"quotes",
        "Unicode λ",
    ];
    let mut args = vec!["echo".into()];
    args.extend(arguments.iter().map(OsString::from));
    let worker = job.spawn(&fixture.spec(args), |_| Ok(())).unwrap();
    let result = job
        .wait(worker, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let expected: Vec<u8> = arguments
        .iter()
        .flat_map(|arg| {
            (arg.len() as u32)
                .to_le_bytes()
                .into_iter()
                .chain(arg.bytes())
        })
        .collect();
    assert_eq!(result.stdout, expected);
    assert_eq!(result.stderr, b"fixture stderr");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn explicit_environment_clears_parent_and_duplicate_names_fail_closed() {
    let fixture = Fixture::new();
    let job = Job::new(&fixture.name).unwrap();
    let mut spec = fixture.spec(vec!["env".into()]);
    spec.env
        .push(("MIRROR_FIXTURE_ONLY".into(), "present".into()));
    let worker = job.spawn(&spec, |_| Ok(())).unwrap();
    let output = job
        .wait(worker, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert_eq!(output.stdout, b"Some(\"present\")|None");
    spec.env
        .push(("mirror_fixture_only".into(), "duplicate".into()));
    assert!(job.spawn(&spec, |_| Ok(())).is_err());
    assert_eq!(job.active_processes().unwrap(), 0);
}

#[test]
fn bounded_capture_overflow_fails_and_preserves_exact_prefix() {
    let fixture = Fixture::new();
    let job = Job::new(&fixture.name).unwrap();
    let paths = CapturePaths {
        stdout: fixture.root.join("out"),
        stderr: fixture.root.join("err"),
    };
    let worker = job
        .spawn_captured(&fixture.spec(vec!["overflow".into()]), &paths, |_| Ok(()))
        .unwrap();
    assert!(
        job.wait(worker, Instant::now() + Duration::from_secs(10))
            .unwrap_err()
            .to_string()
            .contains("output overflow")
    );
    job.wait_empty(Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert_eq!(fs::read(paths.stdout).unwrap(), vec![b'x'; STREAM_LIMIT]);
}

#[test]
fn nonzero_exit_is_preserved_and_expired_deadline_terminates_tree() {
    let fixture = Fixture::new();
    let job = Job::new(&fixture.name).unwrap();
    let worker = job
        .spawn(&fixture.spec(vec!["exit7".into()]), |_| Ok(()))
        .unwrap();
    assert_eq!(
        job.wait(worker, Instant::now() + Duration::from_secs(10))
            .unwrap()
            .exit_code,
        7
    );
    let worker = job
        .spawn(
            &fixture.spec(vec!["tree".into(), fixture.root.as_os_str().to_owned()]),
            |_| Ok(()),
        )
        .unwrap();
    wait_for(|| fixture.root.join("leaf.identity").exists());
    let leaf = identity(&fixture.root.join("leaf.identity"));
    assert_eq!(
        job.wait(worker, Instant::now()).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    job.wait_empty(Instant::now() + Duration::from_secs(10))
        .unwrap();
    // Windows may remove a terminating process from job accounting just before
    // its process object becomes signaled. Observe that final identity too.
    wait_for(|| process_state(leaf).unwrap() == ProcessState::Exited);
}

#[test]
fn reused_pid_proves_original_exit_but_recovery_still_requires_job_and_live_checks() {
    let fixture = Fixture::new();
    let job = Job::new(&fixture.name).unwrap();
    assert!(Job::new(&fixture.name).is_err());
    assert_eq!(job.active_processes().unwrap(), 0);
    let current = current_process_identity().unwrap();
    let mut reused = current;
    reused.start_time += 1;
    // A real live PID with a different creation time is no longer the recorded
    // controller/worker. Recovery must not reject this unrelated incarnation.
    assert_eq!(process_state(current).unwrap(), ProcessState::Running);
    assert_eq!(process_state(reused).unwrap(), ProcessState::Exited);
    prove_recovery(reused, &[reused], &fixture.name).unwrap();
    assert_eq!(
        prove_recovery(current, &[], &fixture.name)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(
        prove_recovery(reused, &[current], &fixture.name)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    let worker = job
        .spawn(
            &fixture.spec(vec!["tree".into(), fixture.root.as_os_str().to_owned()]),
            |_| Ok(()),
        )
        .unwrap();
    wait_for(|| fixture.root.join("leaf.identity").exists());
    assert!(
        prove_recovery(reused, &[reused], &fixture.name)
            .unwrap_err()
            .to_string()
            .contains("old job has live descendants")
    );
    job.terminate().unwrap();
    job.wait(worker, Instant::now() + Duration::from_secs(10))
        .unwrap();
    job.wait_empty(Instant::now() + Duration::from_secs(10))
        .unwrap();
    prove_recovery(reused, &[reused], &fixture.name).unwrap();
    drop(job);
    prove_recovery(reused, &[reused], &fixture.name).unwrap();
}
