//! The first prompts of new workspaces, waiting to be sent.
//!
//! Conductor's link creates a workspace and pre-fills its composer but never presses Return, so the
//! relay sends the first prompt itself once the workspace's chat exists. The entry lives in the
//! relay's store, so it survives a restart, and the phone only watches it (`/api/state`) and may
//! dismiss it.
//!
//! The send is tried while the workspace is still setting up: Conductor takes a first message
//! that early, and waiting for `ready` costs minutes. An early send that does not land spends a
//! small budget of its own and never fails the entry; only failures after `ready` count, and after
//! `MAX_ATTEMPTS` of them (or a workspace that never becomes sendable within `MAX_AGE`) the entry
//! turns `failed` and stays listed, so the phone can show the text beside the reason.
//!
//! Spacing between tries is read off time stamps, never slept inside a step, so one slow entry
//! does not hold up its siblings. A locked Mac freezes every entry: no attempt is spent and the
//! entry does not age.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use rusqlite::OptionalExtension;
use serde_json::{json, Map, Value};
use tokio::sync::Notify;

use super::agent::{apply_agent, AgentError};
use super::deliver::send_budget;
use super::parked::LockProbe;
use super::service::{
    chat_target, deliver_text, locate_checkout, now_ms, workspace_and_chats, Inner, NOT_A_TAB,
    WORKSPACE_GONE,
};
use super::{BoxFuture, WriteAnswer};
use crate::agent::AgentPatch;
use crate::contract::Priority;
use crate::files::attachments::{discard_staged, materialize, prune_staged, staged_attachments};
use crate::reads::{ReadError, Reads};
use crate::state::store::{FirstPromptRow, NewFirstPrompt, ParkedStatus, Store, StoreError};

/// How often the pump steps every waiting entry.
pub const POLL: Duration = Duration::from_secs(1);
/// The spacing between two counted sends (the workspace is ready).
pub const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Counted sends, after the workspace turned ready, before the entry fails.
pub const MAX_ATTEMPTS: u32 = 3;
/// Sends tried while the workspace is still setting up. They never fail the entry.
pub const MAX_EARLY_ATTEMPTS: u32 = 2;
/// The spacing between two early sends: long enough for Conductor to have drawn the chat.
pub const EARLY_RETRY_DELAY: Duration = Duration::from_secs(20);
/// A workspace that has not become sendable in this long is not going to.
pub const MAX_AGE: Duration = Duration::from_secs(15 * 60);
/// Entries, and staged attachment directories nothing refers to, are dropped after this.
pub const KEEP_FAILED: Duration = Duration::from_secs(7 * 24 * 3600);
/// How often the pump prunes while it runs.
pub const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// The staging root of attachments picked before their workspace exists, under the state directory.
pub const STAGING_DIR: &str = "attachment-staging";
/// The relay's meta key of the synced phone preferences (their drafts refer to staged files).
const PREFS_KEY: &str = "prefs";

/// Why an entry of a workspace that never became sendable failed.
pub const NEVER_SET_UP: &str = "the workspace never finished setting up";
/// Why an entry whose staged files are gone failed.
pub const ATTACHMENT_GONE: &str = "an attached file is no longer available";
/// A send that failed without saying why.
const DID_NOT_LAND: &str = "the send didn\u{2019}t land";
const NO_PENDING_PROMPT: &str = "no pending prompt";
/// The error of a send whose task panicked, or of a relay that is shutting down.
const INTERNAL_ERROR: &str = "internal error";
/// The client timeout the send's budget is computed from: `send_budget` makes it 55 s.
const SEND_CLIENT_TIMEOUT_MS: u64 = 60_000;

/// How far Conductor has got with the workspace: only `Ready` means the worktree is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    SettingUp,
    Ready,
}

/// What Conductor's database says about the workspace of an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstPromptTarget {
    pub phase: Phase,
    /// The active chat when it is one of the open chats, else the first open chat.
    pub session_id: Option<String>,
    /// That chat has a `last_user_message_at`: the prompt went already (from the Mac, or by a send
    /// that landed before a restart).
    pub already_sent: bool,
    /// `None` until the worktree exists.
    pub worktree: Option<PathBuf>,
}

