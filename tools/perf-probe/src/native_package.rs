//! Windows packaging adapter. All external workers run in the controller's job.
use crate::{
    lease::{self, DetachEvent, DetachPhase, Detached, Lease, Owner, ReleaseRecord, Replacement},
    package::{self, Artifact, Backend, Manifest, Progress, SourceFile},
    paths,
    record::{self, Value},
    sha256,
    windows_job::{self, CapturePaths, CommandSpec, Job, ProcessIdentity},
};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const LOCK_ROOT: &str = "C:/Users/engin/Desktop/Mirror-locks";
const LIMINE_VERSION: &str = "v12.9.3";
const LIMINE_SHA: &str = "1bdbbcda650c5dba9b1eca5833d598137bf11bd6289bc89b6bcc4f8f112fa676";
const TOTAL_STREAMS: usize = 16 * 1024 * 1024;
const ENV_KEYS: &[&str] = &[
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATH",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "TEMP",
    "TMP",
    "SYSTEMDRIVE",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "INCLUDE",
    "LIB",
    "LIBPATH",
    "RUSTUP_HOME",
];

pub struct Options {
    pub checkout: PathBuf,
    pub lane_root: PathBuf,
    pub operator: String,
    pub release: bool,
    pub recover: Option<String>,
    pub slot: Option<String>,
    pub cargo_home: PathBuf,
    pub limine_archive: PathBuf,
}
pub struct NativePackage {
    options: Options,
    run: String,
    run_dir: PathBuf,
    scratch: PathBuf,
    output: PathBuf,
    job: Option<Job>,
    lease: Option<Lease>,
    journal: Value,
    environment: Vec<(OsString, OsString)>,
    commit: String,
    tree: String,
    toolchain: String,
    capabilities: Vec<(String, String)>,
    source_paths: Vec<String>,
    inventory: Vec<SourceFile>,
    total_streams: usize,
    command_number: usize,
    prior: Option<Progress>,
    recovery: bool,
    released_cleanup: Option<(Detached, ReleaseRecord, Owner)>,
}

