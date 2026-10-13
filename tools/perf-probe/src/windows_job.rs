//! Windows command containment. The controller owns the only job handle; workers
//! enter that job at process creation and remain suspended until their identity
//! has been durably recorded by the caller.
//!
//! References: Microsoft `UpdateProcThreadAttribute` (JOB_LIST / HANDLE_LIST),
//! `CreateProcessW`, `Job Objects`, and `QueryInformationJobObject`.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const STREAM_LIMIT: usize = 1024 * 1024;
const TERMINATED: u32 = 0xe000_0001;
const POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Windows FILETIME: 100 ns intervals since 1601-01-01 UTC.
    pub start_time: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessState {
    /// The recorded process exited, including when its PID now identifies a
    /// different process with another creation time.
    Exited,
    Running,
}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// Complete environment, never an overlay on the controller's environment.
    pub env: Vec<(OsString, OsString)>,
}

#[derive(Clone, Debug)]
pub struct CapturePaths {
    /// Both files must be new. Each retains at most STREAM_LIMIT bytes.
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

#[derive(Debug)]
pub struct CommandOutput {
    pub identity: ProcessIdentity,
    pub exit_code: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct Job {
    handle: OwnedHandle,
    name: String,
}

#[must_use = "wait for the worker through its owning job"]
pub struct Worker {
    process: OwnedHandle,
    identity: ProcessIdentity,
    stdout: Option<JoinHandle<io::Result<Vec<u8>>>>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    stream_failure: Arc<AtomicBool>,
}

impl Worker {
    pub fn identity(&self) -> ProcessIdentity {
        self.identity
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Dropping an unwaited direct worker must not leave it executing. Job
        // ownership still governs descendants; dropping Job closes that tree.
        // SAFETY: process is an owned process handle with termination rights.
        unsafe { TerminateProcess(self.process.as_raw_handle(), TERMINATED) };
    }
}

impl Job {
    pub fn new(name: &str) -> io::Result<Self> {
        validate_job_name(name)?;
        let wide = wide(OsStr::new(name))?;
        // SAFETY: wide is NUL terminated; NULL security attributes make the
        // returned controller handle non-inheritable.
        let raw = unsafe { CreateJobObjectW(null(), wide.as_ptr()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let last_error = io::Error::last_os_error();
        // SAFETY: CreateJobObjectW returned a unique owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        if last_error.raw_os_error() == Some(183) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "job identity already exists",
            ));
        }
        // SAFETY: all-zero values are valid for this plain Windows C record.
        let mut limits: JobExtendedLimits = unsafe { zeroed() };
        limits.basic.limit_flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
        // Neither BREAKAWAY_OK nor SILENT_BREAKAWAY_OK is allowed.
        // SAFETY: handle is a job; limits matches class 9 and lives for the call.
        check(unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                9,
                (&limits as *const JobExtendedLimits).cast(),
                size_of::<JobExtendedLimits>() as u32,
            )
        })?;
        Ok(Self {
            handle,
            name: name.to_owned(),
        })
    }

    pub fn identity(&self) -> &str {
        &self.name
    }

    pub fn spawn(
        &self,
        spec: &CommandSpec,
        before_resume: impl FnOnce(ProcessIdentity) -> io::Result<()>,
    ) -> io::Result<Worker> {
        self.spawn_inner(spec, None, before_resume)
    }

    pub fn spawn_captured(
        &self,
        spec: &CommandSpec,
        capture: &CapturePaths,
        before_resume: impl FnOnce(ProcessIdentity) -> io::Result<()>,
    ) -> io::Result<Worker> {
        let stdout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&capture.stdout)?;
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&capture.stderr)?;
        self.spawn_inner(spec, Some((stdout, stderr)), before_resume)
    }

    fn spawn_inner(
        &self,
        spec: &CommandSpec,
        capture: Option<(File, File)>,
        before_resume: impl FnOnce(ProcessIdentity) -> io::Result<()>,
    ) -> io::Result<Worker> {
        if !spec.program.is_absolute() || !spec.cwd.is_absolute() {
            return Err(invalid("command program and cwd must be absolute"));
        }
        let program = wide(spec.program.as_os_str())?;
        let cwd = wide(spec.cwd.as_os_str())?;
        let mut command = command_line(&spec.program, &spec.args)?;
        let mut env = environment(&spec.env)?;
        let (stdout_read, stdout_write) = pipe()?;
        let (stderr_read, stderr_write) = pipe()?;
        let (stdin_read, stdin_write) = pipe()?;
        // A closed stdin gives deterministic EOF without inheriting a console.
        drop(stdin_write);
        let mut inherited = [
            stdin_read.as_raw_handle(),
            stdout_write.as_raw_handle(),
            stderr_write.as_raw_handle(),
        ];
        let mut jobs = [self.handle.as_raw_handle()];
        let attributes = Attributes::new(2)?;
        // SAFETY: the arrays remain live through CreateProcess and attribute
        // destruction. Only the three pipe endpoints may be inherited.
        check(unsafe {
            UpdateProcThreadAttribute(
                attributes.ptr(),
                0,
                0x0002_0002,
                inherited.as_mut_ptr().cast(),
                size_of_val(&inherited),
                null_mut(),
                null_mut(),
            )
        })?;
        // SAFETY: jobs contains this job's live, non-inheritable handle. JOB_LIST
        // assigns membership as part of process creation, before execution.
        check(unsafe {
            UpdateProcThreadAttribute(
                attributes.ptr(),
                0,
                0x0002_000d,
                jobs.as_mut_ptr().cast(),
                size_of_val(&jobs),
                null_mut(),
                null_mut(),
            )
        })?;
        // SAFETY: zero is valid initialization for the documented C structures.
        let mut startup: StartupInfoEx = unsafe { zeroed() };
        startup.startup.cb = size_of::<StartupInfoEx>() as u32;
        startup.startup.flags = 0x100; // STARTF_USESTDHANDLES.
        startup.startup.stdin = inherited[0];
        startup.startup.stdout = inherited[1];
        startup.startup.stderr = inherited[2];
        startup.attributes = attributes.ptr();
        // SAFETY: zero is valid initialization for output-only PROCESS_INFORMATION.
        let mut information: ProcessInformation = unsafe { zeroed() };
        // SAFETY: all pointers reference live, correctly sized, UTF-16 Windows
        // structures. Explicit environment is double NUL terminated. The handle
        // list excludes the job handle, and CREATE_SUSPENDED prevents user code.
        check(unsafe {
            CreateProcessW(
                program.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                1,
                0x0008_0000 | 0x0000_0004 | 0x0000_0400 | 0x0800_0000,
                env.as_mut_ptr().cast(),
                cwd.as_ptr(),
                &mut startup,
                &mut information,
            )
        })?;
        // SAFETY: a successful CreateProcessW transfers these distinct handles.
        let process = unsafe { OwnedHandle::from_raw_handle(information.process) };
        // SAFETY: successful CreateProcessW also returned the initial thread.
        let thread = unsafe { OwnedHandle::from_raw_handle(information.thread) };
        let suspended = Suspended {
            process: Some(process),
        };
        drop(attributes);
        drop(stdin_read);
        drop(stdout_write);
        drop(stderr_write);
        let identity = process_identity(suspended.process.as_ref().unwrap(), information.pid)?;
        let mut member = 0;
        // SAFETY: the handles are live process/job handles and member is writable.
        check(unsafe {
            IsProcessInJob(suspended.raw(), self.handle.as_raw_handle(), &mut member)
        })?;
        if member == 0 {
            return Err(io::Error::other("worker did not enter the command job"));
        }
        let stream_failure = Arc::new(AtomicBool::new(false));
        let (stdout_file, stderr_file) = match capture {
            Some((out, err)) => (Some(out), Some(err)),
            None => (None, None),
        };
        let stdout = reader(stdout_read, stdout_file, stream_failure.clone())?;
        let stderr = reader(stderr_read, stderr_file, stream_failure.clone())?;
        // Caller must flush the durable journal here. An error never resumes the
        // child: Suspended's destructor terminates it inside the job.
        before_resume(identity)?;
        // SAFETY: thread is the still-suspended initial thread. No other code can
        // resume it because its handle was never inherited or exposed.
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(Worker {
            process: suspended.take(),
            identity,
            stdout: Some(stdout),
            stderr: Some(stderr),
            stream_failure,
        })
    }

    /// Wait under an absolute monotonic deadline. Stream overflow/write failure
    /// and timeout terminate the entire job, including descendants.
    pub fn wait(&self, mut worker: Worker, deadline: Instant) -> io::Result<CommandOutput> {
        let mut failure = None;
        loop {
            if worker.stream_failure.load(Ordering::Acquire) {
                failure = Some(io::Error::other(
                    "command output overflow or capture write failure",
                ));
                break;
            }
            let exited = process_exited(&worker.process)?;
            let drained = worker.stdout.as_ref().unwrap().is_finished()
                && worker.stderr.as_ref().unwrap().is_finished();
            if exited && drained {
                break;
            }
            if Instant::now() >= deadline {
                failure = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "command deadline expired",
                ));
                break;
            }
            thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
        if let Some(failure) = failure {
            let cleanup = self.terminate();
            let code = exit_code(&worker.process);
            // Reader threads have bounded retained buffers/files. They will
            // finish when the killed tree closes pipes; do not join past deadline.
            // Preserve the initial command failure even if cleanup/query fail.
            return Err(io::Error::new(
                failure.kind(),
                format!("{failure}; observed exit_code={code:?}; job termination={cleanup:?}"),
            ));
        }
        let code = exit_code(&worker.process)?;
        let stdout = join_reader(worker.stdout.take().unwrap())?;
        let stderr = join_reader(worker.stderr.take().unwrap())?;
        Ok(CommandOutput {
            identity: worker.identity,
            exit_code: code,
            stdout,
            stderr,
        })
    }

    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: self owns a job handle with termination rights.
        check(unsafe { TerminateJobObject(self.handle.as_raw_handle(), TERMINATED) })
    }

    pub fn active_processes(&self) -> io::Result<u32> {
        job_active(&self.handle)
    }

    pub fn wait_empty(&self, deadline: Instant) -> io::Result<()> {
        loop {
            if self.active_processes()? == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "job still has live descendants",
                ));
            }
            thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
}