/// How one send of a first prompt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Delivered,
    /// The lock screen got in the way: the attempt is handed back.
    Locked,
    Failed(String),
}

/// What the queue needs from Conductor: its database and its window. The relay's own backend reads
/// the database through the writes' reads on the blocking pool and sends through the write
/// service; tests pass fakes.
pub trait FirstPromptBackend: Send + Sync + 'static {
    /// The live workspace's target, `Ok(None)` when there is no live row (yet); `Err` is a failed
    /// read, and the entry waits.
    fn inspect(&self, workspace_id: &str) -> BoxFuture<Result<Option<FirstPromptTarget>, String>>;
    /// Copies the staged files of `attachment_ids` into `worktree`.
    fn materialize(
        &self,
        worktree: PathBuf,
        attachment_ids: Vec<String>,
    ) -> BoxFuture<Result<(), String>>;
    /// Sends `text` (trimmed, not empty) into the chat.
    fn send(&self, workspace_id: &str, session_id: &str, text: &str) -> BoxFuture<SendOutcome>;
    /// True once the backend can do nothing more (the relay is shutting down): the pump ends.
    fn closed(&self) -> bool {
        false
    }
}

/// Milliseconds since the Unix epoch, as the queue reads the time.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The pending first prompts over the relay's store, and the pump that sends them.
pub struct FirstPromptQueue {
    store: Arc<Store>,
    staging: PathBuf,
    locked: LockProbe,
    backend: Arc<dyn FirstPromptBackend>,
    clock: Clock,
    wake: Notify,
    started: AtomicBool,
}

impl FirstPromptQueue {
    /// A queue over `store`; staged attachments live under `<state_dir>/attachment-staging`.
    pub fn new(
        store: Arc<Store>,
        state_dir: &Path,
        locked: LockProbe,
        backend: Arc<dyn FirstPromptBackend>,
        clock: Clock,
    ) -> Arc<FirstPromptQueue> {
        Arc::new(FirstPromptQueue {
            store,
            staging: staging_root(state_dir),
            locked,
            backend,
            clock,
            wake: Notify::new(),
            started: AtomicBool::new(false),
        })
    }

    /// Queues the first prompt of a workspace, replacing any entry it had, and wakes the pump.
    pub fn enqueue(
        &self,
        workspace_id: &str,
        text: &str,
        send_immediately: bool,
        attachment_ids: Vec<String>,
        now_ms: i64,
    ) -> Result<FirstPromptRow, StoreError> {
        let row = self.store.upsert_first_prompt(&NewFirstPrompt {
            workspace_id: workspace_id.to_owned(),
            text: text.to_owned(),
            send_immediately,
            attachment_ids,
            created_at_ms: now_ms,
        })?;
        // No stored permit: the pump registers before it reads the entries, so a wake is never
        // lost, and an entry it has already seen does not cost a second pass.
        self.wake.notify_waiters();
        Ok(row)
    }

    /// Every entry, oldest first, failed ones included.
    pub fn list(&self) -> Vec<FirstPromptRow> {
        list(&self.store)
    }

    /// Every entry as the phone's `FirstPrompt` JSON.
    pub fn pending_prompts(&self) -> Vec<Value> {
        self.list().iter().map(first_prompt_json).collect()
    }

    /// Drops the entry of a workspace and its staged copies: dismissed from the phone, or sent by
    /// hand. False when there was none.
    pub fn forget(&self, workspace_id: &str) -> bool {
        forget_entry(&self.store, &self.staging, workspace_id)
    }

