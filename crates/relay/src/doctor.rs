//! `conductor-remote doctor`: a read-only check of what the relay can see of Conductor.
//!
//! The report is built from a small trait over the probes, so a test runs every branch over a
//! fake. Only [`SystemProbes`] calls the bindings, and it only reads: it never sets anything,
//! never presses anything, never types and never activates an app.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use objc2_app_kit::NSRunningApplication;
use objc2_foundation::NSString;
use serde::Serialize;

use crate::contract::{APP_BUNDLE_ID, CONDUCTOR_BUNDLE_ID};
use crate::ui::actions::Driver;
use crate::ui::ax::{is_trusted, Element};
use crate::ui::driver::{UiDriver, ViewReport};
use crate::ui::identity::bundle_identifier;
use crate::ui::screen::session_state;
use crate::ui::snapshot::{snapshot, NodeSnapshot, SnapshotLimits};
use crate::ui::system::SystemDesktop;

/// Exit status when `--tree` or `--locate` was asked but nothing was read: the process is not
/// trusted, Conductor is not running, or (for `--locate`) the view could not be read.
pub const EXIT_NO_TREE: u8 = 3;
/// Exit status when `--out` could not be written.
pub const EXIT_OUT_FAILED: u8 = 4;

/// The arguments of `conductor-remote doctor`.
#[derive(clap::Args, Debug, Clone, PartialEq, Eq)]
pub struct DoctorArgs {
    /// Ask macOS to show its Accessibility dialog when this app is not trusted yet
    #[arg(long)]
    pub prompt: bool,
    /// Include a snapshot of Conductor's element tree (needs the grant and a running Conductor)
    #[arg(long)]
    pub tree: bool,
    /// Find the pane header, the chat tabs and the composer the writes use (needs the grant and a
    /// running Conductor)
    #[arg(long)]
    pub locate: bool,
    /// The deepest level of the tree; the root is level 0
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub depth: usize,
    /// The most nodes the tree includes
    #[arg(long, value_name = "N", default_value_t = 5000)]
    pub max_nodes: usize,
    /// Keep the first N characters of each value in the tree (values are hidden without this)
    #[arg(long, value_name = "N")]
    pub values: Option<usize>,
    /// Write the report as JSON to this file (mode 0600); a relative path is resolved against
    /// the current directory
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

impl DoctorArgs {
    fn limits(&self) -> SnapshotLimits {
        SnapshotLimits {
            max_depth: self.depth,
            max_nodes: self.max_nodes,
            value_chars: self.values,
        }
    }
}

/// What the identity probe reads of this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbedIdentity {
    pub executable: Option<String>,
    pub bundle_identifier: Option<String>,
}

/// The session as a person would describe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Session {
    pub locked: bool,
    pub on_console: bool,
}

/// The reads the report is made of. Every method only reads.
pub trait Probes {
    /// This process's executable path and bundle identifier.
    fn identity(&self) -> ProbedIdentity;
    /// Whether this process holds the Accessibility grant; `prompt` lets macOS show its dialog.
    fn trusted(&self, prompt: bool) -> bool;
    /// The process id of a running Conductor, `None` when none runs.
    fn conductor_pid(&self) -> Option<i32>;
    /// The window server session, `None` when unknown.
    fn session(&self) -> Option<Session>;
    /// The titles of Conductor's windows; an untitled window is an empty string.
    fn windows(&self, pid: i32) -> Result<Vec<String>, String>;
    /// A snapshot of Conductor's application element.
    fn tree(&self, pid: i32, limits: SnapshotLimits) -> NodeSnapshot;
    /// Where the pane header, the chat tabs and the composer are in Conductor's window.
    fn locate(&self) -> Result<ViewReport, String>;
}

#[derive(Debug, Serialize)]
pub struct IdentityReport {
    pub executable: Option<String>,
    pub bundle_identifier: Option<String>,
    pub expected_bundle: bool,
}

#[derive(Debug, Serialize)]
pub struct ConductorReport {
    pub running: bool,
    pub pid: Option<i32>,
}

/// What `doctor` found. Serialised, it is the JSON written by `--out`.
#[derive(Debug, Serialize)]
pub struct Report {
    pub identity: IdentityReport,
    pub trusted: bool,
    pub conductor: ConductorReport,
    pub session: Option<Session>,
    pub windows: Vec<String>,
    pub tree: Option<NodeSnapshot>,
    /// What `--locate` found; left out of the JSON when it was not read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<ViewReport>,
    /// Why the windows could not be read, for the summary only.
    #[serde(skip)]
    pub windows_error: Option<String>,
    /// Whether `--tree` was asked, for the exit status only.
    #[serde(skip)]
    pub tree_requested: bool,
    /// Why the view could not be read, for the summary only.
    #[serde(skip)]
    pub view_error: Option<String>,
    /// Whether `--locate` was asked, for the summary and the exit status only.
    #[serde(skip)]
    pub locate_requested: bool,
}

