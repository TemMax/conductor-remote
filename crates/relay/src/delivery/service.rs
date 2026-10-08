//! The write service over the UI thread and Conductor's database.
//!
//! Every write runs in a task of its own, so it goes on when the phone hangs up mid-request.
//! Database reads run on the blocking pool, UI commands on the UI thread, and the waits between
//! them on the async runtime.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use super::deliver::{deliver, send_budget, AttemptError, Delivery, DeliveryTimings};
use super::parked::{cursor_of, parked_json_with, ParkedOutcome, ParkedQueue, PARKED_ERROR};
use super::sendonce::{SendOnce, SENDONCE_TTL};
use super::{agent, attach, chats, create, firstprompt, merge, workspace_ops};
use super::{
    BoxFuture, CreateRequest, SendRequest, SplitRequest, WriteAnswer, WriteService, STRATEGY,
};
use crate::agent::AgentPatch;
use crate::contract::Priority;
use crate::reads::extras::commands::Commands;
use crate::reads::receipts::{DeliveryCursor, VisibleSession, WriteWorkspace};
use crate::reads::workspaces::resolve_worktree;
use crate::reads::{ReadError, Reads};
use crate::state::store::{ParkedRow, Store};
use crate::ui::actor::{UiHandle, UiRunError};
use crate::ui::driver::{Tab, Target, UiDriver, UiError};

/// A chat that is not among the workspace's open chats.
pub(crate) const NOT_A_TAB: &str = "chat is no longer one of the workspace\u{2019}s tabs";
pub(crate) const NO_SESSION_WORKSPACE: &str = "workspace for session not found";
pub(crate) const NO_WORKSPACE: &str = "workspace not found";
/// A parked prompt whose workspace is no longer live.
pub(crate) const WORKSPACE_GONE: &str = "the workspace is gone";
const NO_PARKED_PROMPT: &str = "no parked prompt";
/// The client timeout a parked delivery's budget is computed from: `send_budget` makes it 55 s.
const PARKED_CLIENT_TIMEOUT_MS: u64 = 60_000;
const STILL_WORKING: &str =
    "Conductor took the stop but the agent is still working. Try again, or stop it on your Mac.";
pub(crate) const SEVERAL_NEW_CHATS: &str =
    "more than one new chat appeared; refusing to guess which one this request opened";
const NO_NEW_CHAT: &str =
    "Conductor did not confirm a new chat. Check the workspace before trying again.";
/// The `retry-after` of a busy UI queue, in seconds.
const BUSY_RETRY_AFTER_SECS: u32 = 15;

/// The waits of the writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteTimings {
    pub delivery: DeliveryTimings,
    /// How often a stopped chat's status is checked.
    pub stop_poll: Duration,
    /// How many times it is checked.
    pub stop_checks: u32,
    /// The wait between two looks for a new chat.
    pub chat_poll: Duration,
    /// How many looks for a new chat (one fewer waits).
    pub chat_checks: u32,
    /// Replaces `send_budget(client_timeout_ms)` when set; tests only. Default `None`.
    pub send_budget: Option<Duration>,
    /// The wait between two looks for a restored chat.
    pub restore_poll: Duration,
    /// How many looks for a restored chat.
    pub restore_checks: u32,
    /// The wait between two looks for a created workspace.
    pub create_poll: Duration,
    /// How many looks for a created workspace.
    pub create_checks: u32,
}

impl Default for WriteTimings {
    fn default() -> WriteTimings {
        WriteTimings {
            delivery: DeliveryTimings::default(),
            stop_poll: Duration::from_millis(300),
            stop_checks: 20,
            chat_poll: Duration::from_millis(500),
            chat_checks: 13,
            send_budget: None,
            restore_poll: Duration::from_millis(250),
            restore_checks: 40,
            create_poll: Duration::from_millis(500),
            create_checks: 40,
        }
    }
}

/// What the milestone-4 writes need besides the UI thread and Conductor's database.
#[derive(Clone)]
pub struct WriteDeps {
    /// The relay's state directory (the attachment staging lives under it).
    pub state_dir: PathBuf,
    pub store: Arc<Store>,
    pub commands: Arc<dyn Commands>,
    /// `Some(true)` locked, `Some(false)` unlocked, `None` unknown; tests pass `|| Some(false)`.
    pub locked: Arc<dyn Fn() -> Option<bool> + Send + Sync>,
}