pub fn utc_now() -> String {
    #[repr(C)]
    struct SystemTimeParts {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millisecond: u16,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemTime(out: *mut SystemTimeParts);
    }
    let mut t = SystemTimeParts {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        millisecond: 0,
    };
    // SAFETY: t has the documented SYSTEMTIME layout and is writable for the call.
    unsafe { GetSystemTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}
fn unique(prefix: &str) -> io::Result<String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    Ok(format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}
fn text(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| record::invalid("non-Unicode path cannot be journaled"))
}
/// Render already-validated local-drive identities for external programs that
/// do not understand Win32 verbatim prefixes. Internal paths stay canonical.
fn tool_path(path: &Path) -> io::Result<PathBuf> {
    use std::path::{Component, Prefix};
    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => {
                return Err(record::invalid(
                    "external tool path must name a local drive",
                ));
            }
        },
        _ => return Err(record::invalid("external tool path must be absolute")),
    };
    if components.next() != Some(Component::RootDir) {
        return Err(record::invalid("drive-relative tool path is forbidden"));
    }
    let mut result = PathBuf::from(format!("{}:\\", char::from(drive)));
    for component in components {
        match component {
            Component::Normal(name) => result.push(name),
            _ => return Err(record::invalid("ambiguous external tool path")),
        }
    }
    Ok(result)
}
fn tool_arg(path: &Path) -> io::Result<OsString> {
    Ok(tool_path(path)?.into_os_string())
}
fn tool_environment(environment: &[(OsString, OsString)]) -> io::Result<Vec<(OsString, OsString)>> {
    use std::path::{Component, Prefix};
    fn normal(value: &std::ffi::OsStr) -> io::Result<OsString> {
        let path = Path::new(value);
        if matches!(path.components().next(),Some(Component::Prefix(prefix)) if matches!(prefix.kind(),Prefix::VerbatimDisk(_)))
        {
            tool_arg(path)
        } else {
            Ok(value.to_owned())
        }
    }
    environment
        .iter()
        .map(|(key, value)| {
            let value = match key.to_string_lossy().to_ascii_uppercase().as_str() {
                "PATH" | "INCLUDE" | "LIB" | "LIBPATH" => {
                    let paths = std::env::split_paths(value)
                        .map(|p| normal(p.as_os_str()).map(PathBuf::from))
                        .collect::<io::Result<Vec<_>>>()?;
                    std::env::join_paths(paths)
                        .map_err(|e| record::invalid(format!("invalid path environment: {e}")))?
                }
                "CARGO_HOME" | "RUSTUP_HOME" | "SYSTEMROOT" | "WINDIR" | "COMSPEC"
                | "USERPROFILE" | "APPDATA" | "LOCALAPPDATA" | "PROGRAMFILES"
                | "PROGRAMFILES(X86)" | "PROGRAMW6432" | "TEMP" | "TMP" => normal(value)?,
                _ => value.clone(),
            };
            Ok((key.clone(), value))
        })
        .collect()
}
fn identity(p: ProcessIdentity) -> Value {
    Value::object([
        ("pid", Value::Number(u64::from(p.pid))),
        ("start_time", Value::Number(p.start_time)),
    ])
}
fn parse_identity(v: &Value) -> io::Result<ProcessIdentity> {
    v.fields(&["pid", "start_time"])?;
    let id = ProcessIdentity {
        pid: u32::try_from(v.get("pid")?.number()?).map_err(|_| record::invalid("invalid PID"))?,
        start_time: v.get("start_time")?.number()?,
    };
    if id.pid == 0 || id.start_time == 0 {
        return Err(record::invalid("incomplete process identity"));
    }
    Ok(id)
}
fn check_time(end: Instant) -> io::Result<()> {
    if Instant::now() >= end {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "package phase deadline",
        ))
    } else {
        Ok(())
    }
}
fn executable(name: &str) -> io::Result<PathBuf> {
    if matches!(name, "rustc.exe" | "cargo.exe") {
        let home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".rustup")))
            .ok_or_else(|| record::invalid("installed Rust toolchain home is missing"))?;
        return pinned_executable(&home, name);
    }
    for dir in std::env::split_paths(
        &std::env::var_os("PATH").ok_or_else(|| record::invalid("PATH is missing"))?,
    ) {
        let p = dir.join(name);
        if p.is_file() {
            return paths::canonical_no_reparse(&p);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("required installed tool {name} not on PATH"),
    ))
}
fn pinned_executable(home: &Path, name: &str) -> io::Result<PathBuf> {
    if !matches!(name, "rustc.exe" | "cargo.exe") {
        return Err(record::invalid("unknown pinned tool"));
    }
    // Use the installed compiler binary, not the symbolic rustup proxy. Resolving
    // a rustc proxy to rustup.exe would change argv[0] routing and its behavior.
    paths::canonical_no_reparse(
        &home
            .join("toolchains/1.99.0-x86_64-pc-windows-msvc/bin")
            .join(name),
    )
}
pub fn reject_cargo_config(export: &Path, cargo_home: &Path) -> io::Result<()> {
    for ancestor in export
        .parent()
        .ok_or_else(|| record::invalid("export parent missing"))?
        .ancestors()
    {
        for name in ["config", "config.toml"] {
            if ancestor.join(".cargo").join(name).try_exists()? {
                return Err(record::invalid(format!(
                    "external Cargo configuration: {}",
                    ancestor.display()
                )));
            }
        }
    }
    for name in ["config", "config.toml"] {
        if cargo_home.join(name).try_exists()? {
            return Err(record::invalid("CARGO_HOME must be config-free"));
        }
    }
    Ok(())
}
fn environment_from(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    cargo_home: &Path,
) -> Vec<(OsString, OsString)> {
    let mut env = Vec::new();
    for (key, value) in vars {
        let Some(name) = key.to_str() else { continue };
        if ENV_KEYS.contains(&name.to_ascii_uppercase().as_str()) {
            env.push((key, value));
        }
    }
    env.extend([
        ("CARGO_HOME".into(), cargo_home.as_os_str().to_owned()),
        ("CARGO_NET_OFFLINE".into(), "true".into()),
        ("RUSTUP_AUTO_INSTALL".into(), "0".into()),
        ("RUSTUP_TOOLCHAIN".into(), "1.99.0".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "NUL".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ]);
    env
}
fn parse_operator(value: &str) -> io::Result<(String, String)> {
    let (nick, task) = value
        .split_once('/')
        .ok_or_else(|| record::invalid("LockOwner requires nick/task"))?;
    if nick.is_empty() || task.is_empty() || value.contains(['\n', '\r', '=']) {
        return Err(record::invalid("invalid LockOwner"));
    }
    Ok((nick.into(), task.into()))
}

impl NativePackage {
    pub fn open(options: Options) -> io::Result<Self> {
        Self::open_at(options, Path::new(LOCK_ROOT))
    }
    /// Recover using a capability constructed by trusted integration only after
    /// Cameron's explicit, named authorization has been verified. CLI strings
    /// and unverified authorization files must never call this entry point.
    pub fn open_replacement(options: Options, authorization: &Replacement) -> io::Result<Self> {
        Self::open_authorized_at(options, Path::new(LOCK_ROOT), Some(authorization))
    }
    // The injectable slot root is private and lets host fixtures exercise the
    // production journal/acquisition ordering without touching machine leases.
    fn open_at(options: Options, lock_root: &Path) -> io::Result<Self> {
        Self::open_authorized_at(options, lock_root, None)
    }
    fn open_authorized_at(
        mut options: Options,
        lock_root: &Path,
        authorization: Option<&Replacement>,
    ) -> io::Result<Self> {
        if options
            .slot
            .as_deref()
            .is_some_and(|s| !matches!(s, "qemu" | "qemu2"))
        {
            return Err(record::invalid("unknown requested lane slot"));
        }
        if options.recover.is_none() && authorization.is_some() {
            return Err(record::invalid(
                "replacement capability applies only to recovery",
            ));
        }
        options.checkout = paths::canonical_no_reparse(&options.checkout)?;
        if paths::contains(
            &paths::resolve_no_reparse(Path::new("C:/Users/engin/Desktop/Mirror"))?,
            &options.checkout,
        ) {
            return Err(record::invalid("use your own task checkout"));
        }
        options.lane_root =
            paths::validate_lane_root(&options.lane_root, std::slice::from_ref(&options.checkout))?;
        let lock_root = paths::canonical_no_reparse(lock_root)?;
        let recovery = options.recover.is_some();
        let run = options
            .recover
            .clone()
            .map(Ok)
            .unwrap_or_else(|| unique("package"))?;
        package::validate_id(&run)?;
        let run_dir = options.lane_root.join("packages").join(&run);
        let scratch = run_dir.join("source");
        let output = options.checkout.join("build/dist");
        // Recovery performs no compilation: missing caches or newly added build
        // configuration must not obstruct no-live proof and bounded cleanup.
        let env = if recovery {
            Vec::new()
        } else {
            options.cargo_home = paths::canonical_no_reparse(&options.cargo_home)?;
            reject_cargo_config(&scratch, &options.cargo_home)?;
            tool_environment(&environment_from(std::env::vars_os(), &options.cargo_home))?
        };
        let (nick, task) = parse_operator(&options.operator)?;
        let owner = Owner {
            nick,
            task,
            worktree: options.checkout.clone(),
            started: utc_now(),
            kind: "package".into(),
            run: run.clone(),
            lease: unique("lease")?,
            lane_root: options.lane_root.clone(),
        };
        let (lease, journal, job, prior) = if recovery {
            paths::canonical_no_reparse(&run_dir)?;
            let mut journal = record::read(&run_dir.join("journal.json"))?;
            let replacement = validate_journal(
                &journal,
                &options,
                &run,
                &scratch,
                &lock_root,
                authorization,
            )?;
            let prior = package::load_prior(&run_dir, journal.get("progress")?)?;
            validate_prior(&prior, &journal)?;
            let slot = PathBuf::from(journal.get("lease_slot")?.string()?);
            if options
                .slot
                .as_ref()
                .is_some_and(|name| slot != lock_root.join(name))
            {
                return Err(record::invalid(
                    "requested slot differs from the recorded recovery slot",
                ));
            }
            journal.set(
                "replacement",
                replacement.as_ref().map_or(Value::Null, replacement_record),
            )?;
            // The acquisition's prewrite durably records named authorization
            // before the owner record or any shared slot can change.
            let lease = acquire_journaled(
                &run_dir,
                &mut journal,
                &owner,
                &[slot],
                true,
                replacement.as_ref(),
            )?;
            (lease, journal, None, Some(prior))
        } else {
            fs::create_dir_all(run_dir.parent().unwrap())?;
            paths::canonical_no_reparse(run_dir.parent().unwrap())?;
            fs::create_dir(&run_dir)?;
            let job = Job::new(&job_name(&run))?;
            let mut journal = Value::object([
                ("schema", Value::Number(1)),
                ("kind", Value::text("package")),
                ("run", Value::text(&run)),
                ("operator", Value::text(&options.operator)),
                ("replacement", Value::Null),
                ("root", Value::text(text(&options.lane_root)?)),
                ("original_checkout", Value::text(text(&options.checkout)?)),
                ("scratch", Value::text(text(&scratch)?)),
                ("output", Value::text(text(&output)?)),
                (
                    "controller",
                    identity(windows_job::current_process_identity()?),
                ),
                ("job", Value::text(job.identity())),
                ("workers", Value::Array(vec![])),
                ("lease_slot", Value::Null),
                ("lease_identity", Value::Null),
                ("lease_id", Value::Null),
                (
                    "acquisition_controller",
                    identity(windows_job::current_process_identity()?),
                ),
                ("detached_before_acquire", Value::Array(vec![])),
                ("release_detach", Value::Array(vec![])),
                ("release_cleanup", Value::Null),
                (
                    "environment",
                    Value::Array(
                        env.iter()
                            .map(|(k, v)| {
                                Value::object([
                                    ("key", Value::text(k.to_string_lossy())),
                                    ("value", Value::text(v.to_string_lossy())),
                                ])
                            })
                            .collect(),
                    ),
                ),
                ("commands", Value::Array(vec![])),
                ("progress", Progress::default().record()),
            ]);
            // Every slot attempt first persists all identities, paths and the
            // planned slot. A crash before the post-acquire write is recoverable
            // through that same-run lease; no worker can have been resumed.
            let slots = options.slot.as_ref().map_or_else(
                || vec![lock_root.join("qemu"), lock_root.join("qemu2")],
                |name| vec![lock_root.join(name)],
            );
            let lease = acquire_journaled(&run_dir, &mut journal, &owner, &slots, false, None)?;
            (lease, journal, Some(job), None)
        };
        Ok(Self {
            options,
            run,
            run_dir,
            scratch,
            output,
            job,
            lease: Some(lease),
            journal,
            environment: env,
            commit: String::new(),
            tree: String::new(),
            toolchain: String::new(),
            capabilities: vec![],
            source_paths: vec![],
            inventory: vec![],
            total_streams: 0,
            command_number: 0,
            prior,
            recovery,
            released_cleanup: None,
        })
    }
    pub fn execute(mut self) -> io::Result<Progress> {
        let previous = self.prior.take();
        let result = if self.recovery {
            package::recover(&mut self, previous)
        } else {
            package::run(&mut self)
        };
        eprintln!("package {} record: {}", self.run, self.run_dir.display());
        result
    }
    fn command(
        &mut self,
        program: &str,
        args: Vec<OsString>,
        cwd: &Path,
        end: Instant,
    ) -> io::Result<windows_job::CommandOutput> {
        check_time(end)?;
        command_budget(self.total_streams, self.command_number)?;
        self.command_number += 1;
        let n = self.command_number;
        let captures = CapturePaths {
            stdout: self.run_dir.join(format!("command-{n:03}.stdout")),
            stderr: self.run_dir.join(format!("command-{n:03}.stderr")),
        };
        let spec = CommandSpec {
            program: tool_path(&executable(program)?)?,
            args,
            cwd: tool_path(cwd)?,
            env: self.environment.clone(),
        };
        let mut commands = self.journal.get("commands")?.array()?.to_vec();
        commands.push(Value::object([
            ("number", Value::Number(n as u64)),
            ("program", Value::text(text(&spec.program)?)),
            (
                "args",
                Value::Array(
                    spec.args
                        .iter()
                        .map(|v| Value::text(v.to_string_lossy()))
                        .collect(),
                ),
            ),
            ("cwd", Value::text(text(cwd)?)),
        ]));
        self.journal.set("commands", Value::Array(commands))?;
        let job = self
            .job
            .as_ref()
            .ok_or_else(|| record::invalid("recovery must not start build workers"))?;
        let journal = &mut self.journal;
        let path = self.run_dir.join("journal.json");
        let worker = job.spawn_captured(&spec, &captures, |id| {
            let mut workers = journal.get("workers")?.array()?.to_vec();
            workers.push(identity(id));
            journal.set("workers", Value::Array(workers))?;
            record::write(&path, journal)
        })?;
        let out = job.wait(worker, end)?;
        self.total_streams += out.stdout.len() + out.stderr.len();
        // Descendants that closed their output pipes still belong to this job.
        // Waiting out the deadline is a failed command, never a successful build.
        job.wait_empty(end)?;
        check_time(end)?;
        Ok(out)
    }
    fn checked(
        &mut self,
        program: &str,
        args: Vec<OsString>,
        cwd: &Path,
        end: Instant,
    ) -> io::Result<Vec<u8>> {
        let out = self.command(program, args, cwd, end)?;
        if out.exit_code != 0 {
            return Err(io::Error::other(format!(
                "{program} exited {} (command {}); see retained output",
                out.exit_code, self.command_number
            )));
        }
        Ok(out.stdout)
    }
    fn git(&mut self, args: &[&str], end: Instant) -> io::Result<String> {
        let cwd = self.options.checkout.clone();
        let out = self.checked(
            "git.exe",
            args.iter().map(OsString::from).collect(),
            &cwd,
            end,
        )?;
        String::from_utf8(out)
            .map(|s| s.trim().into())
            .map_err(|_| record::invalid("Git returned non-UTF8"))
    }
    fn require_clean_checkout(&mut self, end: Instant) -> io::Result<()> {
        // System/global configuration and their filter definitions stay
        // disabled. Explicitly retain Git for Windows' built-in CRLF text
        // normalization for the worktree comparison; archive still reads HEAD
        // bytes and -text attributes continue to require byte equality. A
        // status-only check can report stale stat differences after changing
        // normalization, even when an actual content comparison is empty.
        // Compare the index to HEAD separately, without text normalization, so
        // a staged edit canceled by the worktree cannot be accepted as clean.
        self.git(
            &[
                "diff",
                "--cached",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                "HEAD",
                "--",
            ],
            end,
        )?;
        self.git(
            &[
                "-c",
                "core.autocrlf=true",
                "diff",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                "--",
            ],
            end,
        )?;
        if !self
            .git(
                &[
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "--directory",
                    "--no-empty-directory",
                    "-z",
                ],
                end,
            )?
            .is_empty()
        {
            return Err(record::invalid(
                "lane packaging requires a clean committed checkout",
            ));
        }
        Ok(())
    }
    fn archive_source(&mut self, end: Instant) -> io::Result<PathBuf> {
        let archive = self.run_dir.join("source.zip");
        let cwd = self.options.checkout.clone();
        self.checked(
            "git.exe",
            vec![
                "archive".into(),
                "--format=zip".into(),
                format!("--output={}", text(&tool_path(&archive)?)?).into(),
                self.commit.clone().into(),
            ],
            &cwd,
            end,
        )?;
        // Paths are data in a script argument array; never interpolated into code.
        let script = self.run_dir.join("expand.ps1");
        record::atomic_write(&script,b"param([string]$Archive,[string]$Destination)\n$ErrorActionPreference='Stop'\nExpand-Archive -LiteralPath $Archive -DestinationPath $Destination\n")?;
        self.checked(
            "powershell.exe",
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                tool_arg(&script)?,
                tool_arg(&archive)?,
                tool_arg(&self.scratch)?,
            ],
            &cwd,
            end,
        )?;
        Ok(archive)
    }
    fn inventory(&self, end: Instant) -> io::Result<Vec<SourceFile>> {
        source_inventory(&self.scratch, &self.source_paths, end)
    }
    // Keep the real Git ancestry gate injectable only through this private
    // helper. Production callers always use the fixed CAPABILITIES commit IDs.
    fn require_capability_ancestor(&mut self, required: &str, end: Instant) -> io::Result<String> {
        let id = self.git(
            &["rev-parse", "--verify", &format!("{required}^{{commit}}")],
            end,
        )?;
        let commit = self.commit.clone();
        self.git(&["merge-base", "--is-ancestor", &id, &commit], end)?;
        Ok(id)
    }
}
fn job_name(run: &str) -> String {
    format!(r"Local\MirrorLane-{run}")
}

