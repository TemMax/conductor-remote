//! Prompts parked while the Mac is locked, sent after unlock.
//!
//! The lock screen hides Conductor's window from Accessibility, so a send that meets it hands its
//! prompt to this queue and answers at once. The queue keeps the prompt in the relay's store, so it
//! survives a restart, and a pump delivers it, oldest first, once the lock probe says the Mac is
//! unlocked. Time spent locked costs nothing; real failures with the Mac unlocked count, and after
//! `MAX_ATTEMPTS` of them the row turns `failed` and stays listed for the phone to show.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tokio::sync::Notify;

use super::BoxFuture;
use crate::agent::AgentPatch;
use crate::reads::receipts::DeliveryCursor;
use crate::state::store::{NewParked, ParkedRow, ParkedStatus, Store, StoreError};

/// Real delivery failures, with the Mac unlocked, before a row turns `failed`.
pub const MAX_ATTEMPTS: u32 = 3;
/// What a parked row waits for, in the words the chat shows under the bubble.
pub const PARKED_REASON: &str = "Sends when the Mac is unlocked";
/// The answer of a send that met the lock screen and was parked.
pub const PARKED_ERROR: &str =
    "The Mac is locked — the relay parked the prompt and will send it when the Mac is unlocked.";
/// Rows older than this are dropped when the queue starts, and again while the pump runs.
pub const KEEP_FAILED: Duration = Duration::from_secs(7 * 24 * 3600);
/// How often the pump prunes old rows while it runs.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// The error a delivery that panicked counts as.
const INTERNAL_ERROR: &str = "internal error";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParkedTimings {
    /// How often the lock is probed while rows wait.
    pub poll: Duration,
    /// The pause after a failed send that will be tried again.
    pub retry: Duration,
}

impl Default for ParkedTimings {
    fn default() -> ParkedTimings {
        ParkedTimings {
            poll: Duration::from_secs(5),
            retry: Duration::from_secs(5),
        }
    }
}

/// How one delivery of a parked row ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParkedOutcome {
    Delivered,
    /// The lock got in the way again: not counted as a failure.
    Locked,
    Failed(String),
}

pub type Deliverer = Arc<dyn Fn(ParkedRow) -> BoxFuture<ParkedOutcome> + Send + Sync>;
/// Called after a delivery (`None`) or the final failure (`Some(error)`).
pub type Notice = Arc<dyn Fn(&ParkedRow, Option<&str>) + Send + Sync>;
/// `Some(true)` locked, `Some(false)` unlocked, `None` unknown (try the send).
pub type LockProbe = Arc<dyn Fn() -> Option<bool> + Send + Sync>;

/// The parked prompts over the store, and the pump that sends them after unlock.
pub struct ParkedQueue {
    store: Arc<Store>,
    locked: LockProbe,
    timings: ParkedTimings,
    wake: Notify,
    started: AtomicBool,
}

impl ParkedQueue {
    pub fn new(store: Arc<Store>, locked: LockProbe, timings: ParkedTimings) -> Arc<ParkedQueue> {
        Arc::new(ParkedQueue {
            store,
            locked,
            timings,
            wake: Notify::new(),
            started: AtomicBool::new(false),
        })
    }

    /// Parks (or re-parks) the trimmed text with the cursor, then wakes the pump.
    pub fn park(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: &str,
        queue: bool,
        cursor: &DeliveryCursor,
        now_ms: i64,
    ) -> Result<ParkedRow, StoreError> {
        let row = self.insert(workspace_id, session_id, text, queue, cursor, now_ms)?;
        self.wake.notify_one();
        Ok(row)
    }

    /// `park` with the agent settings the prompt waits to apply (or none, which clears what a
    /// re-parked row held). The settings are stored before the pump is woken, so it never sees
    /// the row without them.
    #[allow(clippy::too_many_arguments)]
    pub fn park_with_agent(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: &str,
        queue: bool,
        cursor: &DeliveryCursor,
        now_ms: i64,
        agent: Option<&AgentPatch>,
    ) -> Result<ParkedRow, StoreError> {
        let row = self.insert(workspace_id, session_id, text, queue, cursor, now_ms)?;
        self.store
            .set_parked_agent(row.id, agent.map(AgentPatch::to_json).as_deref())?;
        self.wake.notify_one();
        Ok(row)
    }