pub(crate) struct Inner {
    pub(crate) reads: Arc<Reads>,
    pub(crate) ui: UiHandle,
    pub(crate) trusted: Arc<dyn Fn() -> bool + Send + Sync>,
    pub(crate) timings: WriteTimings,
    pub(crate) sendonce: SendOnce<WriteAnswer>,
    pub(crate) parked: Arc<ParkedQueue>,
    /// Set by [`Writes::configure`]; `None` until then.
    pub(crate) deps: Option<WriteDeps>,
    /// The first-prompt queue, made on first use. Whatever makes it must not run before
    /// [`Writes::configure`]: a `Weak` to `Inner` would make its `Arc::get_mut` fail.
    pub(crate) firstprompt: OnceLock<Arc<firstprompt::FirstPromptQueue>>,
    /// The chats opened for a model, so a retried request does not open another.
    pub(crate) switches: agent::Switches,
}

/// The real [`WriteService`].
pub struct Writes {
    inner: Arc<Inner>,
}

impl Writes {
    pub fn new(
        reads: Arc<Reads>,
        ui: UiHandle,
        trusted: Arc<dyn Fn() -> bool + Send + Sync>,
        timings: WriteTimings,
        parked: Arc<ParkedQueue>,
    ) -> Writes {
        Writes {
            inner: Arc::new(Inner {
                reads,
                ui,
                trusted,
                timings,
                sendonce: SendOnce::new(SENDONCE_TTL),
                parked,
                deps: None,
                firstprompt: OnceLock::new(),
                switches: agent::Switches::new(),
            }),
        }
    }

    /// Sets the milestone-4 dependencies; call before the writes are shared.
    ///
    /// # Panics
    ///
    /// When the writes are already shared (another `Arc` or a `Weak` to them exists).
    pub fn configure(mut self, deps: WriteDeps) -> Writes {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("Writes::configure must be called before the writes are shared");
        inner.deps = Some(deps);
        self
    }

    /// Starts the pump that sends the pending first prompts, once; call it inside the tokio
    /// runtime, after [`Writes::configure`] (before that it does nothing).
    pub fn start_first_prompts(&self) {
        firstprompt::start(&self.inner);
    }

    /// One parked delivery for the queue's pump: Background priority, the row's persisted cursor
    /// (else a fresh one), the budget `timings.send_budget` when set, else
    /// `send_budget(Some(60_000))` (55 s). Workspace gone → `Failed("the workspace is gone")`;
    /// chat not among the visible chats → `Failed(<the not-a-tab text>)`; delivered →
    /// `Delivered`; locked → `Locked`; else `Failed(<error>)`.
    pub fn deliver_parked(&self, row: ParkedRow) -> BoxFuture<ParkedOutcome> {
        Box::pin(deliver_row(Arc::clone(&self.inner), row))
    }
}

impl WriteService for Writes {
    fn available(&self) -> bool {
        (self.inner.trusted)()
    }

