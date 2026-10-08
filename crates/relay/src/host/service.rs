//! The real host service: the relay's log, the settings the phone reads, keep-awake and the
//! Conductor restart, over the parts `main` hands it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::logbuf::{self, LogBuffer, Redactor};
use super::nosleep::NoSleep;
use super::restart::{self, AppControl, RestartTimings};
use super::HostService;
use crate::contract::{Priority, VERSION};
use crate::delivery::{BoxFuture, WriteAnswer};
use crate::reads::Reads;
use crate::ui::actor::UiHandle;

/// The LaunchAgent's stdout file, under the log directory.
const RELAY_LOG: &str = "relay.log";
/// The LaunchAgent's stderr file, under the log directory.
const RELAY_ERR_LOG: &str = "relay.err.log";

/// What started this relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Supervisor {
    /// The menu-bar app, which the relay stops with.
    App,
    /// The LaunchAgent of `service install`.
    Launchd,
    /// Neither: a terminal or a test.
    None,
}

impl Supervisor {
    fn as_str(self) -> &'static str {
        match self {
            Supervisor::App => "app",
            Supervisor::Launchd => "launchd",
            Supervisor::None => "none",
        }
    }
}

/// What the host is built from.
pub struct HostParts {
    /// The ring the tracing layer fills.
    pub logs: LogBuffer,
    /// Masks the token in the lines read from the log files.
    pub redactor: Redactor,
    /// Where the LaunchAgent writes `relay.log` and `relay.err.log`.
    pub log_dir: PathBuf,
    pub nosleep: NoSleep,
    pub app: Arc<dyn AppControl>,
    /// Counts the chats mid-turn before a restart; without it none are counted.
    pub reads: Option<Arc<Reads>>,
    /// Whether the Mac's screen is locked; `None` when that cannot be said.
    pub screen: Arc<dyn Fn() -> Option<bool> + Send + Sync>,
    /// Whether this process is the LaunchAgent (launchd set `XPC_SERVICE_NAME` to its label).
    pub managed: bool,
    /// Whether this process holds the Accessibility grant; asked on every status.
    pub trusted: Arc<dyn Fn() -> bool + Send + Sync>,
    /// What started this relay.
    pub supervisor: Supervisor,
    /// The port the relay listens on.
    pub port: u16,
    pub restart_timings: RestartTimings,
    /// The UI thread a restart holds so no write runs while Conductor quits; `None` in tests that
    /// do not need it.
    pub ui: Option<UiHandle>,
}

/// The host the routes call once `main` has wired it.
pub struct Host {
    logs: LogBuffer,
    redactor: Redactor,
    log_dir: PathBuf,
    nosleep: Mutex<NoSleep>,
    app: Arc<dyn AppControl>,
    reads: Option<Arc<Reads>>,
    screen: Arc<dyn Fn() -> Option<bool> + Send + Sync>,
    managed: bool,
    trusted: Arc<dyn Fn() -> bool + Send + Sync>,
    supervisor: Supervisor,
    port: u16,
    restart_timings: RestartTimings,
    ui: Option<UiHandle>,
    /// When a client last used the API; `None` before the first request.
    last_request: Mutex<Option<Instant>>,
    /// Whether a restart is running; a second one is refused.
    restarting: Arc<AtomicBool>,
}

/// Clears the restarting flag when dropped: the restart finished or its request went away.
struct RestartGuard(Arc<AtomicBool>);

impl Drop for RestartGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Host {
    pub fn new(parts: HostParts) -> Host {
        Host {
            logs: parts.logs,
            redactor: parts.redactor,
            log_dir: parts.log_dir,
            nosleep: Mutex::new(parts.nosleep),
            app: parts.app,
            reads: parts.reads,
            screen: parts.screen,
            managed: parts.managed,
            trusted: parts.trusted,
            supervisor: parts.supervisor,
            port: parts.port,
            restart_timings: parts.restart_timings,
            ui: parts.ui,
            last_request: Mutex::new(None),
            restarting: Arc::new(AtomicBool::new(false)),
        }
    }

