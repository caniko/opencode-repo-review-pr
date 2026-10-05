use anyhow::{bail, Context, Result};
use std::{
    fs::{self, OpenOptions},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

struct ChildGroup {
    child: Child,
    reaped: bool,
}

impl ChildGroup {
    fn signal(&self, signal: rustix::process::Signal) {
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_child(&self.child),
            signal,
        );
    }

    fn finish(&mut self) -> Result<ExitStatus> {
        // The leader is still unreaped: its group ID cannot be recycled between
        // observation and descendant cleanup, including a normal successful exit.
        self.signal(rustix::process::Signal::KILL);
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        if !self.reaped {
            self.signal(rustix::process::Signal::KILL);
            let _ = self.child.wait();
        }
    }
}

/// Native argv execution with bounded lifetime and private, size-limited receipts.
pub fn run(
    executable: &Path,
    args: &[&str],
    cwd: &Path,
    output_root: &Path,
    seconds: u64,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    run_input(executable, args, cwd, output_root, seconds, None)
}

pub fn run_input(
    executable: &Path,
    args: &[&str],
    cwd: &Path,
    output_root: &Path,
    seconds: u64,
    input: Option<&[u8]>,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    crate::storage::private_dir(output_root)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let stdout = output_root.join(format!("{}-{nonce}.out", std::process::id()));
    let stderr = output_root.join(format!("{}-{nonce}.err", std::process::id()));
    let create = |path: &Path| -> Result<std::fs::File> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(path)?)
    };
    let mut command = Command::new(executable);
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    let stdin = if let Some(bytes) = input {
        use std::io::Write;
        let path = output_root.join(format!("{}-{nonce}.in", std::process::id()));
        create(&path)?.write_all(bytes)?;
        Stdio::from(std::fs::File::open(path)?)
    } else {
        Stdio::null()
    };
    let child = command
        .args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(stdin)
        .stdout(create(&stdout)?)
        .stderr(create(&stderr)?)
        .spawn()
        .context("configured executable unavailable")?;
    let mut group = ChildGroup {
        child,
        reaped: false,
    };
    let started = Instant::now();
    let status = loop {
        match rustix::process::waitid(
            rustix::process::WaitId::Pid(rustix::process::Pid::from_child(&group.child)),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        ) {
            Ok(Some(_)) => break group.finish()?,
            Ok(None) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
        if started.elapsed() > Duration::from_secs(seconds)
            || fs::metadata(&stdout)?.len() > 16 * 1024 * 1024
            || fs::metadata(&stderr)?.len() > 16 * 1024 * 1024
        {
            group.signal(rustix::process::Signal::TERM);
            std::thread::sleep(Duration::from_millis(100));
            group.finish()?;
            bail!(
                "provider command exceeded its lifetime/output bound; reconcile the existing dispatch"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if fs::metadata(&stdout)?.len() > 16 * 1024 * 1024
        || fs::metadata(&stderr)?.len() > 16 * 1024 * 1024
    {
        bail!("provider output exceeded limit");
    }
    Ok((
        status.code().unwrap_or(22),
        fs::read(stdout)?,
        fs::read(stderr)?,
    ))
}