    fn send_prompt(&self, request: SendRequest) -> BoxFuture<WriteAnswer> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            tokio::spawn(send(inner, request))
                .await
                .unwrap_or_else(|_| internal())
        })
    }

    fn stop_turn(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            tokio::spawn(stop(inner, session_id, workspace_id, priority))
                .await
                .unwrap_or_else(|_| internal())
        })
    }

    fn new_chat(&self, workspace_id: String, priority: Priority) -> BoxFuture<WriteAnswer> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            tokio::spawn(open_chat(inner, workspace_id, priority))
                .await
                .unwrap_or_else(|_| internal())
        })
    }

    fn parked_prompts(&self) -> Vec<Value> {
        self.inner
            .parked
            .list()
            .iter()
            .map(|row| parked_json_with(row, self.inner.parked.agent(row.id).as_ref()))
            .collect()
    }

    fn dismiss_parked(&self, session_id: String) -> BoxFuture<WriteAnswer> {
        let parked = Arc::clone(&self.inner.parked);
        Box::pin(async move {
            if parked.forget_session(&session_id) > 0 {
                WriteAnswer::json(200, json!({ "ok": true }))
            } else {
                WriteAnswer::error(404, NO_PARKED_PROMPT)
            }
        })
    }

    fn upload_attachment(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        name: String,
        bytes: Bytes,
    ) -> BoxFuture<WriteAnswer> {
        spawned(attach::upload_attachment(
            Arc::clone(&self.inner),
            session_id,
            workspace_id,
            name,
            bytes,
        ))
    }

    fn stage_attachment(&self, name: String, bytes: Bytes) -> BoxFuture<WriteAnswer> {
        spawned(attach::stage_attachment(
            Arc::clone(&self.inner),
            name,
            bytes,
        ))
    }

    fn discard_staged(&self, stage_id: String) -> BoxFuture<WriteAnswer> {
        spawned(attach::discard_staged(Arc::clone(&self.inner), stage_id))
    }

    fn merge(&self, workspace_id: String) -> BoxFuture<WriteAnswer> {
        spawned(merge::merge(Arc::clone(&self.inner), workspace_id))
    }

    fn restore_chat(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(chats::restore_chat(
            Arc::clone(&self.inner),
            session_id,
            workspace_id,
            priority,
        ))
    }

    fn join_history(
        &self,
        session_id: String,
        workspace_id: String,
        previous_session_id: String,
    ) -> BoxFuture<WriteAnswer> {
        spawned(chats::join_history(
            Arc::clone(&self.inner),
            session_id,
            workspace_id,
            previous_session_id,
        ))
    }

    fn split_chat(
        &self,
        session_id: String,
        request: SplitRequest,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(chats::split_chat(
            Arc::clone(&self.inner),
            session_id,
            request,
            priority,
        ))
    }

    fn create_workspace(
        &self,
        request: CreateRequest,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(create::create_workspace(
            Arc::clone(&self.inner),
            request,
            priority,
        ))
    }

    fn dismiss_first_prompt(&self, workspace_id: String) -> BoxFuture<WriteAnswer> {
        spawned(firstprompt::dismiss_first_prompt(
            Arc::clone(&self.inner),
            workspace_id,
        ))
    }

    fn pending_prompts(&self) -> Vec<Value> {
        firstprompt::pending_prompts(&self.inner)
    }

    fn chat_history(&self, workspace_id: &str) -> Value {
        chats::chat_history(&self.inner, workspace_id)
    }

    fn set_agent(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        patch: AgentPatch,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(agent::set_agent(
            Arc::clone(&self.inner),
            session_id,
            workspace_id,
            patch,
            priority,
        ))
    }

    fn list_models(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(agent::list_models(
            Arc::clone(&self.inner),
            session_id,
            workspace_id,
            priority,
        ))
    }

    fn close_chat(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        close_running: bool,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(workspace_ops::close_chat(
            Arc::clone(&self.inner),
            session_id,
            workspace_id,
            close_running,
            priority,
        ))
    }

    fn set_workspace_status(
        &self,
        workspace_id: String,
        status: String,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(workspace_ops::set_workspace_status(
            Arc::clone(&self.inner),
            workspace_id,
            status,
            priority,
        ))
    }

    fn archive_workspace(
        &self,
        workspace_id: String,
        stop_agents: bool,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(workspace_ops::archive_workspace(
            Arc::clone(&self.inner),
            workspace_id,
            stop_agents,
            priority,
        ))
    }

    fn continue_workspace(
        &self,
        workspace_id: String,
        session_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        spawned(workspace_ops::continue_workspace(
            Arc::clone(&self.inner),
            workspace_id,
            session_id,
            priority,
        ))
    }
}

/// Runs a write in a task of its own, like every write: it goes on when the caller hangs up.
fn spawned(
    work: impl std::future::Future<Output = WriteAnswer> + Send + 'static,
) -> BoxFuture<WriteAnswer> {
    Box::pin(async move { tokio::spawn(work).await.unwrap_or_else(|_| internal()) })
}

pub(crate) fn internal() -> WriteAnswer {
    WriteAnswer::error(500, "internal error")
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// 502 `{"ok":false,"strategy":…,"error":…}`.
pub(crate) fn failed(error: &str) -> WriteAnswer {
    WriteAnswer::json(
        502,
        json!({ "ok": false, "strategy": STRATEGY, "error": error }),
    )
}

/// Runs `read` on the blocking pool. A failed read is logged and becomes the 500 answer.
pub(crate) async fn blocking<T, F>(
    reads: &Arc<Reads>,
    what: &'static str,
    read: F,
) -> Result<T, WriteAnswer>
where
    T: Send + 'static,
    F: FnOnce(&Reads) -> Result<T, ReadError> + Send + 'static,
{
    let reads = Arc::clone(reads);
    match tokio::task::spawn_blocking(move || read(&reads)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            tracing::error!(%error, what, "a write's database read failed");
            Err(internal())
        }
        Err(error) => {
            tracing::error!(%error, what, "a write's database read did not finish");
            Err(internal())
        }
    }
}

/// The workspace of a write and its open chats; `None` when the workspace is not live.
pub(crate) fn workspace_and_chats(
    reads: &Reads,
    workspace_id: Option<&str>,
    session_id: Option<&str>,
) -> Result<Option<(WriteWorkspace, Vec<VisibleSession>)>, ReadError> {
    let Some(workspace) = reads.write_workspace(workspace_id, session_id)? else {
        return Ok(None);
    };
    let chats = reads.visible_sessions(&workspace.id)?;
    Ok(Some((workspace, chats)))
}

