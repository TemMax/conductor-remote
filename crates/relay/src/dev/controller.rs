//! `DevServers`, the real `DevServerService`: starts, stops and reports a workspace's dev server,
//! publishes the ports it listens on through `tailscale serve`, rebuilds those forwards after a
//! restart and closes the ones whose server is gone.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{Mutex, MutexGuard, OwnedMutexGuard};

use super::bridge::{bridge_matches, Bridge};
use super::ports::{run_task_ports_in, tcp_open, wait_for_port, ProcessSnapshot};
use super::run_configs::run_configs;
use super::store::{self, Forward};
use super::tailscale::{choose_serve_port, ServeStatus, Tailscale};
use super::{DevServerForward, DevServerResult, DevServerService, DevServerState};
use crate::contract::Priority;
use crate::delivery::agent::busy;
use crate::delivery::service::{blocking, internal, workspace_target};
use crate::delivery::{BoxFuture, WriteAnswer};
use crate::reads::extras::commands::Commands;
use crate::reads::receipts::WriteWorkspace;
use crate::reads::workspaces::Workspace;
use crate::reads::{ReadError, Reads};
use crate::ui::actor::{UiHandle, UiRunError};
use crate::ui::driver::{RunOutcome, Target, UiDriver};

const NO_WORKSPACE: &str = "workspace not found";
const NOT_CONNECTED: &str = "Tailscale is not connected on this Mac";
const NO_SERVE: &str = "Tailscale Serve is not available on this Mac";
const CHOOSE_CONFIG: &str = "Choose which Run config to start";
const NO_SERVE_PORT: &str = "no free Tailscale Serve port is available";
const STOPPED_AGAIN: &str = "; it was stopped again";

/// At most this many ports of one workspace are shown and forwarded.
const MAX_FORWARDS: usize = 10;

/// At most this many serve ports are chosen for one forward before it is given up: a chosen port
/// something on this Mac already listens on is passed over.
const SERVE_PORT_CHOICES: usize = 10;

/// How long a state poll may use a kept `ps` listing.
const POLL_LISTING_AGE: Duration = Duration::from_secs(3);

/// How long the tailnet name is kept.
const HOST_TTL: Duration = Duration::from_secs(60);

/// How long a state poll and the availability check of a start may use a kept serve status.
const SERVE_STATUS_AGE: Duration = Duration::from_secs(5);

/// A forward is released once its target was closed on this many sweeps in a row.
const SWEEP_MISSES: u32 = 2;

pub struct DevDeps {
    pub reads: Arc<Reads>,
    pub ui: UiHandle,
    pub commands: Arc<dyn Commands>,
    pub home: PathBuf,
    pub state_dir: PathBuf,
    /// The `tailscale` binary; `None` when this Mac has none.
    pub tailscale: Option<PathBuf>,
}

/// How long a start and a stop wait for the dev server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevTimings {
    /// How long a started task may take to listen on a port.
    pub ready_wait: Duration,
    /// How long a stopped task's port may keep accepting connections.
    pub stop_wait: Duration,
    /// The pause between two looks at a started task's ports.
    pub look: Duration,
}

impl Default for DevTimings {
    fn default() -> DevTimings {
        DevTimings {
            ready_wait: Duration::from_secs(15),
            stop_wait: Duration::from_secs(5),
            look: Duration::from_millis(500),
        }
    }
}

pub struct DevServers {
    inner: Arc<Inner>,
}

impl DevServers {
    pub fn new(deps: DevDeps) -> Arc<DevServers> {
        DevServers::with_timings(deps, DevTimings::default())
    }

    pub fn with_timings(deps: DevDeps, timings: DevTimings) -> Arc<DevServers> {
        let tailscale = deps
            .tailscale
            .map(|bin| Arc::new(Tailscale::new(bin, Arc::clone(&deps.commands))));
        Arc::new(DevServers {
            inner: Arc::new(Inner {
                reads: deps.reads,
                ui: deps.ui,
                commands: deps.commands,
                home: deps.home,
                state_dir: deps.state_dir,
                tailscale,
                timings,
                table: Mutex::new(Table::default()),
                steps: StdMutex::new(HashMap::new()),
                host: StdMutex::new(None),
                serve_status: StdMutex::new(None),
                misses: StdMutex::new(HashMap::new()),
                processes: Arc::new(ProcessSnapshot::new()),
            }),
        })
    }

    /// Rebuilds the bridges of the stored forwards after a restart.
    pub async fn restore(&self) {
        let inner = &self.inner;
        if inner.tailscale.is_none() {
            return;
        }
        let stored = inner.table().await.forwards.clone();
        for forward in stored {
            let _step = inner.step(&forward.workspace_id).await;
            inner.restore_forward(&key_of(&forward)).await;
        }
    }

