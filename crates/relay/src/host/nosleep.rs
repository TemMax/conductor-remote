//! Keep the Mac awake with caffeinate for a while.
//!
//! The relay owns one `caffeinate` child at a time. It needs no root: `-t` bounds the window and
//! `-w <relay pid>` ends the assertion when the relay exits, so a crash or a restart never leaves
//! the Mac awake for good. A closed lid still sleeps the Mac; that is accepted.
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::Serialize;

/// Longest window the API will arm. Long-weekend runs fit, while every phone request stays bounded.
pub const MAX_SECONDS: u64 = 7 * 24 * 3600;

/// The program the system spawner runs.
const CAFFEINATE: &str = "/usr/bin/caffeinate";

/// Starts the keep-awake process. Tests use a fake.
pub trait Spawner: Send + Sync + 'static {
    /// Whether keep-awake can be offered at all.
    fn available(&self) -> bool;
    /// Start the process with `args`.
    fn spawn(&self, args: &[String]) -> io::Result<Box<dyn Child>>;
}

/// A running keep-awake process.
pub trait Child: Send {
    fn id(&self) -> u32;
    /// `Some(exit code)` once it has exited, `None` while it runs.
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
    fn kill(&mut self) -> io::Result<()>;
}

/// Runs `/usr/bin/caffeinate`.
#[derive(Default)]
pub struct SystemSpawner;

impl SystemSpawner {
    pub fn new() -> Self {
        Self
    }
}

struct SystemChild(std::process::Child);

impl Child for SystemChild {
    fn id(&self) -> u32 {
        self.0.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.0.try_wait()?.map(|status| status.code().unwrap_or(-1)))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.0.kill()?;
        // Reap it so no zombie is left behind.
        self.0.wait().map(|_| ())
    }
}

impl Spawner for SystemSpawner {
    fn available(&self) -> bool {
        Path::new(CAFFEINATE).exists()
    }

    fn spawn(&self, args: &[String]) -> io::Result<Box<dyn Child>> {
        let child = Command::new(CAFFEINATE)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Box::new(SystemChild(child)))
    }
}

/// The phone's `NoSleepState` (plus `maxSeconds`, which the routes add on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoSleepState {
    pub available: bool,
    pub armed: bool,
    /// Epoch ms the window ends; `None` when not armed.
    pub until: Option<i64>,
    /// The caffeinate process; `None` when not armed.
    pub pid: Option<u32>,
    pub prevents_screen_lock: bool,
    pub max_seconds: u64,
}

struct Window {
    child: Box<dyn Child>,
    until: i64,
}

pub struct NoSleep {
    spawner: Arc<dyn Spawner>,
    prevent_screen_lock: bool,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    window: Option<Window>,
}

impl NoSleep {
    pub fn new(
        spawner: Arc<dyn Spawner>,
        prevent_screen_lock: bool,
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            spawner,
            prevent_screen_lock,
            clock,
            window: None,
        }
    }

    /// The current state. A child that has exited (its time ran out) is reaped and reads as not
    /// armed. A child that cannot be polled is treated the same way: nothing can be said for it.
    pub fn state(&mut self) -> NoSleepState {
        if let Some(window) = self.window.as_mut() {
            if !matches!(window.child.try_wait(), Ok(None)) {
                self.window = None;
            }
        }
        let available = self.spawner.available();
        match &self.window {
            Some(window) => NoSleepState {
                available,
                armed: true,
                until: Some(window.until),
                pid: Some(window.child.id()),
                prevents_screen_lock: self.prevent_screen_lock,
                max_seconds: MAX_SECONDS,
            },
            None => NoSleepState {
                available,
                armed: false,
                until: None,
                pid: None,
                prevents_screen_lock: false,
                max_seconds: MAX_SECONDS,
            },
        }
    }

    /// Keep the Mac awake for `seconds`, replacing a running window.
    pub fn arm(&mut self, seconds: u64) -> Result<NoSleepState, String> {
        if seconds == 0 || seconds > MAX_SECONDS {
            return Err(format!("seconds must be between 1 and {MAX_SECONDS}"));
        }
        self.stop();
        // -i: no idle sleep; -m: no disk idle sleep; -s: no system sleep (AC power only);
        // -t: the window; -d: no display sleep (the screen lock); -w: end with the relay.
        let mut args: Vec<String> = ["-i", "-m", "-s", "-t"].map(String::from).into();
        args.push(seconds.to_string());
        if self.prevent_screen_lock {
            args.push("-d".into());
        }
        args.push("-w".into());
        args.push(std::process::id().to_string());
        let child = self
            .spawner
            .spawn(&args)
            .map_err(|err| format!("could not start caffeinate: {err}"))?;
        let until = (self.clock)() + seconds as i64 * 1000;
        self.window = Some(Window { child, until });
        Ok(self.state())
    }

    /// End the window now.
    pub fn disarm(&mut self) -> NoSleepState {
        self.stop();
        self.state()
    }

    fn stop(&mut self) {
        if let Some(mut window) = self.window.take() {
            // An already-exited child makes kill fail; either way the window is over.
            let _ = window.child.kill();
        }
    }
}