impl Report {
    /// 0 when the report is complete, [`EXIT_NO_TREE`] when the tree or the view was asked for
    /// and could not be read.
    pub fn exit_code(&self) -> u8 {
        if (self.tree_requested && self.tree.is_none())
            || (self.locate_requested && self.view.is_none())
        {
            EXIT_NO_TREE
        } else {
            0
        }
    }

    /// The human summary. `written` is where `--out` put the JSON, when it was asked for.
    pub fn summary(&self, written: Option<&Path>) -> String {
        let mut lines = vec!["conductor-remote doctor".to_owned()];
        lines.push(format!(
            "relay: {}",
            self.identity
                .executable
                .as_deref()
                .unwrap_or("unknown executable")
        ));
        lines.push(match &self.identity.bundle_identifier {
            None => format!(
                "bundle identifier: none (not run from an app bundle; expected {APP_BUNDLE_ID})"
            ),
            Some(id) if self.identity.expected_bundle => {
                format!("bundle identifier: {id} (as expected)")
            }
            Some(id) => format!("bundle identifier: {id} (expected {APP_BUNDLE_ID})"),
        });
        lines.push(format!(
            "accessibility: {}",
            if self.trusted {
                "trusted"
            } else {
                "not trusted"
            }
        ));
        lines.push(match self.conductor.pid {
            Some(pid) => format!("conductor: running (pid {pid})"),
            None => "conductor: not running".to_owned(),
        });
        lines.push(match self.session {
            None => "session: unknown".to_owned(),
            Some(session) => format!(
                "session: {}, {}",
                if session.locked { "locked" } else { "unlocked" },
                if session.on_console {
                    "on console"
                } else {
                    "not on console"
                }
            ),
        });
        if let Some(error) = &self.windows_error {
            lines.push(format!("windows: could not be read ({error})"));
        } else if self.trusted && self.conductor.running {
            lines.push(format!("windows: {}", self.windows.len()));
            for title in &self.windows {
                lines.push(format!("  - {title:?}"));
            }
        } else {
            lines.push("windows: not read".to_owned());
        }
        if self.tree_requested {
            lines.push(match &self.tree {
                Some(_) => "tree: included".to_owned(),
                None => "tree: not read (needs the grant and a running Conductor)".to_owned(),
            });
        }
        if self.locate_requested {
            lines.push(match (&self.view, &self.view_error) {
                (_, Some(error)) => format!("view: could not be read ({error})"),
                (Some(view), None) => format!(
                    "view: pane {}, {} chat tabs, selected {}, composer {}",
                    view.pane_header
                        .as_ref()
                        .map_or_else(|| "none".to_owned(), |header| format!("{header:?}")),
                    view.chat_tabs,
                    view.selected_tab
                        .map_or_else(|| "no tab selected".to_owned(), |tab| tab.to_string()),
                    if view.composer { "found" } else { "missing" },
                ),
                (None, None) => {
                    "view: not read (needs the grant and a running Conductor)".to_owned()
                }
            });
        }
        if let Some(path) = written {
            lines.push(format!("report written to {}", path.display()));
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }
}

/// Reads everything the arguments ask for, in this order: identity, trust, Conductor's pid, the
/// session, then (only when trusted and Conductor runs) the windows, the tree and, with `--locate`, the view.
pub fn build_report(args: &DoctorArgs, probes: &dyn Probes) -> Report {
    let probed = probes.identity();
    let expected_bundle = probed.bundle_identifier.as_deref() == Some(APP_BUNDLE_ID);
    let trusted = probes.trusted(args.prompt);
    let pid = probes.conductor_pid();
    let session = probes.session();
    let readable = pid.filter(|_| trusted);
    let (windows, windows_error) = match readable.map(|pid| probes.windows(pid)) {
        Some(Ok(titles)) => (titles, None),
        Some(Err(error)) => (Vec::new(), Some(error)),
        None => (Vec::new(), None),
    };
    let tree = readable
        .filter(|_| args.tree)
        .map(|pid| probes.tree(pid, args.limits()));
    let (view, view_error) = match readable.filter(|_| args.locate).map(|_| probes.locate()) {
        Some(Ok(view)) => (Some(view), None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, None),
    };
    Report {
        identity: IdentityReport {
            executable: probed.executable,
            bundle_identifier: probed.bundle_identifier,
            expected_bundle,
        },
        trusted,
        conductor: ConductorReport {
            running: pid.is_some(),
            pid,
        },
        session,
        windows,
        tree,
        windows_error,
        tree_requested: args.tree,
        view,
        view_error,
        locate_requested: args.locate,
    }
}

/// What a run produced: the summary for stdout, a line for stderr and the exit status.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub summary: String,
    pub error: Option<String>,
    pub code: u8,
}