    /// One pass of the sweeper: closes forwards whose server stopped listening.
    pub async fn sweep(&self) {
        let inner = &self.inner;
        let stored = inner.table().await.forwards.clone();
        // A count outlives its record only until the next pass.
        lock(&inner.misses).retain(|key, _| stored.iter().any(|forward| key_of(forward) == *key));
        for forward in stored {
            let _step = inner.step(&forward.workspace_id).await;
            inner.sweep_forward(&key_of(&forward)).await;
        }
    }
}

impl DevServerService for DevServers {
    fn state(&self, workspace_id: String) -> BoxFuture<WriteAnswer> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.state(workspace_id).await })
    }

    fn start(
        &self,
        workspace_id: String,
        run_config_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        let inner = Arc::clone(&self.inner);
        spawned(async move { inner.start(workspace_id, run_config_id, priority).await })
    }

    fn stop(&self, workspace_id: String, priority: Priority) -> BoxFuture<WriteAnswer> {
        let inner = Arc::clone(&self.inner);
        spawned(async move { inner.stop(workspace_id, priority).await })
    }
}

/// A stored forward's identity: its workspace and the port of the dev server.
type Key = (String, u16);

fn key_of(forward: &Forward) -> Key {
    (forward.workspace_id.clone(), forward.target_port)
}

/// The stored forwards and the bridges this process holds for them. Its lock is the process-wide
/// one: every change of it, and the choice of a serve port, happens under it.
#[derive(Default)]
struct Table {
    /// The file has been read.
    loaded: bool,
    forwards: Vec<Forward>,
    bridges: HashMap<Key, Bridge>,
}

impl Table {
    fn find(&self, key: &Key) -> Option<&Forward> {
        self.forwards
            .iter()
            .find(|forward| forward.workspace_id == key.0 && forward.target_port == key.1)
    }

    fn forget(&mut self, key: &Key) {
        self.forwards
            .retain(|forward| !(forward.workspace_id == key.0 && forward.target_port == key.1));
    }
}

struct Inner {
    reads: Arc<Reads>,
    ui: UiHandle,
    commands: Arc<dyn Commands>,
    home: PathBuf,
    state_dir: PathBuf,
    tailscale: Option<Arc<Tailscale>>,
    timings: DevTimings,
    table: Mutex<Table>,
    /// One start, stop, release or restore step at a time per workspace id.
    steps: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The tailnet name and when it was read.
    host: StdMutex<Option<(Instant, String)>>,
    /// The last `tailscale serve status` and when it was read; the relay's own `serve` and
    /// `serve off` calls clear it, and so does the end of a start or a stop.
    serve_status: StdMutex<Option<(Instant, ServeStatus)>>,
    /// How many sweeps in a row found a forward's target closed.
    misses: StdMutex<HashMap<Key, u32>>,
    /// The `ps` listing the state polls of every workspace share.
    processes: Arc<ProcessSnapshot>,
}

/// Why a UI job did not run the task.
enum RunFailure {
    /// The UI queue is full: the busy answer and its text.
    Busy(WriteAnswer, String),
    Failed(String),
}

impl RunFailure {
    fn text(&self) -> &str {
        match self {
            RunFailure::Busy(_, text) | RunFailure::Failed(text) => text,
        }
    }
}

/// What a start or a stop answers besides the workspace's state.
struct Ending {
    status: u16,
    ok: bool,
    task: Option<String>,
    changed: Option<bool>,
    error: Option<String>,
}

impl Inner {
    // ---- the service ----

    async fn state(&self, workspace_id: String) -> WriteAnswer {
        let workspace = match self.workspace(&workspace_id).await {
            Err(answer) => return answer,
            Ok(workspace) => workspace,
        };
        match self.state_of(&workspace, POLL_LISTING_AGE, None).await {
            Err(answer) => answer,
            Ok(state) => answer(200, &state),
        }
    }