/// The target of a workspace with no chat selected.
pub(crate) fn workspace_target(workspace: &WriteWorkspace) -> Target {
    Target {
        workspace_id: workspace.id.clone(),
        session_id: None,
        repo: workspace.repo_name.clone(),
        branch: workspace.branch.clone().unwrap_or_default(),
        workspace_name: workspace.workspace_name.clone(),
        tab: None,
    }
}

/// The target of `session_id` and its status; `None` when it is not one of the open chats.
pub(crate) fn chat_target(
    workspace: &WriteWorkspace,
    chats: &[VisibleSession],
    session_id: &str,
) -> Option<(Target, Option<String>)> {
    let position = chats.iter().position(|chat| chat.id == session_id)?;
    let chat = &chats[position];
    let target = Target {
        session_id: Some(session_id.to_owned()),
        tab: Some(Tab {
            index: position + 1,
            count: chats.len(),
            title: chat.title.clone(),
        }),
        ..workspace_target(workspace)
    };
    Some((target, chat.status.clone()))
}

// ---- send ----

async fn send(inner: Arc<Inner>, request: SendRequest) -> WriteAnswer {
    let workspace_id = request.workspace_id.clone();
    let session_id = request.session_id.clone();
    let located = blocking(&inner.reads, "send.workspace", move |reads| {
        workspace_and_chats(reads, workspace_id.as_deref(), Some(&session_id))
    })
    .await;
    let (workspace, chats) = match located {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, NO_SESSION_WORKSPACE),
        Ok(Some(found)) => found,
    };
    let Some((target, _)) = chat_target(&workspace, &chats, &request.session_id) else {
        return WriteAnswer::json(
            502,
            json!({ "ok": false, "strategy": STRATEGY, "attempts": 0, "error": NOT_A_TAB }),
        );
    };

    let key = request.client_id.clone();
    let sendonce = inner.sendonce.clone();
    let work = deliver_prompt(inner, request, target);
    sendonce
        .run(
            key.as_deref(),
            |answer: &WriteAnswer| answer.status == 200 || answer.status == 202,
            work,
        )
        .await
        .unwrap_or_else(internal)
}

/// The agent settings, then the cursor, then the send loop; a lock in the way parks the prompt.
async fn deliver_prompt(inner: Arc<Inner>, request: SendRequest, target: Target) -> WriteAnswer {
    let requested = request.session_id.clone();
    let mut session_id: Arc<str> = requested.as_str().into();
    let mut target = target;
    if let Some(patch) = &request.agent {
        match agent::apply_agent(
            &inner,
            &requested,
            Some(&target.workspace_id),
            patch,
            request.priority,
        )
        .await
        {
            Err(agent::AgentError::Answer(answer)) => return answer,
            Err(agent::AgentError::Locked(text)) => {
                return park_before_patch(&inner, &request, &target, patch, &text).await;
            }
            Ok(applied) if applied.session_id == requested => {}
            Ok(applied) => {
                match locate_chat(&inner, &target.workspace_id, &applied.session_id).await {
                    Ok(Some(new_target)) => target = new_target,
                    Ok(None) => return not_a_tab(),
                    Err(answer) => return answer,
                }
                session_id = applied.session_id.as_str().into();
            }
        }
    }

    let cursor = match read_cursor(&inner, &session_id, "send.cursor").await {
        Ok(cursor) => cursor,
        Err(answer) => return answer,
    };
    let text = request.text;
    let workspace_id = target.workspace_id.clone();
    let budget = inner
        .timings
        .send_budget
        .unwrap_or_else(|| send_budget(request.client_timeout_ms));
    let delivery = match deliver_text(
        &inner,
        target,
        &session_id,
        &text,
        request.queue,
        request.priority,
        Some(cursor.clone()),
        budget,
    )
    .await
    {
        Ok(delivery) => delivery,
        Err(answer) => return answer,
    };

    if let Some(receipt) = delivery.receipt {
        // The prompt the chat was opened for is in: a later prompt must not be redirected there.
        if let Some(model) = request
            .agent
            .as_ref()
            .and_then(|patch| patch.model.as_deref())
        {
            inner.switches.forget(&requested, model);
        }
        inner.parked.forget_delivered(&session_id, &text);
        // A prompt that landed by hand makes the workspace's pending first prompt moot.
        firstprompt::forget(&inner, &workspace_id);
        let mut body = json!({
            "ok": true,
            "strategy": STRATEGY,
            "attempts": delivery.attempts,
            "receipt": receipt,
        });
        if *session_id != *requested {
            body["sessionId"] = Value::from(&*session_id);
        }
        return WriteAnswer::json(200, body);
    }
    if delivery.locked {
        // The settings were applied already: the row waits without them.
        match inner.parked.park_with_agent(
            &workspace_id,
            &session_id,
            &text,
            request.queue,
            &cursor,
            now_ms(),
            None,
        ) {
            Ok(row) => return parked_answer(&row, None),
            Err(error) => tracing::error!(%error, "could not park a prompt on a locked Mac"),
        }
    }
    WriteAnswer::json(
        502,
        json!({
            "ok": false,
            "strategy": STRATEGY,
            "attempts": delivery.attempts,
            "error": delivery.error.unwrap_or_default(),
        }),
    )
}

