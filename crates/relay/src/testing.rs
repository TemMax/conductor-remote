//! Test doubles for the interfaces in `contract`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use tokio::sync::watch;

use crate::contract::{Asset, Assets, ConductorControl, ConductorStatus, LaunchError};
use crate::reads::extras::commands::{CommandError, Commands, Limits, Output};

pub struct FakeConductor {
    tx: watch::Sender<ConductorStatus>,
    launches: AtomicUsize,
    launch_error: Mutex<Option<String>>,
}

impl FakeConductor {
    pub fn new(status: ConductorStatus) -> Self {
        let (tx, _rx) = watch::channel(status);
        Self {
            tx,
            launches: AtomicUsize::new(0),
            launch_error: Mutex::new(None),
        }
    }

    pub fn set_status(&self, status: ConductorStatus) {
        self.tx.send_replace(status);
    }

    pub fn fail_launch_with(&self, message: &str) {
        *self.launch_error.lock().unwrap() = Some(message.to_owned());
    }

    pub fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }
}

impl ConductorControl for FakeConductor {
    fn status(&self) -> ConductorStatus {
        *self.tx.borrow()
    }

    fn subscribe(&self) -> watch::Receiver<ConductorStatus> {
        self.tx.subscribe()
    }

    fn launch(&self) -> Result<(), LaunchError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        let failure = self.launch_error.lock().unwrap().clone();
        match failure {
            Some(message) => Err(LaunchError(message)),
            None => {
                self.tx.send_replace(ConductorStatus::Running);
                Ok(())
            }
        }
    }
}

#[derive(Default)]
pub struct MemoryAssets {
    files: HashMap<String, (Vec<u8>, String)>,
}

impl MemoryAssets {
    pub fn with(mut self, path: &str, content_type: &str, bytes: &[u8]) -> Self {
        self.files
            .insert(path.to_owned(), (bytes.to_vec(), content_type.to_owned()));
        self
    }
}

impl Assets for MemoryAssets {
    fn get(&self, path: &str) -> Option<Asset> {
        self.files.get(path).map(|(bytes, content_type)| Asset {
            bytes: Cow::Owned(bytes.clone()),
            content_type: content_type.clone(),
        })
    }
}

/// One recorded call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandCall {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub limits: Limits,
}

struct Rule {
    program: String,
    args_prefix: Vec<String>,
    /// `Ok((code, stdout, stderr))`, or a time-out.
    answer: Result<(i32, String, String), ()>,
}

/// Answers `Commands::run` from a script. A call that matches no rule is `CommandError::Spawn`.
pub struct FakeCommands {
    rules: Mutex<Vec<Rule>>,
    calls: Mutex<Vec<CommandCall>>,
}

impl FakeCommands {
    pub fn new() -> Self {
        Self {
            rules: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn add(&self, program: &str, args_prefix: &[&str], answer: Result<(i32, String, String), ()>) {
        self.rules.lock().unwrap().push(Rule {
            program: program.to_owned(),
            args_prefix: args_prefix.iter().map(|a| (*a).to_owned()).collect(),
            answer,
        });
    }

    /// From now on a call of `program` whose arguments start with `args_prefix` exits with `code`
    /// and prints `stdout`. A later rule wins over an earlier one.
    pub fn on(&self, program: &str, args_prefix: &[&str], code: i32, stdout: &str) {
        self.add(
            program,
            args_prefix,
            Ok((code, stdout.to_owned(), String::new())),
        );
    }

    /// The same, with `stderr` as the standard error the call prints.
    pub fn on_with_stderr(
        &self,
        program: &str,
        args_prefix: &[&str],
        code: i32,
        stdout: &str,
        stderr: &str,
    ) {
        self.add(
            program,
            args_prefix,
            Ok((code, stdout.to_owned(), stderr.to_owned())),
        );
    }

    /// From now on such a call fails with `CommandError::Timeout`.
    pub fn fail(&self, program: &str, args_prefix: &[&str]) {
        self.add(program, args_prefix, Err(()));
    }

    pub fn calls(&self) -> Vec<CommandCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl Default for FakeCommands {
    fn default() -> Self {
        Self::new()
    }
}

impl Commands for FakeCommands {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        self.calls.lock().unwrap().push(CommandCall {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            cwd: cwd.map(Path::to_path_buf),
            limits,
        });
        let rules = self.rules.lock().unwrap();
        let rule = rules.iter().rev().find(|rule| {
            rule.program == program
                && args.len() >= rule.args_prefix.len()
                && rule.args_prefix.iter().zip(args).all(|(p, a)| p == a)
        });
        match rule {
            None => Err(CommandError::Spawn {
                program: program.to_owned(),
            }),
            Some(Rule {
                answer: Err(()), ..
            }) => Err(CommandError::Timeout {
                program: program.to_owned(),
            }),
            Some(Rule {
                answer: Ok((code, stdout, stderr)),
                ..
            }) => Ok(Output {
                code: Some(*code),
                stdout: stdout.clone().into_bytes(),
                stderr: stderr.clone().into_bytes(),
            }),
        }
    }
}