    async fn start(
        &self,
        workspace_id: String,
        run_config_id: Option<String>,
        priority: Priority,
    ) -> WriteAnswer {
        let workspace = match self.workspace(&workspace_id).await {
            Err(answer) => return answer,
            Ok(workspace) => workspace,
        };
        let began = Instant::now();
        let _step = self.step(&workspace.id).await;
        // The first two calls of the binary run side by side: the state below finds the tailnet
        // name kept, and the serve status too when the workspace has stored forwards.
        let (serve, _) = tokio::join!(self.serve_status_within(SERVE_STATUS_AGE), self.host());
        let state = match self.state_of(&workspace, Duration::ZERO, None).await {
            Err(answer) => return answer,
            Ok(state) => state,
        };

        let chosen = match &run_config_id {
            None => None,
            Some(id) => match state.run_configs.iter().find(|config| config.id == *id) {
                Some(config) => Some(config.name.clone()),
                None => {
                    let error = format!("Run config {id} is not available in this workspace");
                    return refused(502, state, error);
                }
            },
        };
        if !state.available {
            return result(409, false, state, None);
        }
        if serve.is_err() {
            return refused(502, state, NO_SERVE.to_owned());
        }

        if run_config_id.is_none() {
            let running: Vec<u16> = state
                .forwards
                .iter()
                .filter(|forward| forward.running)
                .map(|forward| forward.port)
                .collect();
            if !running.is_empty() {
                // The task already listens: nothing is pressed, and the forwards that were
                // there before stay whatever happens to the new ones.
                let before = self.forwarded_ports(&workspace.id).await;
                return match self.forward_all(&workspace.id, &running).await {
                    Ok(served) => {
                        let ending = Ending {
                            status: 200,
                            ok: true,
                            task: None,
                            changed: Some(false),
                            error: None,
                        };
                        self.finish(&workspace, ending, Some(served)).await
                    }
                    Err(error) => {
                        self.release_unless(&workspace.id, &before).await;
                        let ending = Ending {
                            status: 502,
                            ok: false,
                            task: None,
                            changed: Some(false),
                            error: Some(error),
                        };
                        self.finish(&workspace, ending, None).await
                    }
                };
            }
            if state.run_configs.len() > 1 {
                return refused(502, state, CHOOSE_CONFIG.to_owned());
            }
        }

        // The config that was asked for, else the only one, else the strip's current task.
        let name = chosen.or_else(|| state.run_configs.first().map(|config| config.name.clone()));
        let target = workspace_target(&write_workspace(&workspace));
        let pressing = Instant::now();
        let outcome = match self.run_task(priority, &target, name, true).await {
            Err(RunFailure::Busy(answer, _)) => return answer,
            Err(RunFailure::Failed(error)) => return refused(502, state, error),
            Ok(outcome) => outcome,
        };
        let ui = pressing.elapsed();
        let RunOutcome { changed, task } = outcome;

        let waiting = Instant::now();
        let ports = match self.wait_until_listening(&workspace).await {
            Err(answer) => return answer,
            Ok(ports) => ports,
        };
        let wait = waiting.elapsed();
        let Some(ports) = ports else {
            let stopped = self.stop_again(changed, priority, &target).await;
            let error = format!(
                "{} started, but nothing listened on a port{stopped}",
                task.as_deref().unwrap_or("Run task")
            );
            let ending = Ending {
                status: 502,
                ok: false,
                task,
                changed: Some(changed),
                error: Some(error),
            };
            return self.finish(&workspace, ending, None).await;
        };

        let ports: Vec<u16> = ports.into_iter().take(MAX_FORWARDS).collect();
        let before = self.forwarded_ports(&workspace.id).await;
        let forwarding = Instant::now();
        match self.forward_all(&workspace.id, &ports).await {
            Ok(served) => {
                let forward = forwarding.elapsed();
                let logged = task.clone();
                let ending = Ending {
                    status: 200,
                    ok: true,
                    task,
                    changed: Some(changed),
                    error: None,
                };
                let answer = self.finish(&workspace, ending, Some(served)).await;
                tracing::info!(
                    workspace = %workspace.id,
                    task = ?logged,
                    ui_ms = millis(ui),
                    wait_ms = millis(wait),
                    forward_ms = millis(forward),
                    total_ms = millis(began.elapsed()),
                    "dev server started"
                );
                answer
            }
            Err(error) => {
                if changed {
                    self.release_workspace(&workspace.id).await;
                } else {
                    // The task was running before: only what this request forwarded goes.
                    self.release_unless(&workspace.id, &before).await;
                }
                let stopped = self.stop_again(changed, priority, &target).await;
                let ending = Ending {
                    status: 502,
                    ok: false,
                    task,
                    changed: Some(changed),
                    error: Some(format!("{error}{stopped}")),
                };
                self.finish(&workspace, ending, None).await
            }
        }
    }