/// Current controller PID and immutable creation timestamp for its journal.
pub fn current_process_identity() -> io::Result<ProcessIdentity> {
    let pid = std::process::id();
    let handle = open_process(pid)?.ok_or_else(|| io::Error::other("current process missing"))?;
    process_identity(&handle, pid)
}

pub fn process_state(identity: ProcessIdentity) -> io::Result<ProcessState> {
    if identity.pid == 0 || identity.start_time == 0 {
        return Err(invalid("missing process identity"));
    }
    let Some(handle) = open_process(identity.pid)? else {
        return Ok(ProcessState::Exited);
    };
    if process_identity(&handle, identity.pid)?.start_time != identity.start_time {
        // The open handle identifies a different process. The original cannot
        // still be alive while Windows has assigned its PID to this process.
        return Ok(ProcessState::Exited);
    }
    Ok(if process_exited(&handle)? {
        ProcessState::Exited
    } else {
        ProcessState::Running
    })
}

/// Prove the original controller exited and its no-breakaway job is empty or
/// absent. Matching live identities still block. An inaccessible worker PID may
/// now belong to an unrelated protected process, so access denial defers to the
/// job proof; an inaccessible controller still blocks because it is outside the
/// job. The caller must verify the durable run/job/worker binding. A named job
/// disappears only after its last handle and all processes are gone.
pub fn prove_recovery(
    controller: ProcessIdentity,
    workers: &[ProcessIdentity],
    job_name: &str,
) -> io::Result<()> {
    prove_recovery_with(
        controller,
        workers,
        job_name,
        process_state,
        prove_job_empty,
    )
}