/// The Mac locked at the patch: the prompt waits with its settings, in the requested chat.
async fn park_before_patch(
    inner: &Arc<Inner>,
    request: &SendRequest,
    target: &Target,
    patch: &AgentPatch,
    lock_text: &str,
) -> WriteAnswer {
    let session_id: Arc<str> = request.session_id.as_str().into();
    let cursor = match read_cursor(inner, &session_id, "send.cursor").await {
        Ok(cursor) => cursor,
        Err(answer) => return answer,
    };
    match inner.parked.park_with_agent(
        &target.workspace_id,
        &session_id,
        &request.text,
        request.queue,
        &cursor,
        now_ms(),
        Some(patch),
    ) {
        Ok(row) => parked_answer(&row, Some(patch)),
        Err(error) => {
            tracing::error!(%error, "could not park a prompt on a locked Mac");
            WriteAnswer::json(
                502,
                json!({
                    "ok": false,
                    "strategy": STRATEGY,
                    "attempts": 0,
                    "error": lock_text,
                }),
            )
        }
    }
}

/// The 202 of a prompt that was parked.
fn parked_answer(row: &ParkedRow, agent: Option<&AgentPatch>) -> WriteAnswer {
    WriteAnswer::json(
        202,
        json!({
            "ok": false,
            "parked": true,
            "queued": parked_json_with(row, agent),
            "strategy": STRATEGY,
            "error": PARKED_ERROR,
        }),
    )
}

fn not_a_tab() -> WriteAnswer {
    WriteAnswer::json(
        502,
        json!({ "ok": false, "strategy": STRATEGY, "attempts": 0, "error": NOT_A_TAB }),
    )
}

/// The target of `session_id` among the open chats of the workspace; `None` when it is not one
/// of them.
async fn locate_chat(
    inner: &Arc<Inner>,
    workspace_id: &str,
    session_id: &str,
) -> Result<Option<Target>, WriteAnswer> {
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    let located = blocking(&inner.reads, "send.relocate", move |reads| {
        workspace_and_chats(reads, Some(&workspace_id), None)
    })
    .await?;
    Ok(located.and_then(|(workspace, chats)| {
        chat_target(&workspace, &chats, &session_id).map(|(target, _)| target)
    }))
}

/// One delivery of `text` into an open chat (the send route's loop): the cursor (given, or read),
/// the UI thread at `priority`, the budget.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn deliver_text(
    inner: &Arc<Inner>,
    target: Target,
    session_id: &str,
    text: &str,
    queue: bool,
    priority: Priority,
    cursor: Option<DeliveryCursor>,
    budget: Duration,
) -> Result<Delivery, WriteAnswer> {
    let session_id: Arc<str> = session_id.into();
    let cursor = match cursor {
        Some(cursor) => cursor,
        None => read_cursor(inner, &session_id, "deliver.cursor").await?,
    };
    Ok(run_delivery(
        inner,
        Prompt {
            target,
            session_id,
            text: text.into(),
            queue,
            priority,
            cursor: Arc::new(cursor),
            budget,
        },
    )
    .await)
}

/// The delivery cursor of a chat, read on the blocking pool.
pub(crate) async fn read_cursor(
    inner: &Arc<Inner>,
    session_id: &Arc<str>,
    what: &'static str,
) -> Result<DeliveryCursor, WriteAnswer> {
    let session_id = Arc::clone(session_id);
    blocking(&inner.reads, what, move |reads| {
        reads.delivery_cursor(&session_id)
    })
    .await
}

/// One prompt to deliver into an open chat.
struct Prompt {
    target: Target,
    session_id: Arc<str>,
    text: Arc<str>,
    queue: bool,
    priority: Priority,
    /// Receipts count only after this.
    cursor: Arc<DeliveryCursor>,
    budget: Duration,
}