    async fn stop(&self, workspace_id: String, priority: Priority) -> WriteAnswer {
        let workspace = match self.workspace(&workspace_id).await {
            Err(answer) => return answer,
            Ok(workspace) => workspace,
        };
        let began = Instant::now();
        let _step = self.step(&workspace.id).await;
        let before = match self.state_of(&workspace, Duration::ZERO, None).await {
            Err(answer) => return answer,
            Ok(state) => state,
        };

        let target = workspace_target(&write_workspace(&workspace));
        let pressing = Instant::now();
        let outcome = match self.run_task(priority, &target, None, false).await {
            Err(RunFailure::Busy(answer, _)) => return answer,
            Err(RunFailure::Failed(error)) => return refused(502, before, error),
            Ok(outcome) => outcome,
        };
        let ui = pressing.elapsed();
        let RunOutcome { changed, task } = outcome;
        let releasing = Instant::now();
        self.release_workspace(&workspace.id).await;
        let release = releasing.elapsed();

        let waiting = Instant::now();
        if let Some(port) = before.port {
            if !wait_for_port(port, false, self.timings.stop_wait).await {
                let error = format!(
                    "{} stopped, but :{port} is still listening",
                    task.as_deref().unwrap_or("Run task")
                );
                let ending = Ending {
                    status: 502,
                    ok: false,
                    task,
                    changed: Some(changed),
                    error: Some(error),
                };
                return self.finish(&workspace, ending, None).await;
            }
        }
        let wait = waiting.elapsed();
        let logged = task.clone();
        let ending = Ending {
            status: 200,
            ok: true,
            task,
            changed: Some(changed),
            error: None,
        };
        let answer = self.finish(&workspace, ending, None).await;
        tracing::info!(
            workspace = %workspace.id,
            task = ?logged,
            ui_ms = millis(ui),
            release_ms = millis(release),
            wait_ms = millis(wait),
            total_ms = millis(began.elapsed()),
            "dev server stopped"
        );
        answer
    }

    // ---- reading ----

    /// The live workspace with this id, or the answer to give: 404 when it is not live.
    async fn workspace(&self, workspace_id: &str) -> Result<Workspace, WriteAnswer> {
        let id = workspace_id.to_owned();
        blocking(&self.reads, "dev.workspace", move |reads| {
            live_workspace(reads, &id)
        })
        .await?
        .ok_or_else(|| WriteAnswer::error(404, NO_WORKSPACE))
    }

    /// The state of the workspace's dev server; its `ps` listing is at most `max_age` old. The
    /// stored forwards are judged with `given` when there is one, else with a serve status at most
    /// `SERVE_STATUS_AGE` old.
    async fn state_of(
        &self,
        workspace: &Workspace,
        max_age: Duration,
        given: Option<&ServeStatus>,
    ) -> Result<DevServerState, WriteAnswer> {
        let run_configs = match workspace.repo_root.clone().filter(|root| !root.is_empty()) {
            Some(root) => pool("dev.run_configs", move || run_configs(Path::new(&root)))
                .await
                .ok_or_else(internal)?,
            None => Vec::new(),
        };
        let available = self.host().await.is_some();

        let mut targets = self.task_ports(workspace, max_age).await?;
        let stored: Vec<Forward> = self
            .table()
            .await
            .forwards
            .iter()
            .filter(|forward| forward.workspace_id == workspace.id)
            .cloned()
            .collect();
        for forward in &stored {
            if !targets.contains(&forward.target_port) {
                targets.push(forward.target_port);
            }
        }
        targets.truncate(MAX_FORWARDS);

        // One reading of the serve status judges every stored forward.
        let read = match given {
            None if !stored.is_empty() => self.serve_status_within(SERVE_STATUS_AGE).await.ok(),
            _ => None,
        };
        let status = given.or(read.as_ref());
        let mut forwards = Vec::with_capacity(targets.len());
        for port in targets {
            let running = tcp_open(port).await;
            let record = stored.iter().find(|forward| forward.target_port == port);
            let forwarded = match (record, status) {
                (Some(record), Some(status)) => still_valid(status, record).await,
                _ => false,
            };
            forwards.push(DevServerForward {
                name: format!("Port {port}"),
                port,
                running,
                forwarded,
                url: record.filter(|_| forwarded).map(url_of),
            });
        }

        let first = forwards.first();
        let first_forwarded = forwards.iter().find(|forward| forward.forwarded);
        Ok(DevServerState {
            available,
            running: first.is_some_and(|forward| forward.running),
            forwarded: first_forwarded.is_some(),
            port: first.map(|forward| forward.port),
            url: first_forwarded.and_then(|forward| forward.url.clone()),
            forwards,
            run_configs,
            task: None,
            error: (!available).then(|| NOT_CONNECTED.to_owned()),
        })
    }