    /// Once: prunes, then spawns the pump on the current tokio runtime. A second call does
    /// nothing.
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.prune();
        let waiting = self.waiting().len();
        if waiting > 0 {
            tracing::info!(waiting, "resuming undelivered first prompts");
        }
        let queue = Arc::clone(self);
        tokio::spawn(async move { queue.pump().await });
    }

    /// Drops the entries older than `KEEP_FAILED`, then the staged attachment directories older
    /// than that which no entry and no synced draft names.
    fn prune(&self) {
        let keep_ms = i64::try_from(KEEP_FAILED.as_millis()).unwrap_or(i64::MAX);
        let before = (self.clock)().saturating_sub(keep_ms);
        match self.store.prune_first_prompts(before) {
            Ok(0) => {}
            Ok(pruned) => tracing::info!(pruned, "dropped first prompts older than seven days"),
            Err(error) => tracing::error!(%error, "could not prune the first prompts"),
        }
        let Some(keep) = self.referenced_staged() else {
            return;
        };
        let removed = prune_staged(&self.staging, KEEP_FAILED, &keep);
        if removed > 0 {
            tracing::info!(removed, "removed abandoned staged attachments");
        }
    }

    /// Every staged id an entry or a synced draft still names; `None` when that cannot be known,
    /// and then nothing staged is pruned.
    fn referenced_staged(&self) -> Option<HashSet<String>> {
        let entries = match self.store.first_prompts() {
            Ok(entries) => entries,
            Err(error) => {
                tracing::error!(%error, "could not read the first prompts to prune staged files");
                return None;
            }
        };
        let mut keep: HashSet<String> = entries
            .into_iter()
            .flat_map(|entry| entry.attachment_ids)
            .collect();
        let prefs = match self.store.meta(PREFS_KEY) {
            Ok(prefs) => prefs,
            Err(error) => {
                tracing::error!(%error, "could not read the drafts to prune staged files");
                return None;
            }
        };
        if let Some(prefs) = prefs {
            let prefs: Value = match serde_json::from_str(&prefs) {
                Ok(prefs) => prefs,
                Err(error) => {
                    tracing::error!(%error, "the synced drafts are unreadable; staged files kept");
                    return None;
                }
            };
            keep.extend(draft_stage_ids(&prefs));
        }
        Some(keep)
    }

    /// The waiting entries, oldest first.
    fn waiting(&self) -> Vec<FirstPromptRow> {
        self.list()
            .into_iter()
            .filter(|entry| entry.status == ParkedStatus::Waiting)
            .collect()
    }

    /// The entry as it is now, when it is still this entry (neither dismissed nor replaced) and
    /// still waiting.
    fn current(&self, entry: &FirstPromptRow) -> Option<FirstPromptRow> {
        match self.store.first_prompt(&entry.workspace_id) {
            Ok(Some(now))
                if now.created_at_ms == entry.created_at_ms
                    && now.status == ParkedStatus::Waiting =>
            {
                Some(now)
            }
            Ok(_) => None,
            Err(error) => {
                tracing::error!(%error, "could not read a first prompt");
                None
            }
        }
    }

    /// The pump. It ends only when the backend is closed.
    async fn pump(&self) {
        // `start` has just pruned.
        let mut last_prune = tokio::time::Instant::now();
        loop {
            if self.backend.closed() {
                return;
            }
            if last_prune.elapsed() >= PRUNE_EVERY {
                self.prune();
                last_prune = tokio::time::Instant::now();
            }
            // Registered before the entries are read: an enqueue from here on wakes the nap below.
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            let waiting = self.waiting();
            if waiting.is_empty() {
                // Nothing waits: Conductor's database is not read until an entry arrives.
                tokio::select! {
                    () = tokio::time::sleep_until(last_prune + PRUNE_EVERY) => {}
                    () = wake => {}
                }
                continue;
            }
            for entry in &waiting {
                // Read again: an earlier step of this pass may have taken a while.
                if let Some(entry) = self.current(entry) {
                    self.step(entry).await;
                }
            }
            tokio::select! {
                () = tokio::time::sleep(POLL) => {}
                () = wake => {}
            }
        }
    }

    /// One look at one waiting entry.
    async fn step(&self, entry: FirstPromptRow) {
        // 1. A locked Mac freezes the entry whole: no attempt, no ageing, no read.
        if (self.locked)() == Some(true) {
            return;
        }
        // 2. The workspace as Conductor's database has it; no live row yet is normal.
        let target = match self.backend.inspect(&entry.workspace_id).await {
            Ok(target) => target,
            Err(error) => {
                tracing::error!(workspace_id = %entry.workspace_id, %error, "could not read a first prompt's workspace");
                return;
            }
        };
        // 3. Not sendable for too long: give up in public.
        let now = (self.clock)();
        let sendable = target
            .as_ref()
            .is_some_and(|target| target.phase == Phase::Ready && target.session_id.is_some());
        if !sendable && now.saturating_sub(entry.created_at_ms) > millis(MAX_AGE) {
            self.fail(&entry, NEVER_SET_UP);
            return;
        }
        // 4. Staged files go into the worktree before the text that refers to them, then their
        //    staged copies go.
        if !entry.attachment_ids.is_empty() {
            let Some(worktree) = target.as_ref().and_then(|target| target.worktree.clone()) else {
                return;
            };
            let ids = entry.attachment_ids.clone();
            if let Err(error) = self.backend.materialize(worktree, ids.clone()).await {
                self.fail(&entry, &error);
                return;
            }
            if let Err(error) = self
                .store
                .clear_first_prompt_attachments(&entry.workspace_id)
            {
                tracing::error!(%error, "could not clear a first prompt's attachments");
                return;
            }
            self.discard(&ids);
        }
        // 5. No chat yet: wait.
        let Some(target) = target else { return };
        let Some(session_id) = target.session_id.clone() else {
            return;
        };
        // 6. The chat has a user message already: sent from the Mac, or before a restart.
        if target.already_sent {
            self.delivered(&entry);
            return;
        }
        // 7. Early (setting up): only when asked to, spaced `EARLY_RETRY_DELAY`, at most
        //    `MAX_EARLY_ATTEMPTS`; ready: spaced `RETRY_DELAY`.
        let early = target.phase != Phase::Ready;
        if early && !entry.send_immediately {
            return;
        }
        let spacing = if early {
            EARLY_RETRY_DELAY
        } else {
            RETRY_DELAY
        };
        if entry
            .last_attempt_at_ms
            .is_some_and(|last| now.saturating_sub(last) < millis(spacing))
        {
            return;
        }
        if early && entry.early_attempts >= MAX_EARLY_ATTEMPTS {
            return;
        }
        // 8. The send. A text with nothing in it counts as delivered.
        let text = entry.text.trim();
        let outcome = if text.is_empty() {
            SendOutcome::Delivered
        } else {
            self.send(&entry.workspace_id, &session_id, text).await
        };
        // 9. Delivered → gone; locked → handed back (not counted); failed → counted, and the
        //    `MAX_ATTEMPTS`th counted failure fails the entry with its error.
        match outcome {
            SendOutcome::Delivered => self.delivered(&entry),
            SendOutcome::Locked => {}
            SendOutcome::Failed(error) => self.failed_attempt(&entry, early, &error),
        }
    }

    /// One send in a task of its own, so a panic in it counts as a failure instead of ending the
    /// pump.
    async fn send(&self, workspace_id: &str, session_id: &str, text: &str) -> SendOutcome {
        let send = self.backend.send(workspace_id, session_id, text);
        match tokio::spawn(send).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(%error, "a first prompt's send panicked");
                SendOutcome::Failed(INTERNAL_ERROR.to_owned())
            }
        }
    }

    /// Counts a send that did not land, and fails the entry on the last counted one.
    fn failed_attempt(&self, entry: &FirstPromptRow, early: bool, error: &str) {
        let error = if error.is_empty() {
            DID_NOT_LAND
        } else {
            error
        };
        // Dismissed or replaced while it was being sent: the new state stands.
        if self.current(entry).is_none() {
            return;
        }
        let at = (self.clock)();
        match self
            .store
            .record_first_prompt_attempt(&entry.workspace_id, early, at)
        {
            Ok(Some(row)) if early => {
                tracing::info!(workspace_id = %row.workspace_id, %error, "a first prompt did not land during setup; waiting");
            }
            Ok(Some(row)) => {
                tracing::warn!(workspace_id = %row.workspace_id, attempts = row.attempts, %error, "a first prompt did not send");
                if row.attempts >= MAX_ATTEMPTS {
                    self.fail(&row, error);
                }
            }
            Ok(None) => {}
            Err(error) => tracing::error!(%error, "could not record a first prompt's attempt"),
        }
    }

    /// Delivered: the entry's job is done, so it stops existing.
    fn delivered(&self, entry: &FirstPromptRow) {
        if self.current(entry).is_none() {
            return;
        }
        match self.store.remove_first_prompt(&entry.workspace_id) {
            Ok(_) => self.discard(&entry.attachment_ids),
            Err(error) => tracing::error!(%error, "could not remove a delivered first prompt"),
        }
    }

    /// Given up on: kept, so the phone can show the text and the reason.
    fn fail(&self, entry: &FirstPromptRow, error: &str) {
        tracing::warn!(workspace_id = %entry.workspace_id, %error, "a first prompt failed");
        if let Err(error) = self.store.fail_first_prompt(&entry.workspace_id, error) {
            tracing::error!(%error, "could not mark a first prompt failed");
        }
    }

    fn discard(&self, attachment_ids: &[String]) {
        discard_ids(&self.staging, attachment_ids);
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// `<state_dir>/attachment-staging`.
pub fn staging_root(state_dir: &Path) -> PathBuf {
    state_dir.join(STAGING_DIR)
}

/// Every `stageId` of `drafts.<key>.attachments[]`, over every draft of the synced preferences.
fn draft_stage_ids(prefs: &Value) -> Vec<String> {
    let Some(drafts) = prefs.get("drafts").and_then(Value::as_object) else {
        return Vec::new();
    };
    drafts
        .values()
        .filter_map(|draft| draft.get("attachments").and_then(Value::as_array))
        .flatten()
        .filter_map(|attachment| attachment.get("stageId").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// Copies the staged files of `attachment_ids` from `staging_root` into `worktree`. Blocking.
pub fn materialize_staged(
    staging_root: &Path,
    worktree: &Path,
    attachment_ids: &[String],
) -> Result<(), String> {
    let staged = staged_attachments(staging_root, attachment_ids)
        .ok_or_else(|| ATTACHMENT_GONE.to_owned())?;
    materialize(&staged, staging_root, worktree)
}

/// The live workspace: its `state` and its active chat.
const WORKSPACE_SQL: &str = "\
SELECT state, active_session_id
 FROM workspaces
 WHERE id = ?1 AND state IN ('ready', 'setting_up')
 LIMIT 1";

/// The open chats of a workspace in tab order, with the time of their last user message.
const CHATS_SQL: &str = "\
SELECT id, last_user_message_at
 FROM sessions
 WHERE workspace_id = ?1 AND COALESCE(is_hidden, 0) = 0
 ORDER BY created_at ASC";

/// The target of a first prompt as Conductor's database has it: the live workspace's `state`, its
/// active chat when that is open (else the first open chat), that chat's `last_user_message_at`,
/// and the worktree. `None` when the workspace is not live. Blocking.
pub fn inspect_target(
    reads: &Reads,
    workspace_id: &str,
) -> Result<Option<FirstPromptTarget>, ReadError> {
    type Chat = (String, Option<String>);
    type Found = (Option<String>, Option<String>, Vec<Chat>);
    let found: Option<Found> = reads.db().read("firstprompt.target", |conn| {
        let workspace: Option<(Option<String>, Option<String>)> = conn
            .query_row(WORKSPACE_SQL, [workspace_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let Some((state, active)) = workspace else {
            return Ok(None);
        };
        let mut stmt = conn.prepare(CHATS_SQL)?;
        let chats = stmt
            .query_map([workspace_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<Chat>>>()?;
        Ok(Some((state, active, chats)))
    })?;
    let Some((state, active, chats)) = found else {
        return Ok(None);
    };
    let chat = chats
        .iter()
        .find(|(id, _)| active.as_deref() == Some(id.as_str()))
        .or_else(|| chats.first());
    let worktree = locate_checkout(reads, workspace_id)?.and_then(|checkout| checkout.worktree);
    Ok(Some(FirstPromptTarget {
        phase: if state.as_deref() == Some("ready") {
            Phase::Ready
        } else {
            Phase::SettingUp
        },
        session_id: chat.map(|(id, _)| id.clone()),
        already_sent: chat
            .and_then(|(_, at)| at.as_deref())
            .is_some_and(|at| !at.is_empty()),
        worktree,
    }))
}

/// The phone's `FirstPrompt`: `{workspaceId, text, status, attempts, earlyAttempts,
/// sendImmediately, attachmentIds, createdAt, lastAttemptAt (only when set), error (only when
/// set)}`.
pub fn first_prompt_json(row: &FirstPromptRow) -> Value {
    let mut map = Map::new();
    map.insert("workspaceId".into(), Value::from(row.workspace_id.as_str()));
    map.insert("text".into(), Value::from(row.text.as_str()));
    let status = match row.status {
        ParkedStatus::Waiting => "waiting",
        ParkedStatus::Failed => "failed",
    };
    map.insert("status".into(), Value::from(status));
    map.insert("attempts".into(), Value::from(row.attempts));
    map.insert("earlyAttempts".into(), Value::from(row.early_attempts));
    map.insert("sendImmediately".into(), Value::Bool(row.send_immediately));
    map.insert("attachmentIds".into(), json!(row.attachment_ids));
    map.insert("createdAt".into(), Value::from(row.created_at_ms));
    if let Some(at) = row.last_attempt_at_ms {
        map.insert("lastAttemptAt".into(), Value::from(at));
    }
    if let Some(error) = &row.error {
        map.insert("error".into(), Value::from(error.as_str()));
    }
    Value::Object(map)
}

fn list(store: &Store) -> Vec<FirstPromptRow> {
    store.first_prompts().unwrap_or_else(|error| {
        tracing::error!(%error, "could not read the first prompts");
        Vec::new()
    })
}

fn discard_ids(staging: &Path, attachment_ids: &[String]) {
    for id in attachment_ids {
        discard_staged(staging, id);
    }
}

/// Removes the entry of `workspace_id` and its staged copies; false when there was none.
fn forget_entry(store: &Store, staging: &Path, workspace_id: &str) -> bool {
    let entry = match store.first_prompt(workspace_id) {
        Ok(entry) => entry,
        Err(error) => {
            tracing::error!(%error, "could not read a first prompt to forget it");
            return false;
        }
    };
    match store.remove_first_prompt(workspace_id) {
        Ok(true) => {
            if let Some(entry) = entry {
                discard_ids(staging, &entry.attachment_ids);
            }
            true
        }
        Ok(false) => false,
        Err(error) => {
            tracing::error!(%error, "could not forget a first prompt");
            false
        }
    }
}

/// The relay's backend: Conductor's database through `inner.reads` on the blocking pool, and the
/// send loop of the write service at Background priority. It holds the writes weakly, so the queue
/// never keeps them alive.
struct InnerBackend {
    inner: Weak<Inner>,
    staging: PathBuf,
}

impl FirstPromptBackend for InnerBackend {
    fn inspect(&self, workspace_id: &str) -> BoxFuture<Result<Option<FirstPromptTarget>, String>> {
        let inner = Weak::clone(&self.inner);
        let workspace_id = workspace_id.to_owned();
        Box::pin(async move {
            let Some(inner) = inner.upgrade() else {
                return Err(INTERNAL_ERROR.to_owned());
            };
            let reads = Arc::clone(&inner.reads);
            drop(inner);
            match tokio::task::spawn_blocking(move || inspect_target(&reads, &workspace_id)).await {
                Ok(Ok(target)) => Ok(target),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            }
        })
    }

    fn materialize(
        &self,
        worktree: PathBuf,
        attachment_ids: Vec<String>,
    ) -> BoxFuture<Result<(), String>> {
        let staging = self.staging.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                materialize_staged(&staging, &worktree, &attachment_ids)
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()))
        })
    }

    fn send(&self, workspace_id: &str, session_id: &str, text: &str) -> BoxFuture<SendOutcome> {
        let inner = Weak::clone(&self.inner);
        let workspace_id = workspace_id.to_owned();
        let session_id = session_id.to_owned();
        let text = text.to_owned();
        Box::pin(async move {
            let Some(inner) = inner.upgrade() else {
                return SendOutcome::Failed(INTERNAL_ERROR.to_owned());
            };
            send_first_prompt(&inner, workspace_id, session_id, text).await
        })
    }

    fn closed(&self) -> bool {
        self.inner.strong_count() == 0
    }
}

/// One send of a first prompt through the send loop, at Background priority.
async fn send_first_prompt(
    inner: &Arc<Inner>,
    workspace_id: String,
    session_id: String,
    text: String,
) -> SendOutcome {
    let agent_workspace_id = workspace_id.clone();
    let located = {
        let reads = Arc::clone(&inner.reads);
        let session_id = session_id.clone();
        tokio::task::spawn_blocking(move || {
            workspace_and_chats(&reads, Some(&workspace_id), Some(&session_id))
        })
        .await
    };
    let (workspace, chats) = match located {
        Ok(Ok(Some(found))) => found,
        Ok(Ok(None)) => return SendOutcome::Failed(WORKSPACE_GONE.to_owned()),
        Ok(Err(error)) => return SendOutcome::Failed(error.to_string()),
        Err(error) => return SendOutcome::Failed(error.to_string()),
    };
    let Some((mut target, _)) = chat_target(&workspace, &chats, &session_id) else {
        return SendOutcome::Failed(NOT_A_TAB.to_owned());
    };
    let mut session_id = session_id;

    // The agent chosen with the prompt goes onto the chat first; it is cleared once applied, so a
    // retried send does not apply it again.
    if let Some(patch) = stored_agent(inner, &agent_workspace_id) {
        match apply_agent(
            inner,
            &session_id,
            Some(&agent_workspace_id),
            &patch,
            Priority::Background,
        )
        .await
        {
            Err(AgentError::Locked(_)) => return SendOutcome::Locked,
            Err(AgentError::Answer(answer)) => {
                return SendOutcome::Failed(
                    answer.body["error"]
                        .as_str()
                        .unwrap_or(INTERNAL_ERROR)
                        .to_owned(),
                );
            }
            Ok(applied) => {
                // Only while the chat stayed the same: when Conductor opened another one, the patch
                // stays so a retry asks for the model again (the memo sends it to the same new
                // chat), and the entry's removal on delivery removes it.
                if applied.session_id == session_id {
                    if let Some(deps) = inner.deps.as_ref() {
                        if let Err(error) =
                            deps.store.set_first_prompt_agent(&agent_workspace_id, None)
                        {
                            tracing::error!(
                                %error,
                                "could not clear a first prompt's agent settings"
                            );
                        }
                    }
                } else {
                    // Conductor opened another chat for the model: the prompt goes there.
                    let reads = Arc::clone(&inner.reads);
                    let workspace_id = agent_workspace_id.clone();
                    let chat_id = applied.session_id.clone();
                    let fresh = tokio::task::spawn_blocking(move || {
                        workspace_and_chats(&reads, Some(&workspace_id), Some(&chat_id))
                    })
                    .await;
                    let (workspace, chats) = match fresh {
                        Ok(Ok(Some(found))) => found,
                        Ok(Ok(None)) => return SendOutcome::Failed(WORKSPACE_GONE.to_owned()),
                        Ok(Err(error)) => return SendOutcome::Failed(error.to_string()),
                        Err(error) => return SendOutcome::Failed(error.to_string()),
                    };
                    let Some((opened, _)) = chat_target(&workspace, &chats, &applied.session_id)
                    else {
                        return SendOutcome::Failed(NOT_A_TAB.to_owned());
                    };
                    target = opened;
                    session_id = applied.session_id;
                }
            }
        }
    }

    let budget = inner
        .timings
        .send_budget
        .unwrap_or_else(|| send_budget(Some(SEND_CLIENT_TIMEOUT_MS)));
    match deliver_text(
        inner,
        target,
        &session_id,
        &text,
        false,
        Priority::Background,
        None,
        budget,
    )
    .await
    {
        Err(answer) => SendOutcome::Failed(
            answer.body["error"]
                .as_str()
                .unwrap_or(INTERNAL_ERROR)
                .to_owned(),
        ),
        Ok(delivery) if delivery.receipt.is_some() => SendOutcome::Delivered,
        Ok(delivery) if delivery.locked => SendOutcome::Locked,
        Ok(delivery) => SendOutcome::Failed(delivery.error.unwrap_or_default()),
    }
}

/// The agent settings kept for a workspace's first prompt; a store error or unreadable text counts
/// as none.
fn stored_agent(inner: &Arc<Inner>, workspace_id: &str) -> Option<AgentPatch> {
    let deps = inner.deps.as_ref()?;
    match deps.store.first_prompt_agent(workspace_id) {
        Ok(text) => text.and_then(|text| AgentPatch::from_json(&text)),
        Err(error) => {
            tracing::error!(%error, "could not read a first prompt's agent settings");
            None
        }
    }
}

/// The queue of the writes, made on first use; `None` until the writes are configured.
#[allow(dead_code)] // used from milestone 4 part 1, wave 3
pub(crate) fn queue(inner: &Arc<Inner>) -> Option<Arc<FirstPromptQueue>> {
    let deps = inner.deps.as_ref()?;
    let queue = inner.firstprompt.get_or_init(|| {
        let backend = InnerBackend {
            inner: Arc::downgrade(inner),
            staging: staging_root(&deps.state_dir),
        };
        FirstPromptQueue::new(
            Arc::clone(&deps.store),
            &deps.state_dir,
            Arc::clone(&deps.locked),
            Arc::new(backend),
            Arc::new(now_ms),
        )
    });
    Some(Arc::clone(queue))
}

/// Starts the pump of the writes' queue, once; nothing before the writes are configured.
#[allow(dead_code)] // used from milestone 4 part 1, wave 3
pub(crate) fn start(inner: &Arc<Inner>) {
    if let Some(queue) = queue(inner) {
        queue.start();
    }
}

/// `DELETE /api/workspaces/:id/prompt`: 200 `{"ok":true}` when an entry went (its staged copies
/// with it), else 404 `{"error":"no pending prompt"}`.
pub(crate) async fn dismiss_first_prompt(inner: Arc<Inner>, workspace_id: String) -> WriteAnswer {
    let Some(deps) = inner.deps.as_ref() else {
        return WriteAnswer::error(404, NO_PENDING_PROMPT);
    };
    if forget_entry(&deps.store, &staging_root(&deps.state_dir), &workspace_id) {
        WriteAnswer::json(200, json!({ "ok": true }))
    } else {
        WriteAnswer::error(404, NO_PENDING_PROMPT)
    }
}

/// Every pending first prompt as the phone's `FirstPrompt` JSON, each with `workspaceId`.
pub(crate) fn pending_prompts(inner: &Arc<Inner>) -> Vec<Value> {
    let Some(deps) = inner.deps.as_ref() else {
        return Vec::new();
    };
    list(&deps.store).iter().map(first_prompt_json).collect()
}

/// Drops the pending first prompt of a workspace (a send by hand landed) and its staged copies,
/// straight through the store, whether or not the queue was made.
#[allow(dead_code)] // used from milestone 4 part 1, wave 3
pub(crate) fn forget(inner: &Arc<Inner>, workspace_id: &str) {
    if let Some(deps) = inner.deps.as_ref() {
        forget_entry(&deps.store, &staging_root(&deps.state_dir), workspace_id);
    }
}