fn prove_recovery_with(
    controller: ProcessIdentity,
    workers: &[ProcessIdentity],
    job_name: &str,
    mut query_process: impl FnMut(ProcessIdentity) -> io::Result<ProcessState>,
    prove_job: impl FnOnce(&str) -> io::Result<()>,
) -> io::Result<()> {
    validate_job_name(job_name)?;
    if workers.len() > 4096 {
        return Err(invalid("too many worker identities"));
    }
    // Job emptiness cannot prove controller death: it owns the job handle but
    // is not a member. Do not apply the worker access-denied fallback here.
    if query_process(controller)? == ProcessState::Running {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "recorded controller is still running",
        ));
    }
    for identity in workers {
        match query_process(*identity) {
            Ok(ProcessState::Exited) => (),
            Ok(ProcessState::Running) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "recorded worker is still running",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => (),
            Err(error) => return Err(error),
        }
    }
    // Mandatory even if every recorded PID has exited: descendants may not
    // have an individual journal entry. Workers enter atomically and cannot
    // break away, so this also resolves an inaccessible historical worker PID.
    prove_job(job_name)
}

fn prove_job_empty(job_name: &str) -> io::Result<()> {
    let name = wide(OsStr::new(job_name))?;
    // SAFETY: name is NUL terminated; QUERY is sufficient and no handle is inherited.
    let raw = unsafe { OpenJobObjectW(0x0004, 0, name.as_ptr()) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(2) {
            Ok(())
        } else {
            Err(error)
        };
    }
    // SAFETY: OpenJobObjectW returned a new owned handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    if job_active(&handle)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "old job has live descendants",
        ));
    }
    Ok(())
}