    fn insert(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: &str,
        queue: bool,
        cursor: &DeliveryCursor,
        now_ms: i64,
    ) -> Result<ParkedRow, StoreError> {
        self.store.park(&NewParked {
            workspace_id: workspace_id.to_owned(),
            session_id: session_id.to_owned(),
            text: text.trim().to_owned(),
            queue,
            created_at_ms: now_ms,
            reason: PARKED_REASON.to_owned(),
            cursor_rowid: Some(cursor.rowid),
            cursor_outbox: cursor.outbox_ids.iter().cloned().collect(),
        })
    }

    /// Stores the agent settings of a parked row, or clears them.
    pub fn set_agent(&self, parked_id: i64, agent: Option<&AgentPatch>) {
        if let Err(error) = self
            .store
            .set_parked_agent(parked_id, agent.map(AgentPatch::to_json).as_deref())
        {
            tracing::error!(%error, "could not store the agent settings of a parked prompt");
        }
    }

    /// The agent settings a parked row waits to apply.
    pub fn agent(&self, parked_id: i64) -> Option<AgentPatch> {
        match self.store.parked_agent(parked_id) {
            Ok(agent) => agent.and_then(|json| AgentPatch::from_json(&json)),
            Err(error) => {
                tracing::error!(%error, "could not read the agent settings of a parked prompt");
                None
            }
        }
    }

    /// Every row, oldest first, failed ones included.
    pub fn list(&self) -> Vec<ParkedRow> {
        self.store.parked().unwrap_or_else(|error| {
            tracing::error!(%error, "could not read the parked prompts");
            Vec::new()
        })
    }

    /// Drops every row of a chat: the phone's Dismiss. Returns how many went.
    pub fn forget_session(&self, session_id: &str) -> usize {
        self.store
            .forget_parked_session(session_id)
            .unwrap_or_else(|error| {
                tracing::error!(%error, "could not forget the parked prompts of a chat");
                0
            })
    }

    /// Drops the row of this chat and text: it was sent by another path.
    pub fn forget_delivered(&self, session_id: &str, text: &str) -> usize {
        self.store
            .forget_parked_text(session_id, text.trim())
            .unwrap_or_else(|error| {
                tracing::error!(%error, "could not forget a delivered parked prompt");
                0
            })
    }

    /// Once: prunes rows older than `KEEP_FAILED`, then spawns the pump on the current tokio
    /// runtime. A second call does nothing.
    pub fn start(self: &Arc<Self>, deliver: Deliverer, notice: Notice, now_ms: i64) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.prune(now_ms);
        let queue = Arc::clone(self);
        tokio::spawn(async move { queue.pump(deliver, notice).await });
    }

    /// Drops the rows older than `KEEP_FAILED` as of `now_ms`.
    fn prune(&self, now_ms: i64) {
        let keep_ms = i64::try_from(KEEP_FAILED.as_millis()).unwrap_or(i64::MAX);
        match self.store.prune_parked(now_ms.saturating_sub(keep_ms)) {
            Ok(0) => {}
            Ok(pruned) => tracing::info!(pruned, "dropped parked prompts older than seven days"),
            Err(error) => tracing::error!(%error, "could not prune the parked prompts"),
        }
    }

    /// The waiting rows, oldest first.
    fn waiting(&self) -> Result<Vec<ParkedRow>, StoreError> {
        Ok(self
            .store
            .parked()?
            .into_iter()
            .filter(|row| row.status == ParkedStatus::Waiting)
            .collect())
    }

    /// Waits `poll`, or less when a park wakes the pump.
    async fn nap(&self) {
        tokio::select! {
            () = tokio::time::sleep(self.timings.poll) => {}
            () = self.wake.notified() => {}
        }
    }

    /// The pump. It never returns: the task ends with the runtime.
    async fn pump(&self, deliver: Deliverer, notice: Notice) {
        // `start` has just pruned.
        let mut last_prune = tokio::time::Instant::now();
        loop {
            if last_prune.elapsed() >= PRUNE_EVERY {
                self.prune(unix_ms());
                last_prune = tokio::time::Instant::now();
            }
            // 1. The waiting rows; none → sleep until a park wakes the pump, or until the next
            //    prune is due.
            let rows = match self.waiting() {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(%error, "could not read the parked prompts");
                    self.nap().await;
                    continue;
                }
            };
            if rows.is_empty() {
                tokio::select! {
                    () = tokio::time::sleep_until(last_prune + PRUNE_EVERY) => {}
                    () = self.wake.notified() => {}
                }
                continue;
            }
            // 2. Locked → probe again after `poll` or a wake.
            if (self.locked)() == Some(true) {
                self.nap().await;
                continue;
            }
            // 3. One pass over the rows, oldest first; the lock in the way ends it early.
            if self.pass(&rows, &deliver, &notice).await {
                self.nap().await;
                continue;
            }
            // 4. Rows still waiting → wait `poll` or a wake before the next pass.
            match self.waiting() {
                Ok(rows) if rows.is_empty() => {}
                _ => self.nap().await,
            }
        }
    }

    /// Delivers each row of `rows` that still waits. True when the lock got in the way.
    async fn pass(&self, rows: &[ParkedRow], deliver: &Deliverer, notice: &Notice) -> bool {
        for id in rows.iter().map(|row| row.id) {
            // Read again: an earlier delivery of this pass may have taken a while, and the row
            // may have been dismissed or re-parked meanwhile.
            let row = match self.waiting() {
                Ok(rows) => rows.into_iter().find(|row| row.id == id),
                Err(error) => {
                    tracing::error!(%error, "could not read the parked prompts");
                    return false;
                }
            };
            let Some(row) = row else { continue };
            match attempt(deliver, row.clone()).await {
                ParkedOutcome::Delivered => {
                    if let Err(error) = self.store.remove_parked(row.id) {
                        tracing::error!(%error, "could not remove a delivered parked prompt");
                    }
                    notice(&row, None);
                }
                ParkedOutcome::Locked => return true,
                ParkedOutcome::Failed(error) => {
                    tracing::warn!(session_id = %row.session_id, %error, "a parked prompt did not send");
                    match self
                        .store
                        .record_parked_failure(row.id, &error, MAX_ATTEMPTS)
                    {
                        Ok(Some(failed)) if failed.status == ParkedStatus::Failed => {
                            notice(&failed, Some(&error));
                        }
                        Ok(Some(_)) => tokio::time::sleep(self.timings.retry).await,
                        // Dismissed while it was being delivered: it stays gone.
                        Ok(None) => {}
                        Err(error) => {
                            tracing::error!(%error, "could not record a parked prompt's failure");
                            tokio::time::sleep(self.timings.retry).await;
                        }
                    }
                }
            }
        }
        false
    }
}