    fn nosleep_lock(&self) -> MutexGuard<'_, NoSleep> {
        self.nosleep.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `{name, size, modifiedAt}` of each log file that exists.
    fn files(&self) -> Vec<Value> {
        [RELAY_LOG, RELAY_ERR_LOG]
            .into_iter()
            .filter_map(|name| {
                let metadata = std::fs::metadata(self.log_dir.join(name)).ok()?;
                let modified_at = metadata.modified().ok().and_then(|modified| {
                    let since = modified.duration_since(UNIX_EPOCH).ok()?;
                    i64::try_from(since.as_millis()).ok()
                });
                Some(json!({
                    "name": name,
                    "size": metadata.len(),
                    "modifiedAt": modified_at,
                }))
            })
            .collect()
    }

    /// The phone's `LogsResponse`.
    fn logs_response(&self, source: &str, entries: Value) -> WriteAnswer {
        WriteAnswer::json(
            200,
            json!({
                "source": source,
                "managed": self.managed,
                "startedAt": self.logs.started_at(),
                "now": now_ms(),
                "files": self.files(),
                "entries": entries,
            }),
        )
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The chats whose status is `working`, across the live workspaces. Chats that cannot be read
/// count as none.
fn working_chats(reads: &Reads) -> usize {
    let workspaces = match reads.list_workspaces() {
        Ok(workspaces) => workspaces,
        Err(error) => {
            tracing::warn!("could not count the chats mid-turn: {error}");
            return 0;
        }
    };
    workspaces
        .iter()
        .filter_map(|workspace| match reads.list_sessions(&workspace.id) {
            Ok(sessions) => Some(sessions),
            Err(error) => {
                tracing::warn!(workspace = %workspace.id, "could not read the chats: {error}");
                None
            }
        })
        .flatten()
        .filter(|session| session.status.as_deref() == Some("working"))
        .count()
}

impl HostService for Host {
    fn logs(&self, file: Option<String>, limit: Option<usize>) -> WriteAnswer {
        let limit = limit.unwrap_or(usize::MAX);
        let Some(file) = file else {
            let entries = serde_json::to_value(self.logs.entries(limit)).unwrap_or(Value::Null);
            return self.logs_response("live", entries);
        };
        let level = match file.as_str() {
            RELAY_LOG => "info",
            RELAY_ERR_LOG => "error",
            _ => return WriteAnswer::error(404, "no such log"),
        };
        let lines = match logbuf::tail(&self.log_dir.join(&file), limit) {
            Ok(lines) => lines,
            Err(error) => {
                return WriteAnswer::error(500, &format!("could not read {file}: {error}"));
            }
        };
        let entries = lines
            .iter()
            .map(|line| json!({ "t": null, "level": level, "text": self.redactor.redact(line) }))
            .collect();
        self.logs_response(&file, Value::Array(entries))
    }

    fn settings(&self) -> WriteAnswer {
        let nosleep = self.nosleep_lock().state();
        WriteAnswer::json(
            200,
            json!({
                "settings": {},
                "nosleep": nosleep,
                "screenLocked": (self.screen)(),
            }),
        )
    }

    fn status(&self) -> WriteAnswer {
        let running = self.app.running();
        // While Conductor is not running its database is closed: do not reopen it.
        let working = match &self.reads {
            Some(reads) if running => working_chats(reads),
            _ => 0,
        };
        let idle_ms = self
            .last_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map(|at| u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX));
        WriteAnswer::json(
            200,
            json!({
                "version": VERSION,
                "pid": std::process::id(),
                "startedAt": self.logs.started_at(),
                "supervisor": self.supervisor.as_str(),
                "port": self.port,
                "conductor": { "running": running },
                "accessibility": { "trusted": (self.trusted)() },
                "screenLocked": (self.screen)(),
                "activity": { "working": working, "idleMs": idle_ms },
            }),
        )
    }

    fn note_request(&self) {
        *self
            .last_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }

    fn nosleep(&self) -> WriteAnswer {
        WriteAnswer::json(200, json!(self.nosleep_lock().state()))
    }

    fn arm_nosleep(&self, seconds: u64) -> WriteAnswer {
        let mut nosleep = self.nosleep_lock();
        match nosleep.arm(seconds) {
            Ok(state) => WriteAnswer::json(200, json!({ "ok": true, "state": state })),
            Err(error) => WriteAnswer::json(
                400,
                json!({ "ok": false, "error": error, "state": nosleep.state() }),
            ),
        }
    }

    fn disarm_nosleep(&self) -> WriteAnswer {
        let state = self.nosleep_lock().disarm();
        WriteAnswer::json(200, json!({ "ok": true, "state": state }))
    }

    fn restart_conductor(&self, stop_agents: bool) -> BoxFuture<WriteAnswer> {
        if self
            .restarting
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Box::pin(async {
                WriteAnswer::json(
                    409,
                    json!({ "ok": false, "error": "Conductor is already restarting." }),
                )
            });
        }
        let guard = RestartGuard(Arc::clone(&self.restarting));
        let reads = self.reads.clone();
        let screen = Arc::clone(&self.screen);
        let app = Arc::clone(&self.app);
        let timings = self.restart_timings;
        let ui = self.ui.clone();
        Box::pin(async move {
            // Cleared however this future ends, finished or dropped.
            let _guard = guard;

            // Hold the UI thread first: no write may run between here and the relaunch.
            let release = match ui {
                Some(ui) => {
                    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
                    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
                    let held = ui.run(Priority::Interactive, move |_driver| {
                        let _ = started_tx.send(());
                        // Returns when `release_tx` is dropped: the restart ended or its
                        // request went away.
                        let _ = release_rx.recv();
                    });
                    tokio::pin!(held);
                    tokio::select! {
                        biased;
                        // A refused job is dropped unrun, which closes `started_tx`: that is
                        // not a start.
                        Ok(()) = started_rx => {}
                        outcome = &mut held => {
                            if let Err(error) = outcome {
                                return WriteAnswer::json(
                                    503,
                                    json!({ "ok": false, "error": error.to_string() }),
                                );
                            }
                            // The job ended without starting: nothing to wait for.
                        }
                    }
                    Some(release_tx)
                }
                None => None,
            };

            // Both read the disk or the window server: off the async workers.
            let looked = tokio::task::spawn_blocking(move || {
                let working = reads.as_deref().map_or(0, working_chats);
                (working, screen() == Some(true))
            })
            .await;
            let (working, locked) = match looked {
                Ok(looked) => looked,
                Err(error) => {
                    tracing::error!("counting the chats mid-turn failed: {error}");
                    return WriteAnswer::error(500, "internal error");
                }
            };
            let answer = restart::restart(app, working, locked, stop_agents, timings).await;
            drop(release);
            answer
        })
    }
}

/// The host before it is wired: 501 to everything.
pub struct Unwired;

/// `Host` written as a value, without parts, is the unwired host; the wired one is built with
/// [`Host::new`].
#[allow(non_upper_case_globals)]
pub const Host: Unwired = Unwired;

fn not_implemented() -> WriteAnswer {
    WriteAnswer::error(501, "not implemented")
}

impl HostService for Unwired {
    fn logs(&self, _file: Option<String>, _limit: Option<usize>) -> WriteAnswer {
        not_implemented()
    }

    fn settings(&self) -> WriteAnswer {
        not_implemented()
    }

    fn nosleep(&self) -> WriteAnswer {
        not_implemented()
    }

    fn arm_nosleep(&self, _seconds: u64) -> WriteAnswer {
        not_implemented()
    }

    fn disarm_nosleep(&self) -> WriteAnswer {
        not_implemented()
    }

    fn restart_conductor(&self, _stop_agents: bool) -> BoxFuture<WriteAnswer> {
        Box::pin(async { not_implemented() })
    }
}