fn validate_job_name(name: &str) -> io::Result<()> {
    if !name.starts_with("Local\\MirrorLane-")
        || name.len() > 200
        || !name[17..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || name.len() == 17
    {
        return Err(invalid(
            "job name must be a unique Local\\MirrorLane- identity",
        ));
    }
    Ok(())
}

fn reader(
    pipe: OwnedHandle,
    mut output: Option<File>,
    failed: Arc<AtomicBool>,
) -> io::Result<JoinHandle<io::Result<Vec<u8>>>> {
    thread::Builder::new()
        .name("lane-command-output".into())
        .spawn(move || {
            let result = (|| {
                let mut file = File::from(pipe);
                let mut retained = Vec::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    let keep = count.min(STREAM_LIMIT - retained.len());
                    retained.extend_from_slice(&buffer[..keep]);
                    if let Some(output) = output.as_mut() {
                        output.write_all(&buffer[..keep])?;
                    }
                    if keep != count {
                        if let Some(output) = output.as_mut() {
                            output.sync_all()?;
                        }
                        return Err(io::Error::other("command stream exceeded 1 MiB"));
                    }
                }
                if let Some(output) = output.as_mut() {
                    output.sync_all()?;
                }
                Ok(retained)
            })();
            if result.is_err() {
                failed.store(true, Ordering::Release);
            }
            result
        })
}

fn join_reader(reader: JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other("command output reader panicked"))?
}

fn open_process(pid: u32) -> io::Result<Option<OwnedHandle>> {
    // SAFETY: PID is a value; requesting query/synchronize cannot mutate a process.
    let raw = unsafe { OpenProcess(0x1000 | 0x0010_0000, 0, pid) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: OpenProcess returned a new owned process handle.
    Ok(Some(unsafe { OwnedHandle::from_raw_handle(raw) }))
}

fn process_identity(handle: &OwnedHandle, pid: u32) -> io::Result<ProcessIdentity> {
    let mut creation = FileTime { low: 0, high: 0 };
    let mut exit = FileTime { low: 0, high: 0 };
    let mut kernel = FileTime { low: 0, high: 0 };
    let mut user = FileTime { low: 0, high: 0 };
    // SAFETY: handle is a process with query rights; four records are writable.
    check(unsafe {
        GetProcessTimes(
            handle.as_raw_handle(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    })?;
    Ok(ProcessIdentity {
        pid,
        start_time: u64::from(creation.low) | (u64::from(creation.high) << 32),
    })
}

fn process_exited(handle: &OwnedHandle) -> io::Result<bool> {
    // SAFETY: handle has SYNCHRONIZE access; zero timeout cannot block.
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
        0 => Ok(true),
        258 => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

fn exit_code(handle: &OwnedHandle) -> io::Result<u32> {
    let mut code = 0;
    // SAFETY: handle is a live process handle and code points to writable DWORD.
    check(unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) })?;
    Ok(code)
}

fn job_active(handle: &OwnedHandle) -> io::Result<u32> {
    // SAFETY: all-zero is valid for this output C record.
    let mut accounting: JobAccounting = unsafe { zeroed() };
    // SAFETY: class 1 expects exactly this correctly sized output structure.
    check(unsafe {
        QueryInformationJobObject(
            handle.as_raw_handle(),
            1,
            (&mut accounting as *mut JobAccounting).cast(),
            size_of::<JobAccounting>() as u32,
            null_mut(),
        )
    })?;
    Ok(accounting.active)
}

struct Suspended {
    process: Option<OwnedHandle>,
}
impl Suspended {
    fn raw(&self) -> *mut c_void {
        self.process.as_ref().unwrap().as_raw_handle()
    }
    fn take(mut self) -> OwnedHandle {
        self.process.take().unwrap()
    }
}
impl Drop for Suspended {
    fn drop(&mut self) {
        if let Some(process) = self.process.as_ref() {
            // SAFETY: this owned process was created suspended with terminate rights.
            unsafe { TerminateProcess(process.as_raw_handle(), TERMINATED) };
        }
    }
}

struct Attributes {
    storage: Vec<usize>,
}
impl Attributes {
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0;
        // SAFETY: first documented sizing call takes NULL storage and writable size.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
        if size == 0 || size > 65536 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: usize storage has pointer alignment and at least size bytes.
        check(unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), count, 0, &mut size)
        })?;
        Ok(Self { storage })
    }
    fn ptr(&self) -> *mut c_void {
        self.storage.as_ptr().cast_mut().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: storage contains a successfully initialized attribute list.
        unsafe { DeleteProcThreadAttributeList(self.ptr()) };
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let attributes = SecurityAttributes {
        length: size_of::<SecurityAttributes>() as u32,
        descriptor: null_mut(),
        inherit: 1,
    };
    let mut read = null_mut();
    let mut write = null_mut();
    // SAFETY: output pointers and sized security record are valid for this call.
    check(unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) })?;
    // SAFETY: CreatePipe returned two distinct owned handles.
    let read = unsafe { OwnedHandle::from_raw_handle(read) };
    // SAFETY: the second handle is the uniquely owned write endpoint.
    let write = unsafe { OwnedHandle::from_raw_handle(write) };
    // HANDLE_LIST, rather than process-global inheritability, is the definitive
    // inheritance allowlist; the read end is required for the child's stdin.
    Ok((read, write))
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut result: Vec<u16> = value.encode_wide().collect();
    if result.contains(&0) {
        return Err(invalid("NUL in Windows string"));
    }
    result.push(0);
    Ok(result)
}

