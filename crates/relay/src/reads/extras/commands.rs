//! Running external programs, behind a trait so tests can script them.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Directories searched for a program after `PATH`: a service started by launchd has a minimal one.
const FALLBACK_DIRS: [&str; 4] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

/// How often a finished-looking child is polled for its exit.
const POLL: Duration = Duration::from_millis(5);

/// How long, after the program has exited, its standard error is still awaited: a grandchild that
/// keeps the pipe open must not hold the call, and what was read by then is returned.
const STDERR_GRACE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub max_stdout: usize,
}

/// What a finished program left: its exit code (`None` when a signal ended it), its standard output
/// and the first `Limits::max_stdout` bytes of its standard error (more is cut, not an error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("could not start {program}")]
    Spawn { program: String },
    #[error("{program} timed out")]
    Timeout { program: String },
    #[error("{program} wrote too much output")]
    TooLarge { program: String },
}

/// Runs external programs. The real one spawns them; tests pass a fake.
pub trait Commands: Send + Sync + 'static {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError>;
}

/// Spawns the programs for real.
pub struct SystemCommands;

impl Commands for SystemCommands {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        let started = Instant::now();
        let spawn_error = || CommandError::Spawn {
            program: program.to_owned(),
        };
        let path_env = std::env::var("PATH").ok();
        let resolved = resolve_program(program, path_env.as_deref()).ok_or_else(spawn_error)?;
        let mut command = Command::new(resolved);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|_| spawn_error())?;
        let Some(mut stdout) = child.stdout.take() else {
            kill(&mut child);
            return Err(spawn_error());
        };

        // The reader sends chunks until the pipe closes; it is never joined, so a grandchild that
        // keeps the pipe open after the kill cannot hold this call.
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        // Standard error is read on its own thread too, so a program that fills both pipes is never
        // stuck on one while this call waits on the other. The thread reads to the end of the pipe
        // but sends only the first `max_stdout` bytes; the rest is read and dropped.
        let (err_tx, err_rx) = mpsc::channel::<Vec<u8>>();
        if let Some(mut stderr) = child.stderr.take() {
            let cap = limits.max_stdout;
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut sent = 0usize;
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            let keep = n.min(cap.saturating_sub(sent));
                            if keep > 0 {
                                sent += keep;
                                if err_tx.send(buf[..keep].to_vec()).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            });
        }

        let timed_out = || CommandError::Timeout {
            program: program.to_owned(),
        };
        let mut out = Vec::new();
        loop {
            let remaining = limits.timeout.saturating_sub(started.elapsed());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    out.extend_from_slice(&chunk);
                    if out.len() > limits.max_stdout {
                        kill(&mut child);
                        return Err(CommandError::TooLarge {
                            program: program.to_owned(),
                        });
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    kill(&mut child);
                    return Err(timed_out());
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // The output is complete; the program may still be running.
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Ok(Output {
                        code: status.code(),
                        stdout: out,
                        stderr: collect_stderr(&err_rx, limits.timeout, started),
                    })
                }
                Ok(None) if started.elapsed() < limits.timeout => std::thread::sleep(POLL),
                Ok(None) => {
                    kill(&mut child);
                    return Err(timed_out());
                }
                Err(_) => {
                    kill(&mut child);
                    return Err(spawn_error());
                }
            }
        }
    }
}

/// The standard error the reader thread has sent: until it ends, at most `STDERR_GRACE` (and never
/// past the call's time-out) after the program exited.
fn collect_stderr(rx: &mpsc::Receiver<Vec<u8>>, timeout: Duration, started: Instant) -> Vec<u8> {
    let deadline = Instant::now() + STDERR_GRACE.min(timeout.saturating_sub(started.elapsed()));
    let mut err = Vec::new();
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => err.extend_from_slice(&chunk),
            Err(_) => return err,
        }
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Where `program` is: itself when it contains a `/`; otherwise the first directory of `path_env`,
/// then of `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, that holds an executable file of that name.
pub fn resolve_program(program: &str, path_env: Option<&str>) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    path_env
        .into_iter()
        .flat_map(|paths| paths.split(':'))
        .filter(|dir| !dir.is_empty())
        .chain(FALLBACK_DIRS)
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}