    /// The answer of a start or a stop, with the workspace's state as it is now. `served` is the
    /// serve status of a start that forwarded its ports: the state is judged with it.
    async fn finish(
        &self,
        workspace: &Workspace,
        ending: Ending,
        served: Option<ServeStatus>,
    ) -> WriteAnswer {
        // A start or a stop has just pressed a task: no kept listing is younger than that.
        let state = self
            .state_of(workspace, Duration::ZERO, served.as_ref())
            .await;
        // A serve status may be kept again, after `serve` or `serve off` had cleared it: the one
        // that state read, or the one a start read for forwards that were all in place. It is
        // dropped here, so the first poll after a start or a stop reads a fresh one.
        *lock(&self.serve_status) = None;
        let Ending {
            status,
            ok,
            task,
            changed,
            error,
        } = ending;
        match state {
            Err(answer) => answer,
            Ok(mut state) => {
                state.task = task;
                if error.is_some() {
                    state.error = error;
                }
                result(status, ok, state, changed)
            }
        }
    }

    /// The ports the workspace's Run task listens on, by a `ps` listing at most `max_age` old;
    /// none without a worktree.
    async fn task_ports(
        &self,
        workspace: &Workspace,
        max_age: Duration,
    ) -> Result<Vec<u16>, WriteAnswer> {
        let Some(worktree) = workspace.worktree.clone() else {
            return Ok(Vec::new());
        };
        let processes = Arc::clone(&self.processes);
        let commands = Arc::clone(&self.commands);
        let home = self.home.clone();
        pool("dev.ports", move || {
            let listing = processes.listing(commands.as_ref(), max_age)?;
            Some(run_task_ports_in(
                &listing,
                commands.as_ref(),
                &home,
                &worktree,
            ))
        })
        .await
        .ok_or_else(internal)
        .map(Option::unwrap_or_default)
    }

    /// The Run task's ports once the first of them accepts connections; `None` when that did not
    /// happen within `ready_wait`.
    async fn wait_until_listening(
        &self,
        workspace: &Workspace,
    ) -> Result<Option<Vec<u16>>, WriteAnswer> {
        let deadline = Instant::now() + self.timings.ready_wait;
        loop {
            let ports = self.task_ports(workspace, Duration::ZERO).await?;
            if let Some(first) = ports.first() {
                if tcp_open(*first).await {
                    return Ok(Some(ports));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            tokio::time::sleep(self.timings.look.min(remaining)).await;
        }
    }

    // ---- the Run task ----

    /// Starts or stops the workspace's Run task with a job on the UI thread.
    async fn run_task(
        &self,
        priority: Priority,
        target: &Target,
        task: Option<String>,
        start: bool,
    ) -> Result<RunOutcome, RunFailure> {
        let target = target.clone();
        let job = move |driver: &mut dyn UiDriver| driver.run_task(&target, task.as_deref(), start);
        match self.ui.run(priority, job).await {
            Err(error @ UiRunError::Busy { waiting }) => {
                Err(RunFailure::Busy(busy(&error, waiting), error.to_string()))
            }
            Err(error @ UiRunError::Crashed) => Err(RunFailure::Failed(error.to_string())),
            Ok(Err(error)) => Err(RunFailure::Failed(error.to_string())),
            Ok(Ok(outcome)) => Ok(outcome),
        }
    }

    /// Stops a task this request started (`changed`) and says how that went: the end of the
    /// error's text, empty when nothing had been changed.
    async fn stop_again(&self, changed: bool, priority: Priority, target: &Target) -> String {
        if !changed {
            return String::new();
        }
        match self.run_task(priority, target, None, false).await {
            Ok(_) => STOPPED_AGAIN.to_owned(),
            Err(failure) => format!("; stopping it again also failed: {}", failure.text()),
        }
    }

    // ---- locks ----

    /// The workspace's lock: one start, stop, release or restore step at a time; a later one waits.
    async fn step(&self, workspace_id: &str) -> OwnedMutexGuard<()> {
        let step = Arc::clone(
            lock(&self.steps)
                .entry(workspace_id.to_owned())
                .or_default(),
        );
        step.lock_owned().await
    }

    /// The process-wide lock and what it guards, the stored forwards read from their file.
    async fn table(&self) -> MutexGuard<'_, Table> {
        let mut table = self.table.lock().await;
        if !table.loaded {
            let state_dir = self.state_dir.clone();
            if let Some(forwards) = pool("dev.forwards", move || store::load(&state_dir)).await {
                table.forwards = forwards;
                table.loaded = true;
            }
        }
        table
    }

    // ---- tailscale ----

    /// This Mac's tailnet name, kept for 60 seconds; `None` without a binary or an answer.
    async fn host(&self) -> Option<String> {
        let tailscale = Arc::clone(self.tailscale.as_ref()?);
        let kept = lock(&self.host)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < HOST_TTL)
            .map(|(_, host)| host.clone());
        if kept.is_some() {
            return kept;
        }
        let host = pool("tailscale status", move || tailscale.host())
            .await
            .flatten()?;
        *lock(&self.host) = Some((Instant::now(), host.clone()));
        Some(host)
    }