/// The wall clock in milliseconds since the epoch.
fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Runs one delivery in a task of its own, so a panic in it surfaces as a join error and counts
/// as a failure instead of ending the pump.
async fn attempt(deliver: &Deliverer, row: ParkedRow) -> ParkedOutcome {
    let deliver = Arc::clone(deliver);
    match tokio::spawn(async move { deliver(row).await }).await {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::error!(%error, "a parked prompt's delivery panicked");
            ParkedOutcome::Failed(INTERNAL_ERROR.to_owned())
        }
    }
}

/// The phone's `ParkedPrompt`: `{workspaceId, sessionId, text, queue (only when true), status,
/// attempts, createdAt, reason, error (only when set)}`.
pub fn parked_json(row: &ParkedRow) -> Value {
    let mut map = Map::new();
    map.insert("workspaceId".into(), Value::from(row.workspace_id.as_str()));
    map.insert("sessionId".into(), Value::from(row.session_id.as_str()));
    map.insert("text".into(), Value::from(row.text.as_str()));
    if row.queue {
        map.insert("queue".into(), Value::Bool(true));
    }
    let status = match row.status {
        ParkedStatus::Waiting => "waiting",
        ParkedStatus::Failed => "failed",
    };
    map.insert("status".into(), Value::from(status));
    map.insert("attempts".into(), Value::from(row.attempts));
    map.insert("createdAt".into(), Value::from(row.created_at_ms));
    map.insert("reason".into(), Value::from(row.reason.as_str()));
    if let Some(error) = &row.error {
        map.insert("error".into(), Value::from(error.as_str()));
    }
    Value::Object(map)
}

/// `parked_json(row)` plus, when there are agent settings, `agent`: the patch as an object.
pub fn parked_json_with(row: &ParkedRow, agent: Option<&AgentPatch>) -> Value {
    let mut json = parked_json(row);
    if let (Some(agent), Some(map)) = (agent, json.as_object_mut()) {
        let patch = serde_json::to_value(agent).unwrap_or_else(|_| Value::Object(Map::new()));
        map.insert("agent".into(), patch);
    }
    json
}

/// The persisted cursor, when one was stored.
pub fn cursor_of(row: &ParkedRow) -> Option<DeliveryCursor> {
    row.cursor_rowid.map(|rowid| DeliveryCursor {
        rowid,
        outbox_ids: row.cursor_outbox.iter().cloned().collect(),
    })
}