/// The send loop over the UI thread and the receipt read: the one delivery of the send route and
/// of the parked queue.
async fn run_delivery(inner: &Arc<Inner>, prompt: Prompt) -> Delivery {
    let Prompt {
        target,
        session_id,
        text,
        queue,
        priority,
        cursor,
        budget,
    } = prompt;
    let target = Arc::new(target);

    // The deadline is advisory: a queued UI job cannot be cancelled and every Accessibility
    // message has its own timeout, so the run is never cut short (an abandoned job would type
    // later).
    let attempt = |_deadline| {
        let ui = inner.ui.clone();
        let target = Arc::clone(&target);
        let text = Arc::clone(&text);
        let reads = Arc::clone(&inner.reads);
        let session_id = Arc::clone(&session_id);
        let cursor = Arc::clone(&cursor);
        async move {
            let run = ui.run(priority, move |driver: &mut dyn UiDriver| {
                // The receipt is read again here, on the UI thread, right before typing: two
                // deliveries of one text can both have seen no receipt and queued their jobs, and
                // the UI thread runs them one after the other. A blocking read is fine on it.
                match reads.delivery_receipt_since(&session_id, &text, &cursor) {
                    Ok(Some(_)) => return Ok(0),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(%error, "the receipt read before typing failed");
                    }
                }
                driver.send_prompt(&target, &text, queue)
            });
            match run.await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => Err(attempt_error(&error)),
                Err(error) => Err(AttemptError {
                    message: error.to_string(),
                    sent_nothing: true,
                    terminal: false,
                    lock: false,
                }),
            }
        }
    };
    let probe = || {
        let reads = Arc::clone(&inner.reads);
        let session_id = Arc::clone(&session_id);
        let text = Arc::clone(&text);
        let cursor = Arc::clone(&cursor);
        async move {
            let read = tokio::task::spawn_blocking(move || {
                reads.delivery_receipt_since(&session_id, &text, &cursor)
            })
            .await;
            match read {
                Ok(Ok(receipt)) => receipt,
                Ok(Err(error)) => {
                    tracing::error!(%error, "the receipt read failed");
                    None
                }
                Err(error) => {
                    tracing::error!(%error, "the receipt read did not finish");
                    None
                }
            }
        }
    };
    deliver(attempt, probe, budget, inner.timings.delivery).await
}

/// The text of a write answer's `error`, for a parked delivery that failed before the loop.
fn answer_error(answer: &WriteAnswer) -> String {
    answer.body["error"]
        .as_str()
        .unwrap_or("internal error")
        .to_owned()
}

/// One parked row through the send loop, at Background priority.
async fn deliver_row(inner: Arc<Inner>, row: ParkedRow) -> ParkedOutcome {
    let located = {
        let workspace_id = row.workspace_id.clone();
        let session_id = row.session_id.clone();
        blocking(&inner.reads, "parked.workspace", move |reads| {
            workspace_and_chats(reads, Some(&workspace_id), Some(&session_id))
        })
        .await
    };
    let (workspace, chats) = match located {
        Err(answer) => return ParkedOutcome::Failed(answer_error(&answer)),
        Ok(None) => return ParkedOutcome::Failed(WORKSPACE_GONE.to_owned()),
        Ok(Some(found)) => found,
    };
    let Some((mut target, _)) = chat_target(&workspace, &chats, &row.session_id) else {
        return ParkedOutcome::Failed(NOT_A_TAB.to_owned());
    };
    let mut session_id: Arc<str> = row.session_id.as_str().into();
    let mut cursor = cursor_of(&row);
    let stored_patch = inner.parked.agent(row.id);
    let stored_model = stored_patch.as_ref().and_then(|patch| patch.model.clone());
    if let Some(patch) = stored_patch {
        match agent::apply_agent(
            &inner,
            &row.session_id,
            Some(&row.workspace_id),
            &patch,
            Priority::Background,
        )
        .await
        {
            Err(agent::AgentError::Locked(_)) => return ParkedOutcome::Locked,
            Err(agent::AgentError::Answer(answer)) => {
                return ParkedOutcome::Failed(answer_error(&answer));
            }
            Ok(applied) if applied.session_id == row.session_id => {
                // A retry must not apply the settings again.
                inner.parked.set_agent(row.id, None);
            }
            Ok(applied) => {
                // The patch stays: a retry asks for the model again and the memo of the service
                // sends it to this same new chat.
                match locate_chat(&inner, &row.workspace_id, &applied.session_id).await {
                    Ok(Some(new_target)) => target = new_target,
                    Ok(None) => return ParkedOutcome::Failed(NOT_A_TAB.to_owned()),
                    Err(answer) => return ParkedOutcome::Failed(answer_error(&answer)),
                }
                session_id = applied.session_id.as_str().into();
                cursor = None;
            }
        }
    }
    let cursor = match cursor {
        Some(cursor) => cursor,
        None => match read_cursor(&inner, &session_id, "parked.cursor").await {
            Ok(cursor) => cursor,
            Err(answer) => return ParkedOutcome::Failed(answer_error(&answer)),
        },
    };
    let budget = inner
        .timings
        .send_budget
        .unwrap_or_else(|| send_budget(Some(PARKED_CLIENT_TIMEOUT_MS)));
    let delivery = run_delivery(
        &inner,
        Prompt {
            target,
            session_id,
            text: row.text.as_str().into(),
            queue: row.queue,
            priority: Priority::Background,
            cursor: Arc::new(cursor),
            budget,
        },
    )
    .await;

    if delivery.receipt.is_some() {
        if let Some(model) = stored_model {
            inner.switches.forget(&row.session_id, &model);
        }
        ParkedOutcome::Delivered
    } else if delivery.locked {
        ParkedOutcome::Locked
    } else {
        ParkedOutcome::Failed(delivery.error.unwrap_or_default())
    }
}