fn command_line(program: &Path, args: &[OsString]) -> io::Result<Vec<u16>> {
    let mut out = Vec::new();
    for value in std::iter::once(program.as_os_str()).chain(args.iter().map(OsString::as_os_str)) {
        if !out.is_empty() {
            out.push(b' ' as u16);
        }
        out.push(b'"' as u16);
        let mut slashes = 0;
        for ch in value.encode_wide() {
            if ch == 0 {
                return Err(invalid("NUL in command argument"));
            }
            if ch == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            if ch == b'"' as u16 {
                out.extend(std::iter::repeat_n(b'\\' as u16, 2 * slashes + 1));
            } else {
                out.extend(std::iter::repeat_n(b'\\' as u16, slashes));
            }
            slashes = 0;
            out.push(ch);
        }
        out.extend(std::iter::repeat_n(b'\\' as u16, 2 * slashes));
        out.push(b'"' as u16);
    }
    out.push(0);
    if out.len() > 32767 {
        return Err(invalid("Windows command line exceeds 32767 UTF-16 units"));
    }
    Ok(out)
}

fn environment(values: &[(OsString, OsString)]) -> io::Result<Vec<u16>> {
    let mut entries = Vec::new();
    for (key, value) in values {
        let key = key
            .to_str()
            .ok_or_else(|| invalid("environment name must be ASCII"))?;
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic() && b != b'=') {
            return Err(invalid("invalid environment name"));
        }
        entries.push((key.to_ascii_uppercase(), key, value));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid("duplicate environment name"));
    }
    let mut output = Vec::new();
    for (_, key, value) in entries {
        output.extend(key.encode_utf16());
        output.push(b'=' as u16);
        output.extend(wide(value)?);
    }
    if output.is_empty() {
        output.push(0);
    }
    output.push(0);
    if output.len() > 32767 {
        return Err(invalid("environment block exceeds limit"));
    }
    Ok(output)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn check(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
#[repr(C)]
struct FileTime {
    low: u32,
    high: u32,
}
#[repr(C)]
struct ProcessInformation {
    process: *mut c_void,
    thread: *mut c_void,
    pid: u32,
    thread_id: u32,
}
#[repr(C)]
struct StartupInfo {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_chars: u32,
    y_chars: u32,
    fill: u32,
    flags: u32,
    show_window: u16,
    reserved_count: u16,
    reserved_bytes: *mut u8,
    stdin: *mut c_void,
    stdout: *mut c_void,
    stderr: *mut c_void,
}
#[repr(C)]
struct StartupInfoEx {
    startup: StartupInfo,
    attributes: *mut c_void,
}
#[repr(C)]
struct JobBasicLimits {
    process_time: i64,
    job_time: i64,
    limit_flags: u32,
    minimum_working_set: usize,
    maximum_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority: u32,
    scheduling: u32,
}
#[repr(C)]
struct JobExtendedLimits {
    basic: JobBasicLimits,
    io: [u64; 6],
    process_memory: usize,
    job_memory: usize,
    peak_process: usize,
    peak_job: usize,
}
#[repr(C)]
struct JobAccounting {
    user: i64,
    kernel: i64,
    period_user: i64,
    period_kernel: i64,
    faults: u32,
    total: u32,
    active: u32,
    terminated: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const SecurityAttributes, name: *const u16) -> *mut c_void;
    fn OpenJobObjectW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(
        job: *mut c_void,
        class: i32,
        information: *const c_void,
        length: u32,
    ) -> i32;
    fn QueryInformationJobObject(
        job: *mut c_void,
        class: i32,
        information: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
    fn TerminateJobObject(job: *mut c_void, code: u32) -> i32;
    fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: *mut i32) -> i32;
    fn InitializeProcThreadAttributeList(
        list: *mut c_void,
        count: u32,
        flags: u32,
        size: *mut usize,
    ) -> i32;
    fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *mut c_void,
        size: usize,
        previous: *mut c_void,
        returned: *mut usize,
    ) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(
        application: *const u16,
        command: *mut u16,
        process_security: *const SecurityAttributes,
        thread_security: *const SecurityAttributes,
        inherit: i32,
        flags: u32,
        environment: *mut c_void,
        cwd: *const u16,
        startup: *mut StartupInfoEx,
        information: *mut ProcessInformation,
    ) -> i32;
    fn CreatePipe(
        read: *mut *mut c_void,
        write: *mut *mut c_void,
        attributes: *const SecurityAttributes,
        size: u32,
    ) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn GetProcessTimes(
        process: *mut c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn ResumeThread(thread: *mut c_void) -> u32;
    fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    const CONTROLLER: ProcessIdentity = ProcessIdentity {
        pid: 1,
        start_time: 1,
    };
    const WORKER: ProcessIdentity = ProcessIdentity {
        pid: 2,
        start_time: 2,
    };

    fn denied_worker(identity: ProcessIdentity) -> io::Result<ProcessState> {
        if identity == CONTROLLER {
            Ok(ProcessState::Exited)
        } else {
            assert_eq!(identity, WORKER);
            // ERROR_ACCESS_DENIED returned when a historical PID now names a
            // protected process, without modifying any real process ACL.
            Err(io::Error::from_raw_os_error(5))
        }
    }

    #[test]
    fn worker_access_denial_accepts_real_empty_and_absent_named_job_proof() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let current = current_process_identity().unwrap();
        let name = format!(
            "Local\\MirrorLane-recovery-{}-{}-{}",
            current.pid,
            current.start_time,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let job = Job::new(&name).unwrap();
        prove_recovery_with(CONTROLLER, &[WORKER], &name, denied_worker, prove_job_empty).unwrap();
        drop(job);
        prove_recovery_with(CONTROLLER, &[WORKER], &name, denied_worker, prove_job_empty).unwrap();
    }

    #[test]
    fn worker_access_denial_never_hides_named_job_failure() {
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
        ] {
            let error = prove_recovery_with(
                CONTROLLER,
                &[WORKER],
                "Local\\MirrorLane-recovery-denied",
                denied_worker,
                |_| Err(io::Error::new(kind, "injected named-job proof failure")),
            )
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert!(error.to_string().contains("named-job proof failure"));
        }
    }

    #[test]
    fn inaccessible_controller_cannot_use_worker_job_fallback() {
        let error = prove_recovery_with(
            CONTROLLER,
            &[WORKER],
            "Local\\MirrorLane-recovery-controller-denied",
            |identity| {
                assert_eq!(identity, CONTROLLER);
                Err(io::Error::from_raw_os_error(5))
            },
            |_| panic!("an empty job cannot prove controller death"),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn matching_live_controller_or_worker_blocks_even_with_job_proof_available() {
        for live in [CONTROLLER, WORKER] {
            let error = prove_recovery_with(
                CONTROLLER,
                &[WORKER],
                "Local\\MirrorLane-recovery-live",
                |identity| {
                    Ok(if identity == live {
                        ProcessState::Running
                    } else {
                        ProcessState::Exited
                    })
                },
                |_| panic!("job proof must not override a matching live identity"),
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        }
    }

    #[test]
    fn other_worker_query_errors_do_not_use_access_denied_fallback() {
        let error = prove_recovery_with(
            CONTROLLER,
            &[WORKER],
            "Local\\MirrorLane-recovery-worker-error",
            |identity| {
                if identity == CONTROLLER {
                    Ok(ProcessState::Exited)
                } else {
                    Err(io::Error::other("injected worker query failure"))
                }
            },
            |_| panic!("unexpected query failures must remain fail-closed"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("worker query failure"));
    }
}