fn validate_slot(slot: &Path, root: &Path) -> io::Result<()> {
    let slot = paths::resolve_no_reparse(slot)?;
    if ![root.join("qemu"), root.join("qemu2")].contains(&slot) {
        return Err(record::invalid("journal names an unknown shared slot"));
    }
    Ok(())
}
fn file_identity(value: &str) -> bool {
    value.len() == 25
        && value.as_bytes()[8] == b':'
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_hexdigit())
}
fn validate_journal(
    journal: &Value,
    options: &Options,
    run: &str,
    scratch: &Path,
    lock_root: &Path,
    authorization: Option<&Replacement>,
) -> io::Result<Option<Replacement>> {
    journal.fields(&[
        "schema",
        "kind",
        "run",
        "operator",
        "replacement",
        "root",
        "original_checkout",
        "scratch",
        "output",
        "controller",
        "job",
        "workers",
        "lease_slot",
        "lease_identity",
        "lease_id",
        "acquisition_controller",
        "detached_before_acquire",
        "release_detach",
        "release_cleanup",
        "environment",
        "commands",
        "progress",
    ])?;
    if journal.get("schema")?.number()? != 1
        || journal.get("kind")?.string()? != "package"
        || journal.get("run")?.string()? != run
        || Path::new(journal.get("root")?.string()?) != options.lane_root
        || Path::new(journal.get("scratch")?.string()?) != scratch
        || journal.get("job")?.string()? != job_name(run)
    {
        return Err(record::invalid("package recovery identity mismatch"));
    }
    parse_identity(journal.get("controller")?)?;
    parse_identity(journal.get("acquisition_controller")?)?;
    for worker in journal.get("workers")?.array()? {
        parse_identity(worker)?;
    }
    let original =
        paths::resolve_no_reparse(Path::new(journal.get("original_checkout")?.string()?))?;
    if Path::new(journal.get("output")?.string()?) != original.join("build/dist") {
        return Err(record::invalid(
            "recorded output does not match original checkout",
        ));
    }
    validate_slot(Path::new(journal.get("lease_slot")?.string()?), lock_root)?;
    package::validate_id(journal.get("lease_id")?.string()?)?;
    if let Value::String(value) = journal.get("lease_identity")? {
        let Some((directory, owner)) = value
            .strip_prefix("directory=")
            .and_then(|v| v.split_once(";owner="))
        else {
            return Err(record::invalid("invalid recorded lease identity"));
        };
        if !file_identity(directory) || !file_identity(owner) {
            return Err(record::invalid("invalid recorded lease identity"));
        }
    } else if journal.get("lease_identity")? != &Value::Null {
        return Err(record::invalid("invalid pending lease identity"));
    }
    let mut intents = std::collections::BTreeSet::new();
    for event in journal
        .get("detached_before_acquire")?
        .array()?
        .iter()
        .chain(journal.get("release_detach")?.array()?)
    {
        event.fields(&["phase", "path", "identity"])?;
        let path = paths::resolve_no_reparse(Path::new(event.get("path")?.string()?))?;
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| record::invalid("detached path missing name"))?;
        if path.parent() != Some(lock_root)
            || !["qemu.released.", "qemu2.released."].iter().any(|prefix| {
                name.strip_prefix(prefix).is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit() || b == b'-')
                })
            })
        {
            return Err(record::invalid(
                "detached generation is outside the slot root",
            ));
        }
        let id = event.get("identity")?.string()?;
        if !file_identity(id) {
            return Err(record::invalid("invalid detached file identity"));
        }
        let key = (text(&path)?, id.to_owned());
        match event.get("phase")?.string()? {
            "intent" => {
                intents.insert(key);
            }
            "detached" if intents.contains(&key) => {}
            _ => {
                return Err(record::invalid(
                    "detached generation lacks prior durable intent",
                ));
            }
        }
    }
    if journal.get("release_cleanup")? != &Value::Null {
        journal.get("release_cleanup")?.string()?;
    }
    validate_replacement(journal, options, run, authorization)
}
fn replacement_record(cap: &Replacement) -> Value {
    Value::object([
        ("run", Value::text(&cap.run)),
        ("root", Value::text(cap.lane_root.to_string_lossy())),
        ("original_operator", Value::text(&cap.original_operator)),
        (
            "replacement_operator",
            Value::text(&cap.replacement_operator),
        ),
        ("scope", Value::text(&cap.scope)),
        (
            "authorization_reference",
            Value::text(&cap.authorization_reference),
        ),
    ])
}
fn parse_replacement(value: &Value) -> io::Result<Replacement> {
    value.fields(&[
        "run",
        "root",
        "original_operator",
        "replacement_operator",
        "scope",
        "authorization_reference",
    ])?;
    Ok(Replacement {
        run: value.get("run")?.string()?.into(),
        lane_root: PathBuf::from(value.get("root")?.string()?),
        original_operator: value.get("original_operator")?.string()?.into(),
        replacement_operator: value.get("replacement_operator")?.string()?.into(),
        scope: value.get("scope")?.string()?.into(),
        authorization_reference: value.get("authorization_reference")?.string()?.into(),
    })
}
fn validate_replacement(
    journal: &Value,
    options: &Options,
    run: &str,
    provided: Option<&Replacement>,
) -> io::Result<Option<Replacement>> {
    let original = journal.get("operator")?.string()?;
    parse_operator(original)?;
    let retained = match journal.get("replacement")? {
        Value::Null => None,
        value => Some(parse_replacement(value)?),
    };
    let mut effective = match (retained, provided) {
        (Some(retained), Some(provided)) => {
            let mut provided = provided.clone();
            provided.lane_root = paths::canonical_no_reparse(&provided.lane_root)?;
            if replacement_record(&retained) != replacement_record(&provided) {
                return Err(record::invalid(
                    "replacement capability differs from durable authorization",
                ));
            }
            Some(retained)
        }
        (Some(retained), None) => Some(retained),
        (None, Some(provided)) => Some(provided.clone()),
        (None, None) => None,
    };
    if let Some(cap) = effective.as_mut() {
        for value in [
            &cap.run,
            &cap.original_operator,
            &cap.replacement_operator,
            &cap.scope,
            &cap.authorization_reference,
        ] {
            if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
                return Err(record::invalid("malformed replacement capability"));
            }
        }
        cap.lane_root = paths::canonical_no_reparse(&cap.lane_root)?;
        parse_operator(&cap.replacement_operator)?;
        if cap.run != run
            || cap.lane_root != options.lane_root
            || cap.original_operator != original
            || cap.replacement_operator != options.operator
            || cap.original_operator == cap.replacement_operator
            || cap.scope != "package-recover"
        {
            return Err(record::invalid(
                "replacement authorization does not name this exact package recovery",
            ));
        }
    } else if original != options.operator {
        return Err(record::invalid(
            "foreign recovery requires named explicit authorization",
        ));
    }
    Ok(effective)
}
fn validate_prior(p: &Progress, journal: &Value) -> io::Result<()> {
    let run = journal.get("run")?.string()?;
    if p.manifest.as_ref().is_some_and(|m| m.package_id != run) {
        return Err(record::invalid(
            "final manifest belongs to another package run",
        ));
    }
    if let Some(proof) = p.lock_release.as_ref()
        && (proof.get("run")?.string()? != run
            || proof.get("slot")? != journal.get("lease_slot")?)
    {
        return Err(record::invalid(
            "final release belongs to another package run or slot",
        ));
    }
    // A recovery crash can leave an old finalized proof alongside the new
    // lease identity in the journal. Those generations must not be conflated.
    Ok(())
}