    /// Runs a call of the `tailscale` binary on the blocking pool.
    async fn tailscale<T, F>(&self, what: &'static str, call: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Tailscale) -> Result<T, String> + Send + 'static,
    {
        let Some(tailscale) = self.tailscale.clone() else {
            return Err(NOT_CONNECTED.to_owned());
        };
        pool(what, move || call(&tailscale))
            .await
            .unwrap_or_else(|| Err(format!("{what} did not finish")))
    }

    /// A fresh reading of the serve status; it is kept for `serve_status_within`.
    async fn serve_status(&self) -> Result<ServeStatus, String> {
        let status = self
            .tailscale("tailscale serve status", Tailscale::serve_status)
            .await?;
        *lock(&self.serve_status) = Some((Instant::now(), status.clone()));
        Ok(status)
    }

    /// The kept serve status when it is younger than `max_age`, else a fresh reading.
    async fn serve_status_within(&self, max_age: Duration) -> Result<ServeStatus, String> {
        let kept = lock(&self.serve_status)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < max_age)
            .map(|(_, status)| status.clone());
        match kept {
            Some(status) => Ok(status),
            None => self.serve_status().await,
        }
    }

    async fn serve(&self, serve_port: u16, bridge_port: u16) -> Result<(), String> {
        let served = self
            .tailscale("tailscale serve", move |tailscale| {
                tailscale.serve(serve_port, bridge_port)
            })
            .await;
        // Whatever it returned, the mappings may have changed.
        *lock(&self.serve_status) = None;
        served
    }

    async fn unserve(&self, serve_port: u16, bridge_port: u16) -> Result<bool, String> {
        let unserved = self
            .tailscale("tailscale serve off", move |tailscale| {
                tailscale.unserve(serve_port, bridge_port)
            })
            .await;
        *lock(&self.serve_status) = None;
        unserved
    }

    // ---- forwards ----

    /// Forwards every port of `ports`; the first failure ends it. The table lock is held from
    /// before the one fresh reading of the serve status the ports are chosen with until the last
    /// of them is done, so two starts never choose from readings that predate each other's
    /// `serve`. That status comes back with the mappings made here.
    async fn forward_all(&self, workspace_id: &str, ports: &[u16]) -> Result<ServeStatus, String> {
        // Without a tailnet name nothing is forwarded, and that is what the error says.
        self.host().await.ok_or_else(|| NOT_CONNECTED.to_owned())?;
        let mut table = self.table().await;
        let mut status = self.serve_status().await?;
        for port in ports {
            let others: BTreeSet<u16> = ports
                .iter()
                .copied()
                .filter(|other| other != port)
                .collect();
            self.forward_port(&mut table, &mut status, workspace_id, *port, &others)
                .await?;
        }
        Ok(status)
    }

    /// Publishes `port` of the workspace's dev server on the tailnet, unless it still is: on a
    /// serve port of the range of their own, never on `port` or one of the nine after it, and
    /// never on one something on this Mac already listens on. `others` are the other ports being
    /// forwarded now. The caller holds the table lock and read `status` under it; a mapping made
    /// here is recorded in it.
    async fn forward_port(
        &self,
        table: &mut Table,
        status: &mut ServeStatus,
        workspace_id: &str,
        port: u16,
        others: &BTreeSet<u16>,
    ) -> Result<(), String> {
        let host = self.host().await.ok_or_else(|| NOT_CONNECTED.to_owned())?;
        let key: Key = (workspace_id.to_owned(), port);
        if let Some(stored) = table.find(&key).cloned() {
            if still_valid(status, &stored).await {
                return Ok(());
            }
            self.release(table, &stored).await?;
            *status = self.serve_status().await?;
        }

        let bridge = Bridge::open(port)
            .await
            .map_err(|_| format!("couldn't open a bridge to port {port}"))?;
        let bridge_port = bridge.port();
        // `tailscale serve` knows nothing of the other listeners of this Mac: a chosen port one
        // of them holds joins the reserved ones, and the choice is made again.
        let mut reserved = others.clone();
        let mut serve_port = None;
        for _ in 0..SERVE_PORT_CHOICES {
            let Some(chosen) = choose_serve_port(status, port, &reserved) else {
                break;
            };
            if !tcp_open(chosen).await {
                serve_port = Some(chosen);
                break;
            }
            reserved.insert(chosen);
        }
        let Some(serve_port) = serve_port else {
            bridge.close().await;
            return Err(NO_SERVE_PORT.to_owned());
        };
        if let Err(error) = self.serve(serve_port, bridge_port).await {
            bridge.close().await;
            // A serve that failed half-way must not leave a mapping to a bridge that is gone.
            if let Err(error) = self.unserve(serve_port, bridge_port).await {
                tracing::warn!(%error, serve_port, "a failed forward may have left its mapping");
            }
            return Err(error);
        }
        // What `tailscale serve status` says of it now: the next port of this start does not
        // choose this serve port, and the final state finds the mapping.
        status.ports.insert(serve_port);
        status
            .proxies
            .insert(serve_port, format!("http://127.0.0.1:{bridge_port}"));

        table.forwards.push(Forward {
            workspace_id: workspace_id.to_owned(),
            target_port: port,
            serve_port,
            bridge_port,
            host,
            bridge_token: bridge.token().to_owned(),
        });
        table.bridges.insert(key, bridge);
        // The caller releases the forwards on a failure, this one included.
        self.save(table)
            .await
            .map_err(|_| format!("couldn't record the forward of port {port}"))
    }

