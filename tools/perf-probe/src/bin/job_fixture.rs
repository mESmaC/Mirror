//! Synthetic Windows processes for the production command-job acceptance tests.
//! This helper performs no VM, build, package, or shared-lock operations.

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    use perf_probe::windows_job::{CommandSpec, Job, ProcessIdentity, current_process_identity};
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    fn write_identity(path: &Path, identity: ProcessIdentity) -> std::io::Result<()> {
        durable(
            path,
            format!("{} {}", identity.pid, identity.start_time).as_bytes(),
        )
    }
    fn durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let pending = path.with_extension("pending");
        let mut file = File::create(&pending)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(pending, path)
    }
    fn wait() {
        thread::sleep(Duration::from_secs(60));
    }
    let args: Vec<OsString> = std::env::args_os().collect();
    let mode = args.get(1).and_then(|s| s.to_str()).unwrap_or("");
    match mode {
        "controller" | "controller-before-resume" => {
            let root = Path::new(&args[2]);
            write_identity(
                &root.join("controller.identity"),
                current_process_identity()?,
            )?;
            let job_name = args[3].to_str().unwrap();
            let job = Job::new(job_name)?;
            let spec = CommandSpec {
                program: std::env::current_exe()?,
                args: vec!["tree".into(), root.as_os_str().to_owned()],
                cwd: root.to_owned(),
                env: vec![("SystemRoot".into(), std::env::var_os("SystemRoot").unwrap())],
            };
            let _worker = job.spawn(&spec, |id| {
                write_identity(&root.join("worker.identity"), id)?;
                durable(
                    &root.join("before-resume"),
                    b"durable identity precedes execution",
                )?;
                if mode == "controller-before-resume" {
                    wait();
                }
                Ok(())
            })?;
            wait();
        }
        "tree" => {
            let root = Path::new(&args[2]);
            durable(&root.join("worker.executed"), b"yes")?;
            // A normal descendant inherits job membership without any adapter
            // help, and must die when the remote controller is killed.
            let mut child = Command::new(std::env::current_exe()?)
                .arg("leaf")
                .arg(root)
                .spawn()?;
            wait();
            child.kill()?;
            child.wait()?;
        }
        "leaf" => {
            write_identity(
                &Path::new(&args[2]).join("leaf.identity"),
                current_process_identity()?,
            )?;
            wait();
        }
        "touch" => durable(Path::new(&args[2]), b"executed")?,
        "echo" => {
            let mut out = std::io::stdout().lock();
            for arg in &args[2..] {
                let arg = arg.to_string_lossy();
                out.write_all(&(arg.len() as u32).to_le_bytes())?;
                out.write_all(arg.as_bytes())?;
            }
            std::io::stderr().write_all(b"fixture stderr")?;
        }
        "env" => {
            print!(
                "{:?}|{:?}",
                std::env::var_os("MIRROR_FIXTURE_ONLY"),
                std::env::var_os("PATH")
            );
        }
        "overflow" => {
            std::io::stdout().write_all(&vec![b'x'; perf_probe::windows_job::STREAM_LIMIT + 1])?;
            wait();
        }
        "exit7" => std::process::exit(7),
        _ => return Err(std::io::Error::other("unknown synthetic job fixture")),
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("synthetic job fixture requires Windows");
    std::process::exit(1);
}