fn attempt_error(error: &UiError) -> AttemptError {
    AttemptError {
        message: error.to_string(),
        sent_nothing: error.sent_nothing(),
        terminal: error.retry_wont_help(),
        lock: error.is_lock(),
    }
}

// ---- stop ----

async fn stop(
    inner: Arc<Inner>,
    session_id: String,
    workspace_id: Option<String>,
    priority: Priority,
) -> WriteAnswer {
    let located = {
        let session_id = session_id.clone();
        blocking(&inner.reads, "stop.workspace", move |reads| {
            workspace_and_chats(reads, workspace_id.as_deref(), Some(&session_id))
        })
        .await
    };
    let (workspace, chats) = match located {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, NO_SESSION_WORKSPACE),
        Ok(Some(found)) => found,
    };
    let Some((target, status)) = chat_target(&workspace, &chats, &session_id) else {
        return WriteAnswer::error(409, NOT_A_TAB);
    };

    if status.as_deref() != Some("working") {
        let body = json!({ "ok": true, "alreadyIdle": true });
        return with_session(&inner.reads, &workspace.id, &session_id, body).await;
    }

    match inner
        .ui
        .run(priority, move |driver: &mut dyn UiDriver| {
            driver.stop_turn(&target)
        })
        .await
    {
        Err(error) => return failed(&error.to_string()),
        Ok(Err(error)) => return failed(&error.to_string()),
        Ok(Ok(())) => {}
    }

    // The keystroke is fire-and-forget: the receipt is the status leaving "working". A chat that
    // is gone from the list keeps the status last seen.
    let mut observed = status;
    for _ in 0..inner.timings.stop_checks {
        if observed.as_deref() != Some("working") {
            break;
        }
        tokio::time::sleep(inner.timings.stop_poll).await;
        let workspace_id = workspace.id.clone();
        let chats = match blocking(&inner.reads, "stop.status", move |reads| {
            reads.visible_sessions(&workspace_id)
        })
        .await
        {
            Ok(chats) => chats,
            Err(answer) => return answer,
        };
        if let Some(chat) = chats.into_iter().find(|chat| chat.id == session_id) {
            observed = chat.status;
        }
    }
    if observed.as_deref() == Some("working") {
        return failed(STILL_WORKING);
    }

    let body = json!({ "ok": true, "strategy": STRATEGY });
    with_session(&inner.reads, &workspace.id, &session_id, body).await
}

/// 200 with `body` plus `"session"`: the chat's `SessionRow` as `list_sessions` serves it, the
/// key left out when the chat is not listed.
pub(crate) async fn with_session(
    reads: &Arc<Reads>,
    workspace_id: &str,
    session_id: &str,
    mut body: Value,
) -> WriteAnswer {
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    let row = match blocking(reads, "stop.session", move |reads| {
        Ok(reads
            .list_sessions(&workspace_id)?
            .into_iter()
            .find(|row| row.id == session_id))
    })
    .await
    {
        Ok(row) => row,
        Err(answer) => return answer,
    };
    if let Some(row) = row {
        let session = match serde_json::to_value(row) {
            Ok(session) => session,
            Err(error) => {
                tracing::error!(%error, "a session row did not serialize");
                return internal();
            }
        };
        if let Some(object) = body.as_object_mut() {
            object.insert("session".to_owned(), session);
        }
    }
    WriteAnswer::json(200, body)
}

// ---- new chat ----

/// What the new-chat job saw on the UI thread.
enum Opened {
    /// The baseline read failed; nothing was pressed.
    BaselineFailed,
    Done {
        command: Result<(), UiError>,
        /// The ids that appeared, in tab order.
        fresh: Vec<String>,
    },
}

async fn open_chat(inner: Arc<Inner>, workspace_id: String, priority: Priority) -> WriteAnswer {
    let workspace = match blocking(&inner.reads, "new_chat.workspace", move |reads| {
        reads.write_workspace(Some(&workspace_id), None)
    })
    .await
    {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, NO_WORKSPACE),
        Ok(Some(workspace)) => workspace,
    };
    match open_new_chat(&inner, &workspace, priority).await {
        Ok(id) => WriteAnswer::json(200, json!({ "ok": true, "sessionId": id })),
        Err(answer) => answer,
    }
}