    /// The target ports the workspace has a stored forward for.
    async fn forwarded_ports(&self, workspace_id: &str) -> BTreeSet<u16> {
        self.table()
            .await
            .forwards
            .iter()
            .filter(|forward| forward.workspace_id == workspace_id)
            .map(|forward| forward.target_port)
            .collect()
    }

    /// Releases every forward of the workspace; one that cannot be released is kept and logged.
    async fn release_workspace(&self, workspace_id: &str) {
        self.release_unless(workspace_id, &BTreeSet::new()).await;
    }

    /// Releases the forwards of the workspace except those of the target ports in `keep`.
    async fn release_unless(&self, workspace_id: &str, keep: &BTreeSet<u16>) {
        let mut table = self.table().await;
        let stored: Vec<Forward> = table
            .forwards
            .iter()
            .filter(|forward| forward.workspace_id == workspace_id)
            .filter(|forward| !keep.contains(&forward.target_port))
            .cloned()
            .collect();
        for forward in stored {
            if let Err(error) = self.release(&mut table, &forward).await {
                warn_kept(&error, &forward, "a forward could not be released");
            }
        }
    }

    /// Turns the forward's mapping off when it still is the relay's, closes its bridge and drops
    /// its record. When the mapping cannot be judged the bridge is closed all the same and the
    /// record is kept for a later try.
    async fn release(&self, table: &mut Table, forward: &Forward) -> Result<(), String> {
        let key = key_of(forward);
        let unserved = self.unserve(forward.serve_port, forward.bridge_port).await;
        if let Some(bridge) = table.bridges.remove(&key) {
            bridge.close().await;
        }
        unserved?;
        table.forget(&key);
        if let Err(error) = self.save(table).await {
            warn_kept(&error, forward, "a released forward is still on file");
        }
        Ok(())
    }

    /// Writes the stored forwards to their file.
    async fn save(&self, table: &Table) -> Result<(), String> {
        let state_dir = self.state_dir.clone();
        let forwards = table.forwards.clone();
        match pool("dev.forwards", move || store::save(&state_dir, &forwards)).await {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(error.to_string()),
            None => Err("the forwards were not saved".to_owned()),
        }
    }

    /// One stored forward after a restart; the workspace's lock is held.
    async fn restore_forward(&self, key: &Key) {
        let mut table = self.table().await;
        let Some(forward) = table.find(key).cloned() else {
            return;
        };
        let status = match self.serve_status().await {
            Ok(status) => status,
            Err(error) => return warn_kept(&error, &forward, "a forward was not restored"),
        };
        if !proxied(&status, &forward) {
            // The mapping is gone or someone else's: there is nothing left to rebuild.
            if let Some(bridge) = table.bridges.remove(key) {
                bridge.close().await;
            }
            table.forget(key);
            if let Err(error) = self.save(&table).await {
                warn_kept(&error, &forward, "a dropped forward is still on file");
            }
            return;
        }
        if !tcp_open(forward.target_port).await {
            if let Err(error) = self.release(&mut table, &forward).await {
                warn_kept(&error, &forward, "a forward could not be released");
            }
            return;
        }

        let bridge = match Bridge::open(forward.target_port).await {
            Ok(bridge) => bridge,
            Err(error) => {
                return warn_kept(&error.to_string(), &forward, "a forward was not restored")
            }
        };
        let mut served = self.serve(forward.serve_port, bridge.port()).await;
        if served.is_err() {
            // `tailscale` refuses to repoint a port that is in use: free it and serve again.
            served = match self.unserve(forward.serve_port, forward.bridge_port).await {
                Ok(_) => self.serve(forward.serve_port, bridge.port()).await,
                Err(error) => Err(error),
            };
        }
        if let Err(error) = served {
            bridge.close().await;
            return warn_kept(&error, &forward, "a forward was not restored");
        }

        if let Some(record) = table
            .forwards
            .iter_mut()
            .find(|record| key_of(record) == *key)
        {
            record.bridge_port = bridge.port();
            record.bridge_token = bridge.token().to_owned();
        }
        if let Some(old) = table.bridges.insert(key.clone(), bridge) {
            old.close().await;
        }
        if let Err(error) = self.save(&table).await {
            warn_kept(&error, &forward, "a restored forward was not saved");
        }
    }