fn record_detach(
    run_dir: &Path,
    journal: &mut Value,
    field: &str,
    event: &DetachEvent,
) -> io::Result<()> {
    let mut events = journal.get(field)?.array()?.to_vec();
    events.push(Value::object([
        (
            "phase",
            Value::text(match event.phase {
                DetachPhase::Intent => "intent",
                DetachPhase::Detached => "detached",
            }),
        ),
        ("path", Value::text(text(&event.generation.path)?)),
        (
            "identity",
            Value::text(event.generation.identity.to_string()),
        ),
    ]));
    journal.set(field, Value::Array(events))?;
    record::write(&run_dir.join("journal.json"), journal)
}
fn acquire_journaled(
    run_dir: &Path,
    journal: &mut Value,
    owner: &Owner,
    slots: &[PathBuf],
    recovery: bool,
    replacement: Option<&Replacement>,
) -> io::Result<Lease> {
    let mut errors = Vec::new();
    for slot in slots {
        journal.set("lease_slot", Value::text(text(slot)?))?;
        journal.set("lease_identity", Value::Null)?;
        journal.set("lease_id", Value::text(&owner.lease))?;
        journal.set(
            "acquisition_controller",
            identity(windows_job::current_process_identity()?),
        )?;
        record::write(&run_dir.join("journal.json"), journal)?;
        let observer =
            |event: &DetachEvent| record_detach(run_dir, journal, "detached_before_acquire", event);
        let lease = if recovery {
            Lease::recover_with_observer(slot, owner.clone(), replacement, observer)
        } else {
            Lease::acquire_with_observer(slot, owner.clone(), observer)
        };
        match lease {
            Ok(lease) => {
                journal.set("lease_identity", Value::text(lease.identity()))?;
                journal.set("lease_id", Value::text(&lease.owner().lease))?;
                // On write failure, the owned handles close but the unreleased
                // owner record remains paired with the pre-acquire journal.
                record::write(&run_dir.join("journal.json"), journal)?;
                return Ok(lease);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                errors.push(format!("{}: {e}", slot.display()))
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::WouldBlock, errors.join("; ")))
}

impl Backend for NativePackage {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn persist(&mut self, p: &Progress, final_record: bool) -> io::Result<()> {
        self.journal.set("progress", p.record())?;
        record::write(&self.run_dir.join("journal.json"), &self.journal)?;
        if final_record {
            record::write(&self.run_dir.join("final.json"), &p.record())?;
        }
        Ok(())
    }
    fn validate(&mut self, end: Instant) -> io::Result<()> {
        let top = self.git(&["rev-parse", "--show-toplevel"], end)?;
        require_checkout_root(&self.options.checkout, Path::new(&top))?;
        self.require_clean_checkout(end)?;
        self.commit = self.git(&["rev-parse", "--verify", "HEAD^{commit}"], end)?;
        self.tree = self.git(&["rev-parse", "--verify", "HEAD^{tree}"], end)?;
        let worktrees = self.git(&["worktree", "list", "--porcelain"], end)?;
        let paths_list = worktrees
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        paths::validate_lane_root(&self.options.lane_root, &paths_list)?;
        for (name, required) in package::CAPABILITIES {
            let id = self.require_capability_ancestor(required, end)?;
            self.capabilities.push((name.into(), id));
        }
        let cwd = self.options.checkout.clone();
        let commit = self.commit.clone();
        let tree = self.checked(
            "git.exe",
            vec![
                "ls-tree".into(),
                "-rz".into(),
                "--full-tree".into(),
                commit.into(),
            ],
            &cwd,
            end,
        )?;
        self.source_paths = parse_source_paths(&tree)?;
        let rust = self.checked("rustc.exe", vec!["--version".into()], &cwd, end)?;
        self.toolchain = String::from_utf8(rust)
            .map_err(|_| record::invalid("invalid rustc version"))?
            .trim()
            .into();
        if !self.toolchain.starts_with("rustc 1.99.0 ") {
            return Err(record::invalid("wrong pinned toolchain"));
        }
        let (_, hash) = sha256::file_until(&self.options.limine_archive, end)?;
        if hash != LIMINE_SHA {
            return Err(record::invalid("cached Limine archive checksum mismatch"));
        }
        Ok(())
    }
    fn export(&mut self, end: Instant) -> io::Result<()> {
        let archive = self.archive_source(end)?;
        paths::canonical_no_reparse(&self.scratch)?;
        reject_cargo_config(&self.scratch, &self.options.cargo_home)?;
        self.inventory = self.inventory(end)?;
        let private_archive = self
            .scratch
            .join(format!(".tools/limine/{LIMINE_VERSION}/limine-binary.zip"));
        fs::create_dir_all(private_archive.parent().unwrap())?;
        copy_verified(&self.options.limine_archive, &private_archive, end)?;
        // The fixed cache is rechecked in build.ps1; it can never enter its download branch.
        if sha256::file_until(&private_archive, end)?.1 != LIMINE_SHA {
            return Err(record::invalid("private Limine archive changed"));
        }
        fs::remove_file(archive)?;
        Ok(())
    }
    fn build(&mut self, end: Instant) -> io::Result<()> {
        let cwd = self.scratch.clone();
        for manifest in ["Cargo.toml", "tools/mirror-image/Cargo.toml"] {
            self.checked(
                "cargo.exe",
                vec![
                    "metadata".into(),
                    "--locked".into(),
                    "--offline".into(),
                    "--format-version".into(),
                    "1".into(),
                    "--manifest-path".into(),
                    manifest.into(),
                ],
                &cwd,
                end,
            )?;
        }
        let mut args = vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-File".into(),
            tool_arg(&cwd.join("scripts/build.ps1"))?,
        ];
        if self.options.release {
            args.push("-Release".into());
        }
        self.checked("powershell.exe", args, &cwd, end)?;
        let config =
            record::read_bounded(&cwd.join("build/esp/limine.conf"), record::RECORD_LIMIT)?;
        tokenless_config(&config)?;
        if self.inventory(end)? != self.inventory {
            return Err(record::invalid(
                "committed source changed during private build",
            ));
        }
        Ok(())
    }
    fn publish(&mut self, end: Instant) -> io::Result<Manifest> {
        self.prove_no_live_build(false, end)?;
        if self.inventory(end)? != self.inventory {
            return Err(record::invalid(
                "committed source changed before publication",
            ));
        }
        paths::validate_child(&self.options.checkout, &self.output)?;
        fs::create_dir_all(&self.output)?;
        paths::canonical_no_reparse(&self.output)?;
        // Invalidate provenance BEFORE touching either destination artifact.
        match fs::remove_file(self.output.join("mirror.package.json")) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let image = copy_verified(
            &self.scratch.join("build/mirror.img"),
            &self.output.join("mirror.img"),
            end,
        )?;
        let descriptor = copy_verified(
            &self.scratch.join("build/mirror.vmdk"),
            &self.output.join("mirror.vmdk"),
            end,
        )?;
        let m = Manifest {
            package_id: self.run.clone(),
            commit: self.commit.clone(),
            tree: self.tree.clone(),
            release: self.options.release,
            toolchain: self.toolchain.clone(),
            capabilities: self.capabilities.clone(),
            inventory: self.inventory.clone(),
            image,
            descriptor,
        };
        Manifest::parse(&m.record())?;
        record::write(&self.output.join("mirror.package.json"), &m.record())?;
        record::write(&self.run_dir.join("manifest.json"), &m.record())?;
        Ok(m)
    }
    fn terminate_workers(&mut self) -> io::Result<()> {
        self.job.as_ref().map_or(Ok(()), Job::terminate)
    }
    fn prove_no_live_build(&mut self, recovery: bool, end: Instant) -> io::Result<()> {
        check_time(end)?;
        if recovery {
            let workers = self
                .journal
                .get("workers")?
                .array()?
                .iter()
                .map(parse_identity)
                .collect::<io::Result<Vec<_>>>()?;
            windows_job::prove_recovery(
                parse_identity(self.journal.get("controller")?)?,
                &workers,
                self.journal.get("job")?.string()?,
            )?;
        } else {
            // Every worker enters this no-breakaway job before it can run.
            // The retained job handle proves the whole build tree empty;
            // reopening old PIDs could instead observe unrelated processes.
            self.job
                .as_ref()
                .ok_or_else(|| record::invalid("missing worker job"))?
                .wait_empty(end)?;
        }
        Ok(())
    }
    fn cleanup(&mut self, end: Instant) -> io::Result<()> {
        check_time(end)?;
        paths::remove_scratch_until(&self.options.lane_root, &self.scratch, end)?;
        check_time(end)
    }
    fn scratch_path(&self) -> String {
        self.scratch.display().to_string()
    }
    fn revalidate(&mut self, m: &Manifest, end: Instant) -> io::Result<()> {
        check_time(end)?;
        if m.package_id != self.run
            || Manifest::parse(&record::read(&self.run_dir.join("manifest.json"))?)? != *m
        {
            return Err(record::invalid("durable package manifest mismatch"));
        }
        // Recovery may read the old recorded publication, but never writes it.
        let output = paths::canonical_no_reparse(Path::new(self.journal.get("output")?.string()?))?;
        let original = paths::canonical_no_reparse(Path::new(
            self.journal.get("original_checkout")?.string()?,
        ))?;
        if output != original.join("build/dist") {
            return Err(record::invalid(
                "recorded output does not match original checkout",
            ));
        }
        if Manifest::parse(&record::read(&output.join("mirror.package.json"))?)? != *m {
            return Err(record::invalid("published manifest changed"));
        }
        for (name, expected) in [("mirror.img", &m.image), ("mirror.vmdk", &m.descriptor)] {
            check_time(end)?;
            let (size, sha256) =
                sha256::file_until(&paths::canonical_no_reparse(&output.join(name))?, end)?;
            if *expected != (Artifact { size, sha256 }) {
                return Err(record::invalid("published package hash mismatch"));
            }
        }
        Ok(())
    }
    fn publish_release(&mut self) -> io::Result<Value> {
        let r = self
            .lease
            .as_mut()
            .ok_or_else(|| record::invalid("lease already closed"))?
            .publish_release(&utc_now())?;
        Ok(Value::object([
            ("state", Value::text("owner-record")),
            ("slot", Value::text(text(&r.slot)?)),
            ("directory", Value::text(r.directory_identity.to_string())),
            ("owner", Value::text(r.owner_identity.to_string())),
            ("run", Value::text(r.run)),
            ("lease", Value::text(r.lease)),
            ("released", Value::text(r.released)),
        ]))
    }
    fn close_released(&mut self) -> io::Result<Option<Value>> {
        let lease = self
            .lease
            .take()
            .ok_or_else(|| record::invalid("lease already closed"))?;
        let slot = lease.slot().to_owned();
        let owner = lease.owner().clone();
        let mut proof = self.journal.get("progress")?.get("lock_release")?.clone();
        let release = lease
            .release_record()
            .ok_or_else(|| record::invalid("lease was not released"))?
            .clone();
        // finalize has already flushed both the owner's release and its final
        // proof. Optional detach never deletes at a shared pathname and failure
        // cannot undo the valid durable release.
        match lease.detach_released_with_observer(|event| {
            record_detach(&self.run_dir, &mut self.journal, "release_detach", event)
        }) {
            Ok(detached) => {
                eprintln!(
                    "lock free for package {}; detached generation {} identity={}",
                    self.run,
                    detached.path.display(),
                    detached.identity
                );
                proof.set("state", Value::text("detached"))?;
                proof.set("detached_path", Value::text(text(&detached.path)?))?;
                proof.set(
                    "detached_identity",
                    Value::text(detached.identity.to_string()),
                )?;
                self.released_cleanup = Some((detached, release, owner));
                return Ok(Some(proof));
            }
            Err(error) => {
                let message = format!(
                    "released slot {} retained or detach completion uncertain: {error}; inspect durable detach intent",
                    slot.display()
                );
                eprintln!("{message}");
                self.journal.set("release_cleanup", Value::text(message))?;
                // Diagnostics are best effort here: the authoritative owner
                // record and final proof were persisted before optional cleanup.
                if let Err(e) = record::write(&self.run_dir.join("journal.json"), &self.journal) {
                    eprintln!("release cleanup diagnostic could not be persisted: {e}");
                }
            }
        }
        Ok(None)
    }
    fn cleanup_released(&mut self) -> io::Result<bool> {
        let Some((detached, release, owner)) = self.released_cleanup.take() else {
            return Ok(false);
        };
        match lease::cleanup_detached(&detached, &release, &owner) {
            Ok(()) => {
                eprintln!(
                    "removed own released generation {}",
                    detached.path.display()
                );
                Ok(true)
            }
            Err(error) => {
                let message = format!(
                    "detached generation {} identity={} retained: {error}",
                    detached.path.display(),
                    detached.identity
                );
                eprintln!("{message}");
                self.journal.set("release_cleanup", Value::text(message))?;
                record::write(&self.run_dir.join("journal.json"), &self.journal)?;
                Ok(false)
            }
        }
    }
}
pub fn copy_verified(source: &Path, destination: &Path, end: Instant) -> io::Result<Artifact> {
    copy_verified_with(source, destination, end, || Ok(()))
}
fn copy_verified_with(
    source: &Path,
    destination: &Path,
    end: Instant,
    before_verify: impl FnOnce() -> io::Result<()>,
) -> io::Result<Artifact> {
    check_time(end)?;
    paths::canonical_no_reparse(source)?;
    paths::resolve_no_reparse(destination)?;
    let temporary = destination.with_file_name(format!(
        "{}.{}.tmp",
        destination.file_name().unwrap().to_string_lossy(),
        unique("copy")?
    ));
    let mut from = File::open(source)?;
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        let mut buffer = [0; 65536];
        loop {
            check_time(end)?;
            let n = from.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            to.write_all(&buffer[..n])?;
        }
        to.sync_all()?;
        drop(to);
        before_verify()?;
        let (size, hash) = sha256::file_until(&temporary, end)?;
        if sha256::file_until(source, end)? != (size, hash.clone()) {
            return Err(record::invalid("copy hash mismatch"));
        }
        check_time(end)?;
        fs::rename(&temporary, destination)?;
        Ok(Artifact { size, sha256: hash })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn command_budget(total_streams: usize, command_number: usize) -> io::Result<()> {
    if total_streams > TOTAL_STREAMS - 2 * windows_job::STREAM_LIMIT || command_number >= 128 {
        return Err(record::invalid("command evidence budget exhausted"));
    }
    Ok(())
}
fn require_checkout_root(checkout: &Path, top: &Path) -> io::Result<()> {
    if checkout != paths::canonical_no_reparse(top)? {
        return Err(record::invalid(
            "lane checkout must be the Git worktree root",
        ));
    }
    Ok(())
}
fn parse_source_paths(tree: &[u8]) -> io::Result<Vec<String>> {
    let mut paths = Vec::new();
    if !tree.ends_with(&[0]) {
        return Err(record::invalid("unterminated ls-tree output"));
    }
    for entry in tree.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let value =
            std::str::from_utf8(entry).map_err(|_| record::invalid("non-UTF8 source path"))?;
        let (meta, path) = value
            .split_once('\t')
            .ok_or_else(|| record::invalid("malformed ls-tree"))?;
        let fields: Vec<_> = meta.split(' ').collect();
        if fields.len() != 3
            || !matches!(fields[0], "100644" | "100755")
            || fields[1] != "blob"
            || fields[2].len() != 40
            || !fields[2].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(record::invalid(
                "source symlinks/submodules are not archive inputs",
            ));
        }
        package::validate_source_path(path)?;
        paths.push(path.to_owned());
    }
    paths.sort();
    if paths.is_empty() || paths.windows(2).any(|w| w[0] == w[1]) {
        return Err(record::invalid("empty or duplicate source tree"));
    }
    Ok(paths)
}
fn source_inventory(scratch: &Path, names: &[String], end: Instant) -> io::Result<Vec<SourceFile>> {
    names
        .iter()
        .map(|name| {
            check_time(end)?;
            package::validate_source_path(name)?;
            let path = paths::canonical_no_reparse(&scratch.join(name))?;
            let (size, hash) = sha256::file_until(&path, end)?;
            Ok(SourceFile {
                path: name.clone(),
                artifact: Artifact { size, sha256: hash },
            })
        })
        .collect()
}
fn tokenless_config(config: &[u8]) -> io::Result<()> {
    let config =
        std::str::from_utf8(config).map_err(|_| record::invalid("invalid Limine configuration"))?;
    if config.contains("mirror.perf=") || config.contains("mirror.test=") {
        return Err(record::invalid("lane package must be tokenless"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::Verdict;
    use std::time::Duration;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(unique("mirror-native-package-test").unwrap());
            fs::create_dir(&path).unwrap();
            Self(paths::canonical_no_reparse(&path).unwrap())
        }
        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }
        fn options(&self) -> Options {
            Options {
                checkout: self.dir("checkout"),
                lane_root: self.dir("lane"),
                operator: "fixture/package".into(),
                release: false,
                recover: None,
                slot: None,
                cargo_home: self.dir("cargo-home"),
                limine_archive: self.0.join("cache.zip"),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn end() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }
    fn release_fixture(p: &mut NativePackage) {
        assert!(
            p.journal
                .get("workers")
                .unwrap()
                .array()
                .unwrap()
                .is_empty()
        );
        p.lease
            .as_mut()
            .unwrap()
            .publish_release(&utc_now())
            .unwrap();
        p.lease.take();
    }
    fn valid_progress() -> Progress {
        let artifact = Artifact {
            size: 3,
            sha256: sha256::bytes(b"abc"),
        };
        Progress {
            verdict: Verdict::FinalizedValid,
            phase: "finalized".into(),
            release_intent: true,
            no_live_build: true,
            manifest: Some(Manifest {
                package_id: "package-test".into(),
                commit: "1".repeat(40),
                tree: "2".repeat(40),
                release: false,
                toolchain: "rustc 1.99.0 (fixture)".into(),
                capabilities: package::CAPABILITIES
                    .iter()
                    .map(|(n, c)| (n.to_string(), c.to_string()))
                    .collect(),
                inventory: vec![SourceFile {
                    path: "Cargo.toml".into(),
                    artifact: artifact.clone(),
                }],
                image: artifact.clone(),
                descriptor: artifact,
            }),
            ..Progress::default()
        }
    }
    #[test]
    fn environment_is_allowlisted_and_forces_offline_pin() {
        assert!(
            parse_identity(&identity(ProcessIdentity {
                pid: 0,
                start_time: 1
            }))
            .is_err()
        );
        assert!(
            parse_identity(&identity(ProcessIdentity {
                pid: 1,
                start_time: 0
            }))
            .is_err()
        );
        let vars = [
            ("Path", "fixture-path"),
            ("SystemRoot", "fixture-system"),
            ("CARGO_HOME", "wrong"),
            ("RUSTFLAGS", "injected"),
            ("CARGO_ENCODED_RUSTFLAGS", "injected"),
            ("RUSTUP_TOOLCHAIN", "nightly"),
            ("RUSTUP_AUTO_INSTALL", "1"),
            ("CL", "injected"),
            ("GIT_CONFIG_COUNT", "1"),
            ("UNRELATED_SECRET", "must-not-copy"),
        ];
        let env = environment_from(
            vars.map(|(k, v)| (k.into(), v.into())),
            Path::new("C:/cache"),
        );
        let normalized: std::collections::BTreeMap<_, _> = env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_ascii_uppercase(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(normalized["PATH"], "fixture-path");
        assert_eq!(normalized["CARGO_HOME"], "C:/cache");
        assert_eq!(normalized["CARGO_NET_OFFLINE"], "true");
        assert_eq!(normalized["RUSTUP_TOOLCHAIN"], "1.99.0");
        assert_eq!(normalized["RUSTUP_AUTO_INSTALL"], "0");
        assert!(!normalized.contains_key("RUSTFLAGS"));
        assert!(!normalized.contains_key("CARGO_ENCODED_RUSTFLAGS"));
        assert!(!normalized.contains_key("CL"));
        assert!(!normalized.contains_key("GIT_CONFIG_COUNT"));
        assert!(!normalized.contains_key("UNRELATED_SECRET"));
    }
    #[test]
    fn external_cargo_config_rejected_but_committed_project_config_allowed() {
        let f = Fixture::new();
        let export = f.dir("parent/source");
        let cache = f.dir("cache");
        fs::create_dir(export.join(".cargo")).unwrap();
        fs::write(export.join(".cargo/config.toml"), "[build]").unwrap();
        reject_cargo_config(&export, &cache).unwrap();
        fs::write(cache.join("config.toml"), "[build]").unwrap();
        assert!(reject_cargo_config(&export, &cache).is_err());
        fs::remove_file(cache.join("config.toml")).unwrap();
        let parent_config = f.dir("parent/.cargo").join("config");
        fs::write(parent_config, "[build]").unwrap();
        assert!(reject_cargo_config(&export, &cache).is_err());
    }
    #[test]
    fn missing_final_preserves_journal_failure_but_cannot_grant_validity() {
        let f = Fixture::new();
        let valid = valid_progress();
        let journal = Value::object([
            ("run", Value::text("package-test")),
            ("progress", valid.record()),
        ]);
        assert_eq!(
            package::load_prior(&f.0, journal.get("progress").unwrap())
                .unwrap()
                .verdict,
            Verdict::Incomplete
        );
        record::write(&f.0.join("final.json"), &valid.record()).unwrap();
        assert_eq!(
            package::load_prior(&f.0, journal.get("progress").unwrap())
                .unwrap()
                .verdict,
            Verdict::FinalizedValid
        );
        fs::remove_file(f.0.join("final.json")).unwrap();
        let failed = Progress {
            first_failure: Some("original worker failed".into()),
            ..Progress::default()
        };
        assert_eq!(
            package::load_prior(&f.0, &failed.record())
                .unwrap()
                .first_failure,
            failed.first_failure
        );
        fs::write(f.0.join("final.json"), b"corrupt").unwrap();
        assert!(
            package::load_prior(&f.0, journal.get("progress").unwrap()).is_err(),
            "malformed final must not silently fall back"
        );
    }
    #[test]
    fn recovery_skips_build_inputs_and_records_fresh_lease_and_detach() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
        let run = p.run.clone();
        let original_lease = p.lease.as_ref().unwrap().identity();
        assert_eq!(
            record::read(&p.run_dir.join("journal.json"))
                .unwrap()
                .get("lease_identity")
                .unwrap()
                .string()
                .unwrap(),
            original_lease
        );
        let progress = Progress {
            first_failure: Some("retained first failure".into()),
            ..Progress::default()
        };
        p.persist(&progress, false).unwrap();
        release_fixture(&mut p);
        drop(p);
        let mut options = f.options();
        options.recover = Some(run.clone());
        options.cargo_home = f.0.join("absent-cargo-home");
        fs::write(
            f.dir("lane/.cargo").join("config.toml"),
            "bad external config",
        )
        .unwrap();
        let mut recovered = NativePackage::open_at(options, &locks).unwrap();
        assert!(recovered.environment.is_empty());
        assert!(recovered.job.is_none());
        assert_eq!(
            recovered.prior.as_ref().unwrap().first_failure.as_deref(),
            Some("retained first failure")
        );
        assert_ne!(recovered.lease.as_ref().unwrap().identity(), original_lease);
        let journal = record::read(&recovered.run_dir.join("journal.json")).unwrap();
        assert_eq!(
            journal.get("lease_identity").unwrap().string().unwrap(),
            recovered.lease.as_ref().unwrap().identity()
        );
        assert_eq!(
            journal.get("lease_id").unwrap().string().unwrap(),
            recovered.lease.as_ref().unwrap().owner().lease
        );
        let events = journal
            .get("detached_before_acquire")
            .unwrap()
            .array()
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].get("phase").unwrap().string().unwrap(), "intent");
        assert_eq!(
            events[1].get("phase").unwrap().string().unwrap(),
            "detached"
        );
        assert_eq!(
            events[0].get("path").unwrap(),
            events[1].get("path").unwrap()
        );
        let mut forged = journal.clone();
        forged
            .set("job", Value::text("Local\\unrelated-job"))
            .unwrap();
        assert!(
            validate_journal(
                &forged,
                &recovered.options,
                &run,
                &recovered.scratch,
                &locks,
                None
            )
            .is_err()
        );
        let mut event = events[0].clone();
        let mut forged = journal;
        event
            .set(
                "path",
                Value::text(text(&f.0.join("outside.released.1")).unwrap()),
            )
            .unwrap();
        forged
            .set("detached_before_acquire", Value::Array(vec![event]))
            .unwrap();
        assert!(
            validate_journal(
                &forged,
                &recovered.options,
                &run,
                &recovered.scratch,
                &locks,
                None
            )
            .is_err()
        );
        release_fixture(&mut recovered);
    }
    #[test]
    fn busy_slots_leave_full_pre_acquisition_journal_without_workers() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let options = f.options();
        let owner = Owner {
            nick: "other".into(),
            task: "fixture".into(),
            worktree: options.checkout.clone(),
            started: utc_now(),
            kind: "package".into(),
            run: "other-run".into(),
            lease: "other-lease".into(),
            lane_root: options.lane_root.clone(),
        };
        let mut first = Lease::acquire(&locks.join("qemu"), owner.clone()).unwrap();
        let mut second = Lease::acquire(&locks.join("qemu2"), owner).unwrap();
        let error = NativePackage::open_at(options, &locks).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let dirs: Vec<_> = fs::read_dir(f.0.join("lane/packages"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(dirs.len(), 1);
        let journal = record::read(&dirs[0].join("journal.json")).unwrap();
        assert_eq!(journal.get("lease_identity").unwrap(), &Value::Null);
        assert_eq!(
            Path::new(journal.get("lease_slot").unwrap().string().unwrap()),
            locks.join("qemu2")
        );
        assert!(journal.get("workers").unwrap().array().unwrap().is_empty());
        assert_eq!(
            journal.get("job").unwrap().string().unwrap(),
            job_name(journal.get("run").unwrap().string().unwrap())
        );
        assert!(
            parse_identity(journal.get("controller").unwrap())
                .unwrap()
                .pid
                > 0
        );
        first.publish_release(&utc_now()).unwrap();
        second.publish_release(&utc_now()).unwrap();
    }
    #[test]
    fn journal_cap_failure_prevents_acquisition_and_stream_caps_prevent_spawns() {
        let f = Fixture::new();
        let slot = f.0.join("qemu");
        let owner = Owner {
            nick: "fixture".into(),
            task: "package".into(),
            worktree: f.0.clone(),
            started: utc_now(),
            kind: "package".into(),
            run: "test-run".into(),
            lease: "test-lease".into(),
            lane_root: f.0.clone(),
        };
        let mut journal =
            Value::object([("oversized", Value::text("x".repeat(record::RECORD_LIMIT)))]);
        assert!(
            acquire_journaled(
                &f.0,
                &mut journal,
                &owner,
                std::slice::from_ref(&slot),
                false,
                None
            )
            .is_err()
        );
        assert!(!slot.exists());
        command_budget(TOTAL_STREAMS - 2 * windows_job::STREAM_LIMIT, 127).unwrap();
        assert!(command_budget(TOTAL_STREAMS - 2 * windows_job::STREAM_LIMIT + 1, 127).is_err());
        assert!(command_budget(0, 128).is_err());
    }
    #[test]
    fn clean_source_inputs_and_inventory_detect_changes() {
        let id = "1".repeat(40);
        let good = format!("100644 blob {id}\tCargo.toml\0");
        assert_eq!(parse_source_paths(good.as_bytes()).unwrap(), ["Cargo.toml"]);
        for bad in [
            format!("120000 blob {id}\tlink\0"),
            format!("160000 commit {id}\tsub\0"),
            format!("100644 blob {id}\t../escape\0"),
            format!("100644 blob {id}\tbuild/output\0"),
            format!("{good}{good}"),
            good.trim_end_matches('\0').to_owned(),
        ] {
            assert!(parse_source_paths(bad.as_bytes()).is_err());
        }
        let f = Fixture::new();
        require_checkout_root(&f.0, &f.0).unwrap();
        assert!(require_checkout_root(&f.dir("nested"), &f.0).is_err());
        fs::write(f.0.join("Cargo.toml"), b"first").unwrap();
        let paths = vec!["Cargo.toml".to_owned()];
        let before = source_inventory(&f.0, &paths, end()).unwrap();
        fs::write(f.0.join("Cargo.toml"), b"later").unwrap();
        assert_ne!(before, source_inventory(&f.0, &paths, end()).unwrap());
        assert!(source_inventory(&f.0, &paths, Instant::now()).is_err());
        tokenless_config(b"cmdline: mirror.ui=desktop").unwrap();
        assert!(tokenless_config(b"cmdline: mirror.perf=boot").is_err());
        assert!(tokenless_config(b"cmdline: mirror.test=fault").is_err());
    }
    #[test]
    fn verified_copy_is_atomic_and_rejects_mutation_or_expiry() {
        let f = Fixture::new();
        let source = f.0.join("source.img");
        let dest = f.0.join("published.img");
        let bytes = vec![43; 140_000];
        fs::write(&source, &bytes).unwrap();
        fs::write(&dest, b"old").unwrap();
        assert!(copy_verified(&source, &dest, Instant::now()).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"old");
        assert!(
            copy_verified_with(&source, &dest, end(), || fs::write(&source, b"changed")).is_err()
        );
        assert_eq!(fs::read(&dest).unwrap(), b"old");
        assert_eq!(
            fs::read_dir(&f.0).unwrap().count(),
            2,
            "failed copy removes its sole temporary"
        );
        fs::write(&source, &bytes).unwrap();
        let artifact = copy_verified(&source, &dest, end()).unwrap();
        assert_eq!(
            artifact,
            Artifact {
                size: bytes.len() as u64,
                sha256: sha256::bytes(&bytes)
            }
        );
        assert_eq!(fs::read(&dest).unwrap(), bytes);
    }
    #[test]
    fn pinned_tool_lookup_bypasses_proxies_without_relaxing_artifact_paths() {
        let f = Fixture::new();
        let bin = f.dir("toolchains/1.99.0-x86_64-pc-windows-msvc/bin");
        fs::write(bin.join("rustc.exe"), b"fixture installed binary").unwrap();
        assert_eq!(
            pinned_executable(&f.0, "rustc.exe").unwrap(),
            bin.join("rustc.exe")
        );
        assert!(pinned_executable(&f.0, "cargo.exe").is_err());
        assert!(pinned_executable(&f.0, "../other.exe").is_err());
        let actual = executable("rustc.exe").unwrap();
        assert!(actual.ends_with("toolchains/1.99.0-x86_64-pc-windows-msvc/bin/rustc.exe"));
    }
    #[test]
    fn named_replacement_recovers_exact_run_preserving_both_operators() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
        let run = p.run.clone();
        let slot = p.lease.as_ref().unwrap().slot().to_owned();
        // An actual exited, contained process supplies the controller identity
        // for this synthetic interruption; no build or shared slot is involved.
        let cwd = p.options.checkout.clone();
        let out = p
            .command("rustc.exe", vec!["--version".into()], &cwd, end())
            .unwrap();
        assert_eq!(out.exit_code, 0);
        p.journal.set("controller", identity(out.identity)).unwrap();
        let cause = Progress {
            first_failure: Some("synthetic interrupted package".into()),
            ..Progress::default()
        };
        p.persist(&cause, false).unwrap();
        drop(p); // deliberately retained unreleased fixture generation
        let options = |operator: &str| {
            let mut o = f.options();
            o.recover = Some(run.clone());
            o.operator = operator.into();
            o
        };
        let cap = Replacement {
            run: run.clone(),
            lane_root: f.0.join("lane"),
            original_operator: "fixture/package".into(),
            replacement_operator: "replacement/package".into(),
            scope: "package-recover".into(),
            authorization_reference: "fixture-verified-Cameron-instruction".into(),
        };
        assert!(NativePackage::open_at(options("impostor/package"), &locks).is_err());
        assert!(NativePackage::open_at(options("replacement/package"), &locks).is_err());
        for field in ["run", "root", "original", "replacement", "scope"] {
            let mut bad = cap.clone();
            match field {
                "run" => bad.run = "other-run".into(),
                "root" => bad.lane_root = f.dir("other-root"),
                "original" => bad.original_operator = "wrong/package".into(),
                "replacement" => bad.replacement_operator = "wrong/package".into(),
                _ => bad.scope = "vm-recover".into(),
            }
            assert!(
                NativePackage::open_authorized_at(
                    options("replacement/package"),
                    &locks,
                    Some(&bad)
                )
                .is_err(),
                "{field}"
            );
        }
        let recovered =
            NativePackage::open_authorized_at(options("replacement/package"), &locks, Some(&cap))
                .unwrap();
        let journal = record::read(&recovered.run_dir.join("journal.json")).unwrap();
        assert_eq!(
            journal.get("operator").unwrap().string().unwrap(),
            "fixture/package"
        );
        assert_eq!(
            journal
                .get("replacement")
                .unwrap()
                .get("replacement_operator")
                .unwrap()
                .string()
                .unwrap(),
            "replacement/package"
        );
        drop(recovered); // simulate interruption after authorization and owner append
        assert!(NativePackage::open_at(options("fixture/package"), &locks).is_err());
        let mut changed = cap.clone();
        changed.authorization_reference = "different authorization".into();
        assert!(
            NativePackage::open_authorized_at(
                options("replacement/package"),
                &locks,
                Some(&changed)
            )
            .is_err()
        );
        let recovered = NativePackage::open_at(options("replacement/package"), &locks).unwrap();
        let owner = fs::read_to_string(slot.join("owner.txt")).unwrap();
        assert!(owner.contains("nick=fixture\n"));
        assert!(owner.contains("replacement_operator=replacement/package\n"));
        assert_eq!(
            owner.matches("replacement_authorization=").count(),
            1,
            "replay must not duplicate owner history"
        );
        let run_dir = recovered.run_dir.clone();
        let result = recovered.execute().unwrap();
        assert_eq!(result.verdict, Verdict::RecoveredInvalid);
        assert_eq!(
            result.first_failure.as_deref(),
            Some("synthetic interrupted package")
        );
        assert!(
            !slot.exists(),
            "only the owned released fixture generation was detached"
        );
        let journal = record::read(&run_dir.join("journal.json")).unwrap();
        let events = journal.get("release_detach").unwrap().array().unwrap();
        assert_eq!(events.len(), 2);
        let detached = PathBuf::from(events[1].get("path").unwrap().string().unwrap());
        assert!(!detached.exists());
        assert_eq!(
            result
                .lock_release
                .unwrap()
                .get("state")
                .unwrap()
                .string()
                .unwrap(),
            "removed"
        );
    }
    #[test]
    fn malformed_manual_slot_is_held_and_other_slot_is_acquired() {
        for record in [
            "owner: claude\n",
            "\u{feff}nick=claude\n",
            "released=2026-10-13T13:00:00Z\n",
        ] {
            let f = Fixture::new();
            let locks = f.dir("locks");
            let slot = f.dir("locks/qemu");
            fs::write(slot.join("owner.txt"), record).unwrap();
            let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
            assert_eq!(p.lease.as_ref().unwrap().slot(), locks.join("qemu2"));
            assert_eq!(fs::read_to_string(slot.join("owner.txt")).unwrap(), record);
            assert!(
                p.journal
                    .get("detached_before_acquire")
                    .unwrap()
                    .array()
                    .unwrap()
                    .is_empty()
            );
            release_fixture(&mut p);
        }
    }
    #[test]
    fn normal_empty_job_proof_does_not_reopen_old_worker_pids() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
        let mut reused = windows_job::current_process_identity().unwrap();
        reused.start_time += 1;
        p.journal
            .set("workers", Value::Array(vec![identity(reused)]))
            .unwrap();
        p.prove_no_live_build(false, end()).unwrap();
        p.journal.set("workers", Value::Array(vec![])).unwrap();
        release_fixture(&mut p);
    }
    #[test]
    fn explicit_slot_never_detaches_another_released_generation() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let options = f.options();
        let owner = Owner {
            nick: "fixture".into(),
            task: "busy".into(),
            worktree: options.checkout.clone(),
            started: utc_now(),
            kind: "package".into(),
            run: "other-run".into(),
            lease: "other-lease".into(),
            lane_root: options.lane_root.clone(),
        };
        let mut first = Lease::acquire(&locks.join("qemu"), owner.clone()).unwrap();
        let mut second = Lease::acquire(&locks.join("qemu2"), owner).unwrap();
        second.publish_release(&utc_now()).unwrap();
        drop(second);
        let old_owner = fs::read(locks.join("qemu2/owner.txt")).unwrap();
        let mut options = f.options();
        options.slot = Some("qemu".into());
        assert_eq!(
            NativePackage::open_at(options, &locks)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(fs::read(locks.join("qemu2/owner.txt")).unwrap(), old_owner);
        assert_eq!(fs::read_dir(&locks).unwrap().count(), 2);
        first.publish_release(&utc_now()).unwrap();
        drop(first);
        let mut options = f.options();
        options.slot = Some("../qemu".into());
        assert!(NativePackage::open_at(options, &locks).is_err());
        let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
        let run = p.run.clone();
        release_fixture(&mut p);
        drop(p);
        let mut recovery = f.options();
        recovery.recover = Some(run);
        recovery.slot = Some("qemu2".into());
        assert!(NativePackage::open_at(recovery, &locks).is_err());
        assert_eq!(fs::read(locks.join("qemu2/owner.txt")).unwrap(), old_owner);
    }
    #[test]
    fn optional_detach_failure_retains_durable_owner_proof() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
        let slot = p.lease.as_ref().unwrap().slot().to_owned();
        let proof = p.publish_release().unwrap();
        let progress = Progress {
            phase: "finalized".into(),
            verdict: Verdict::Failed,
            first_failure: Some("fixture failure".into()),
            no_live_build: true,
            release_intent: true,
            lock_release: Some(proof.clone()),
            ..Progress::default()
        };
        p.persist(&progress, true).unwrap();
        p.journal
            .set(
                "oversized-diagnostic",
                Value::text("x".repeat(record::RECORD_LIMIT)),
            )
            .unwrap();
        assert!(p.close_released().unwrap().is_none());
        assert!(slot.exists());
        assert!(
            fs::read_to_string(slot.join("owner.txt"))
                .unwrap()
                .contains("released=")
        );
        let durable =
            Progress::parse(&record::read(&p.run_dir.join("final.json")).unwrap()).unwrap();
        assert_eq!(durable.lock_release, Some(proof));
    }
    #[test]
    fn installed_pinned_compiler_and_cargo_version_probes_use_contained_workers() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
        let cwd = p.options.checkout.clone();
        let rust = p
            .command("rustc.exe", vec!["--version".into()], &cwd, end())
            .unwrap();
        assert_eq!(rust.exit_code, 0);
        assert!(
            std::str::from_utf8(&rust.stdout)
                .unwrap()
                .starts_with("rustc 1.99.0 ")
        );
        let cargo = p
            .command("cargo.exe", vec!["--version".into()], &cwd, end())
            .unwrap();
        assert_eq!(cargo.exit_code, 0);
        assert!(
            std::str::from_utf8(&cargo.stdout)
                .unwrap()
                .starts_with("cargo 1.99.0 ")
        );
        p.lease
            .as_mut()
            .unwrap()
            .publish_release(&utc_now())
            .unwrap();
        p.lease.take();
    }
    #[test]
    fn external_tool_paths_are_dos_only_and_keep_canonical_identity() {
        let f = Fixture::new();
        let canonical = f.dir("space directory");
        let normal = tool_path(&canonical).unwrap();
        assert!(!normal.as_os_str().to_string_lossy().starts_with(r"\\?\"));
        assert_eq!(paths::canonical_no_reparse(&normal).unwrap(), canonical);
        assert!(tool_path(Path::new(r"C:relative")).is_err());
        assert!(tool_path(Path::new(r"\\server\share\file")).is_err());
        assert!(tool_path(Path::new(r"\\?\UNC\server\share\file")).is_err());
        let path_list = std::env::join_paths([canonical.clone(), f.0.clone()]).unwrap();
        let env = tool_environment(&[
            ("CARGO_HOME".into(), canonical.as_os_str().into()),
            ("PATH".into(), path_list),
        ])
        .unwrap();
        assert_eq!(env[0].1, normal.as_os_str());
        assert!(
            std::env::split_paths(&env[1].1)
                .all(|p| !p.as_os_str().to_string_lossy().starts_with(r"\\?\"))
        );
    }
    #[test]
    fn contained_git_archive_and_powershell_expand_accept_canonical_paths() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let mut options = f.options();
        options.checkout = f.dir("synthetic repository");
        options.lane_root = f.dir("lane records");
        let mut p = NativePackage::open_at(options, &locks).unwrap();
        let cwd = p.options.checkout.clone();
        p.checked(
            "git.exe",
            vec!["init".into(), "-b".into(), "fixture".into()],
            &cwd,
            end(),
        )
        .unwrap();
        let nested = cwd.join("directory with spaces");
        fs::create_dir(&nested).unwrap();
        let content = b"archive fixture content\n";
        fs::write(nested.join("file.txt"), content).unwrap();
        p.checked(
            "git.exe",
            vec![
                "add".into(),
                "--".into(),
                "directory with spaces/file.txt".into(),
            ],
            &cwd,
            end(),
        )
        .unwrap();
        p.checked(
            "git.exe",
            vec![
                "-c".into(),
                "user.name=Lane Fixture".into(),
                "-c".into(),
                "user.email=fixture@example.invalid".into(),
                "-c".into(),
                "commit.gpgsign=false".into(),
                "commit".into(),
                "-m".into(),
                "archive fixture".into(),
            ],
            &cwd,
            end(),
        )
        .unwrap();
        p.commit = p.git(&["rev-parse", "HEAD"], end()).unwrap();
        let archive = p.archive_source(end()).unwrap();
        assert!(archive.is_file());
        assert_eq!(
            fs::read(p.scratch.join("directory with spaces/file.txt")).unwrap(),
            content
        );
        assert_eq!(paths::canonical_no_reparse(&p.scratch).unwrap(), p.scratch);
        for command in p.journal.get("commands").unwrap().array().unwrap() {
            assert!(
                !command
                    .get("program")
                    .unwrap()
                    .string()
                    .unwrap()
                    .starts_with(r"\\?\")
            );
            for arg in command.get("args").unwrap().array().unwrap() {
                assert!(!arg.string().unwrap().contains(r"\\?\"));
            }
        }
        p.prove_no_live_build(false, end()).unwrap();
        p.lease
            .as_mut()
            .unwrap()
            .publish_release(&utc_now())
            .unwrap();
        p.lease.take();
    }
    #[test]
    fn real_git_clean_gate_accepts_crlf_text_and_rejects_content_or_raw_byte_changes() {
        let f = Fixture::new();
        let locks = f.dir("locks");
        let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
        p.git(&["init", "--quiet", "-b", "fixture"], end()).unwrap();
        let cwd = p.options.checkout.clone();
        fs::write(cwd.join(".gitattributes"), b"*.raw -text\n").unwrap();
        fs::write(cwd.join("source.txt"), b"first line\nsecond line\n").unwrap();
        fs::write(cwd.join("bytes.raw"), b"unchanged bytes\n").unwrap();
        p.git(&["-c", "core.autocrlf=false", "add", "--", "."], end())
            .unwrap();
        p.git(
            &[
                "-c",
                "user.name=Lane Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "LF source fixture",
            ],
            end(),
        )
        .unwrap();
        fs::write(cwd.join("source.txt"), b"first line\r\nsecond line\r\n").unwrap();
        // This is the pre-fix failure: status with sanitized configuration
        // treats a normal Windows CRLF checkout as a content modification.
        assert!(
            !p.git(&["status", "--porcelain=v1"], end())
                .unwrap()
                .is_empty()
        );
        p.require_clean_checkout(end()).unwrap();
        assert_eq!(
            fs::read(cwd.join("source.txt")).unwrap(),
            b"first line\r\nsecond line\r\n"
        );
        fs::write(cwd.join("source.txt"), b"edited line\r\nsecond line\r\n").unwrap();
        assert!(p.require_clean_checkout(end()).is_err());
        fs::write(cwd.join("source.txt"), b"first line\r\nsecond line\r\n").unwrap();
        p.require_clean_checkout(end()).unwrap();
        // Normalization is only for the worktree. A CRLF blob newly staged in
        // place of the committed LF blob must remain a staged modification.
        p.git(
            &["-c", "core.autocrlf=false", "add", "--", "source.txt"],
            end(),
        )
        .unwrap();
        fs::write(cwd.join("source.txt"), b"first line\nsecond line\n").unwrap();
        assert!(p.require_clean_checkout(end()).is_err());
        p.git(
            &["-c", "core.autocrlf=false", "add", "--", "source.txt"],
            end(),
        )
        .unwrap();
        fs::write(cwd.join("source.txt"), b"first line\r\nsecond line\r\n").unwrap();
        p.require_clean_checkout(end()).unwrap();
        fs::write(cwd.join("bytes.raw"), b"unchanged bytes\r\n").unwrap();
        assert!(p.require_clean_checkout(end()).is_err());
        fs::write(cwd.join("bytes.raw"), b"unchanged bytes\n").unwrap();
        p.require_clean_checkout(end()).unwrap();
        fs::write(cwd.join("untracked.txt"), b"uncommitted\r\n").unwrap();
        assert!(p.require_clean_checkout(end()).is_err());
        assert!(
            p.environment
                .contains(&("GIT_CONFIG_NOSYSTEM".into(), "1".into()))
        );
        assert!(
            p.environment
                .contains(&("GIT_CONFIG_GLOBAL".into(), "NUL".into()))
        );
        p.prove_no_live_build(false, end()).unwrap();
        p.lease
            .as_mut()
            .unwrap()
            .publish_release(&utc_now())
            .unwrap();
        p.lease.take();
    }
    #[test]
    fn real_git_capability_gate_accepts_ancestor_and_rejects_absent_or_unrelated() {
        fn commit(
            p: &mut NativePackage,
            tree: &str,
            parent: Option<&str>,
            message: &str,
        ) -> String {
            let mut args = vec![
                "-c",
                "user.name=Lane Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                tree,
                "-m",
                message,
            ];
            if let Some(parent) = parent {
                args.extend(["-p", parent]);
            }
            p.git(&args, end()).unwrap()
        }
        let f = Fixture::new();
        let locks = f.dir("locks");
        let mut p = NativePackage::open_at(f.options(), &locks).unwrap();
        p.git(&["init", "--quiet", "-b", "fixture"], end()).unwrap();
        let tree = p.git(&["write-tree"], end()).unwrap();
        let ancestor = commit(&mut p, &tree, None, "required capability");
        let descendant = commit(&mut p, &tree, Some(&ancestor), "source package");
        let unrelated = commit(&mut p, &tree, None, "unrelated capability");
        p.git(&["update-ref", "HEAD", &descendant], end()).unwrap();
        p.commit = p.git(&["rev-parse", "HEAD^{commit}"], end()).unwrap();
        assert_eq!(
            p.require_capability_ancestor(&ancestor, end()).unwrap(),
            ancestor
        );
        let before = p.command_number;
        assert!(
            p.require_capability_ancestor(&"0".repeat(40), end())
                .is_err()
        );
        assert_eq!(
            p.command_number,
            before + 1,
            "absent object must fail resolution before ancestry"
        );
        let before = p.command_number;
        assert!(p.require_capability_ancestor(&unrelated, end()).is_err());
        assert_eq!(
            p.command_number,
            before + 2,
            "existing unrelated object must reach merge-base rejection"
        );
        assert_eq!(p.commit, descendant);
        p.prove_no_live_build(false, end()).unwrap();
        p.lease
            .as_mut()
            .unwrap()
            .publish_release(&utc_now())
            .unwrap();
        p.lease.take();
    }
}