/// The new-chat job of `open_chat` (baseline, Cmd+T, checks): the id of the one new chat, or the
/// answer to give.
pub(crate) async fn open_new_chat(
    inner: &Arc<Inner>,
    workspace: &WriteWorkspace,
    priority: Priority,
) -> Result<String, WriteAnswer> {
    let target = workspace_target(workspace);
    let reads = Arc::clone(&inner.reads);
    let timings = inner.timings;

    // One job does it all, so two opens cannot claim each other's chat.
    let job = move |driver: &mut dyn UiDriver| -> Opened {
        let baseline: HashSet<String> = match reads.visible_sessions(&target.workspace_id) {
            Ok(chats) => chats.into_iter().map(|chat| chat.id).collect(),
            Err(error) => {
                tracing::error!(%error, "the open chats before a new chat could not be read");
                return Opened::BaselineFailed;
            }
        };
        let command = driver.new_chat(&target);
        // A command that pressed nothing cannot have opened a chat: free the UI thread now.
        if let Err(error) = &command {
            if error.sent_nothing() {
                return Opened::Done {
                    command,
                    fresh: Vec::new(),
                };
            }
        }
        let mut fresh = Vec::new();
        for check in 0..timings.chat_checks {
            if check > 0 {
                std::thread::sleep(timings.chat_poll);
            }
            match reads.visible_sessions(&target.workspace_id) {
                Ok(chats) => {
                    fresh = chats
                        .into_iter()
                        .map(|chat| chat.id)
                        .filter(|id| !baseline.contains(id))
                        .collect();
                    if !fresh.is_empty() {
                        break;
                    }
                }
                // A later look may still see the chat, so a failed one is only logged.
                Err(error) => {
                    tracing::error!(%error, "the open chats after a new chat could not be read");
                }
            }
        }
        Opened::Done { command, fresh }
    };

    match inner.ui.run(priority, job).await {
        Err(error @ UiRunError::Busy { waiting }) => Err(WriteAnswer {
            status: 503,
            body: json!({
                "error": error.to_string(),
                "busy": true,
                "queue": { "waiting": waiting, "busy": true },
            }),
            retry_after_secs: Some(BUSY_RETRY_AFTER_SECS),
        }),
        Err(error @ UiRunError::Crashed) => Err(failed(&error.to_string())),
        Ok(Opened::BaselineFailed) => Err(internal()),
        Ok(Opened::Done { command, fresh }) => match (fresh.as_slice(), command) {
            ([id], _) => Ok(id.clone()),
            ([], Ok(())) => Err(failed(NO_NEW_CHAT)),
            ([], Err(error)) => Err(failed(&error.to_string())),
            (_, _) => Err(failed(SEVERAL_NEW_CHATS)),
        },
    }
}

// ---- checkout ----

/// A workspace's worktree and its repository's checkout.
#[allow(dead_code)] // used from milestone 4 part 1, wave 2
pub(crate) struct Checkout {
    /// `repos.root_path`; `None` when missing or empty.
    pub repo_root: Option<String>,
    /// `repos.name`.
    pub repo_name: Option<String>,
    /// The resolved worktree directory.
    pub worktree: Option<PathBuf>,
}

const CHECKOUT_SQL: &str = "SELECT r.root_path, r.name, w.directory_name, w.branch \
     FROM workspaces w LEFT JOIN repos r ON r.id = w.repository_id WHERE w.id = ?";

/// The worktree of a live workspace, and its repository's checkout: `repos.root_path` read with
/// one query, the worktree resolved with `resolve_worktree` exactly as the messages read does. An
/// empty `root_path` counts as missing; `None` when the workspace is not found. Blocking.
#[allow(dead_code)] // used from milestone 4 part 1, wave 2
pub(crate) fn locate_checkout(
    reads: &Reads,
    workspace_id: &str,
) -> Result<Option<Checkout>, ReadError> {
    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let found: Option<Row> = reads.db().read("writes.checkout", |conn| {
        conn.query_row(CHECKOUT_SQL, [workspace_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .optional()
    })?;
    Ok(found.map(|(root_path, repo_name, directory_name, branch)| {
        let repo_root = root_path.filter(|root| !root.is_empty());
        let worktree = resolve_worktree(
            reads.workspaces_root(),
            repo_name.as_deref(),
            directory_name.as_deref(),
            branch.as_deref(),
            repo_root.as_deref(),
        );
        Checkout {
            repo_root,
            repo_name,
            worktree,
        }
    }))
}