    /// One stored forward in a pass of the sweeper; the workspace's lock is held.
    async fn sweep_forward(&self, key: &Key) {
        let Some(forward) = self.table().await.find(key).cloned() else {
            lock(&self.misses).remove(key);
            return;
        };
        if tcp_open(forward.target_port).await {
            lock(&self.misses).remove(key);
            return;
        }
        let misses = {
            let mut misses = lock(&self.misses);
            let count = misses.entry(key.clone()).or_insert(0);
            *count += 1;
            *count
        };
        if misses < SWEEP_MISSES {
            return;
        }
        let mut table = self.table().await;
        match self.release(&mut table, &forward).await {
            Ok(()) => {
                lock(&self.misses).remove(key);
            }
            // The count stays, so the next pass tries again.
            Err(error) => warn_kept(&error, &forward, "a forward could not be released"),
        }
    }
}

/// A duration in whole milliseconds, for the log.
fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// A poisoned lock holds consistent data: nothing panics while holding one.
fn lock<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Runs `work` on the blocking pool; `None`, logged, when it did not finish.
async fn pool<T, F>(what: &'static str, work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::error!(%error, what, "a dev-server job did not finish");
            None
        }
    }
}

/// Runs a start or a stop in a task of its own, like every write: it goes on when the caller
/// hangs up.
fn spawned(
    work: impl std::future::Future<Output = WriteAnswer> + Send + 'static,
) -> BoxFuture<WriteAnswer> {
    Box::pin(async move { tokio::spawn(work).await.unwrap_or_else(|_| internal()) })
}

fn warn_kept(error: &str, forward: &Forward, message: &'static str) {
    tracing::warn!(
        %error,
        workspace = %forward.workspace_id,
        port = forward.target_port,
        serve_port = forward.serve_port,
        "{message}"
    );
}

/// The live workspace with this id.
fn live_workspace(reads: &Reads, workspace_id: &str) -> Result<Option<Workspace>, ReadError> {
    Ok(reads
        .list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.id == workspace_id))
}

/// What a UI target needs of a live workspace, from the fields the live list has.
fn write_workspace(workspace: &Workspace) -> WriteWorkspace {
    WriteWorkspace {
        id: workspace.id.clone(),
        branch: workspace.branch.clone(),
        repo_name: workspace.repo_name.clone(),
        workspace_name: workspace.workspace_name.clone(),
        directory_name: workspace.directory_name.clone(),
    }
}

/// Whether `tailscale serve` proxies the forward's serve port to its bridge.
fn proxied(status: &ServeStatus, forward: &Forward) -> bool {
    status.proxies.get(&forward.serve_port).map(String::as_str)
        == Some(format!("http://127.0.0.1:{}", forward.bridge_port).as_str())
}

/// Whether the forward still works: its mapping is in place and its bridge answers as the relay's.
async fn still_valid(status: &ServeStatus, forward: &Forward) -> bool {
    proxied(status, forward) && bridge_matches(forward.bridge_port, &forward.bridge_token).await
}

/// `https://<host>:<serve port>/`, without the port when it is 443.
fn url_of(forward: &Forward) -> String {
    match forward.serve_port {
        443 => format!("https://{}/", forward.host),
        port => format!("https://{}:{port}/", forward.host),
    }
}

/// The body as JSON; one that does not serialize is the 500 answer.
fn answer(status: u16, body: &impl Serialize) -> WriteAnswer {
    match serde_json::to_value(body) {
        Ok(body) => WriteAnswer::json(status, body),
        Err(error) => {
            tracing::error!(%error, "a dev-server answer did not serialize");
            internal()
        }
    }
}

fn result(status: u16, ok: bool, state: DevServerState, changed: Option<bool>) -> WriteAnswer {
    answer(status, &DevServerResult { ok, state, changed })
}

/// `ok: false` with the state and `error`.
fn refused(status: u16, mut state: DevServerState, error: String) -> WriteAnswer {
    state.error = Some(error);
    result(status, false, state, None)
}