/// Builds the report, writes `--out` when asked (a relative path is resolved against `cwd`) and
/// settles the exit status: [`EXIT_OUT_FAILED`] wins over [`EXIT_NO_TREE`].
pub fn execute(args: &DoctorArgs, probes: &dyn Probes, cwd: &Path) -> Outcome {
    let report = build_report(args, probes);
    let mut code = report.exit_code();
    let mut error = None;
    let mut written = None;
    if let Some(out) = &args.out {
        let path = cwd.join(out);
        match write_json(&path, &report) {
            Ok(()) => written = Some(path),
            Err(message) => {
                code = EXIT_OUT_FAILED;
                error = Some(format!(
                    "error: could not write {}: {message}",
                    path.display()
                ));
            }
        }
    }
    Outcome {
        summary: report.summary(written.as_deref()),
        error,
        code,
    }
}

/// Removes an existing file (a link is removed, not followed) and creates the new one with mode
/// 0600.
fn write_json(path: &Path, report: &Report) -> Result<(), String> {
    let mut json = serde_json::to_string_pretty(report).map_err(|error| error.to_string())?;
    json.push('\n');
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(json.as_bytes())
        .map_err(|error| error.to_string())
}

/// The real probes: the bindings of [`crate::ui`] and AppKit, read-only.
pub struct SystemProbes;

const AX_TIMEOUT_SECONDS: f32 = 2.0;

impl SystemProbes {
    /// Bounds the Accessibility reads to 2 seconds, before any of them: on the system-wide
    /// element here, and again on each element the probes make (a timeout set on one element is
    /// not assumed to carry over to another).
    pub fn new() -> SystemProbes {
        bound(&Element::system_wide());
        SystemProbes
    }
}

/// Sets the messaging timeout of `element` and returns it; a failure is a warning, not a stop.
fn bound(element: &Element) {
    if let Err(error) = element.set_messaging_timeout(AX_TIMEOUT_SECONDS) {
        eprintln!("warning: could not set the Accessibility messaging timeout: {error}");
    }
}

fn conductor_element(pid: i32) -> Element {
    let element = Element::application(pid);
    bound(&element);
    element
}

impl Default for SystemProbes {
    fn default() -> Self {
        SystemProbes::new()
    }
}

impl Probes for SystemProbes {
    fn identity(&self) -> ProbedIdentity {
        ProbedIdentity {
            executable: std::env::current_exe()
                .ok()
                .map(|path| path.display().to_string()),
            bundle_identifier: bundle_identifier(),
        }
    }

    fn trusted(&self, prompt: bool) -> bool {
        is_trusted(prompt)
    }

    fn conductor_pid(&self) -> Option<i32> {
        NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(
            CONDUCTOR_BUNDLE_ID,
        ))
        .iter()
        .filter(|app| !app.isTerminated())
        .map(|app| app.processIdentifier())
        .find(|pid| *pid != -1)
    }

    fn session(&self) -> Option<Session> {
        session_state().map(|state| Session {
            locked: state.locked,
            on_console: state.on_console,
        })
    }

    fn windows(&self, pid: i32) -> Result<Vec<String>, String> {
        let windows = conductor_element(pid)
            .elements("AXWindows")
            .map_err(|error| error.to_string())?;
        windows
            .iter()
            .map(|window| {
                bound(window);
                window
                    .string("AXTitle")
                    .map(Option::unwrap_or_default)
                    .map_err(|error| error.to_string())
            })
            .collect()
    }

    fn tree(&self, pid: i32, limits: SnapshotLimits) -> NodeSnapshot {
        snapshot(&conductor_element(pid), limits)
    }

    /// `UiDriver::locate` only reads: it checks trust, the lock and the pid, then walks the
    /// first window; it presses, types and activates nothing.
    fn locate(&self) -> Result<ViewReport, String> {
        Driver::new(SystemDesktop::new())
            .locate()
            .map_err(|error| error.to_string())
    }
}

/// Runs `doctor` against the real Mac and prints the outcome.
pub fn run(args: &DoctorArgs) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("error: could not read the current directory: {error}");
            return ExitCode::from(EXIT_OUT_FAILED);
        }
    };
    let outcome = execute(args, &SystemProbes::new(), &cwd);
    print!("{}", outcome.summary);
    if let Some(error) = &outcome.error {
        eprintln!("{error}");
    }
    ExitCode::from(outcome.code)
}
