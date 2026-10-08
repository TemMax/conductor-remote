//! The agent settings of a chat: apply a patch through Conductor's composer controls, and read
//! the model names off its menu.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::json;

use super::service::{
    blocking, chat_target, failed, internal, now_ms, with_session, workspace_and_chats, Inner,
    NOT_A_TAB, NO_SESSION_WORKSPACE, SEVERAL_NEW_CHATS,
};
use super::{WriteAnswer, STRATEGY};
use crate::agent::AgentPatch;
use crate::contract::Priority;
use crate::reads::receipts::{VisibleSession, WriteWorkspace};
use crate::reads::sessions::SessionRow;
use crate::ui::actor::UiRunError;
use crate::ui::driver::{AgentFailure, AgentOutcome, Target, UiDriver};

/// How long a chat Conductor opened for a model is remembered.
const SWITCH_TTL: Duration = Duration::from_secs(10 * 60);
/// How many times the chat's row is looked at for the settings to show.
const FOLLOW_CHECKS: u32 = 10;
/// The `retry-after` of a busy UI queue, in seconds.
const BUSY_RETRY_AFTER_SECS: u32 = 15;
const NO_MODEL_CHAT: &str =
    "Conductor did not confirm the new chat for the model. Check the workspace before trying again.";

/// The chats Conductor opened for a model, so a retried request does not open another.
pub(crate) struct Switches {
    opened: Mutex<HashMap<(String, String), (String, Instant)>>,
}

impl Switches {
    pub(crate) fn new() -> Switches {
        Switches {
            opened: Mutex::new(HashMap::new()),
        }
    }

    /// The chat opened for `model` (compared lower-cased) from `session_id`, within the last 10
    /// minutes.
    pub(crate) fn get(&self, session_id: &str, model: &str) -> Option<String> {
        let opened = self.opened.lock().unwrap_or_else(PoisonError::into_inner);
        let (new_session_id, at) = opened.get(&key(session_id, model))?;
        (at.elapsed() < SWITCH_TTL).then(|| new_session_id.clone())
    }

    pub(crate) fn put(&self, session_id: &str, model: &str, new_session_id: &str) {
        let mut opened = self.opened.lock().unwrap_or_else(PoisonError::into_inner);
        opened.retain(|_, (_, at)| at.elapsed() < SWITCH_TTL);
        opened.insert(
            key(session_id, model),
            (new_session_id.to_owned(), Instant::now()),
        );
    }

    /// Drops the chat remembered for `model` from `session_id`: the prompt it was opened for
    /// has been delivered, so a later request opens its own chat.
    pub(crate) fn forget(&self, session_id: &str, model: &str) {
        let mut opened = self.opened.lock().unwrap_or_else(PoisonError::into_inner);
        opened.remove(&key(session_id, model));
    }
}

fn key(session_id: &str, model: &str) -> (String, String) {
    (session_id.to_owned(), model.to_lowercase())
}

/// A patch that was applied.
pub(crate) struct Applied {
    pub workspace: WriteWorkspace,
    /// The chat the settings now belong to: the requested one, or the chat Conductor opened.
    pub session_id: String,
}

pub(crate) enum AgentError {
    /// The Mac is locked; nothing was changed. Holds `UiError::Locked`'s text.
    Locked(String),
    /// The answer to give.
    Answer(WriteAnswer),
}

/// What the agent job saw on the UI thread.
enum Ran {
    /// The baseline read failed; nothing was pressed.
    BaselineFailed,
    Done {
        result: Result<AgentOutcome, AgentFailure>,
        /// The ids that appeared, in tab order.
        fresh: Vec<String>,
    },
}

/// 503 for a full UI queue, as the new-chat write gives it.
pub(crate) fn busy(error: &UiRunError, waiting: usize) -> WriteAnswer {
    WriteAnswer {
        status: 503,
        body: json!({
            "error": error.to_string(),
            "busy": true,
            "queue": { "waiting": waiting, "busy": true },
        }),
        retry_after_secs: Some(BUSY_RETRY_AFTER_SECS),
    }
}

/// The workspace, its open chats and the target of `session_id`, or the answer to give.
async fn locate(
    inner: &Arc<Inner>,
    what: &'static str,
    session_id: &str,
    workspace_id: Option<&str>,
) -> Result<(WriteWorkspace, Vec<VisibleSession>, Target), WriteAnswer> {
    let located = {
        let workspace_id = workspace_id.map(str::to_owned);
        let session_id = session_id.to_owned();
        blocking(&inner.reads, what, move |reads| {
            workspace_and_chats(reads, workspace_id.as_deref(), Some(&session_id))
        })
        .await?
    };
    let Some((workspace, chats)) = located else {
        return Err(WriteAnswer::error(404, NO_SESSION_WORKSPACE));
    };
    let Some((target, _)) = chat_target(&workspace, &chats, session_id) else {
        return Err(WriteAnswer::error(409, NOT_A_TAB));
    };
    Ok((workspace, chats, target))
}

/// Applies `patch` to the chat. A model of another provider makes Conductor open a new chat; the
/// chat is remembered, so a retry goes on in it instead of opening another.
pub(crate) async fn apply_agent(
    inner: &Arc<Inner>,
    session_id: &str,
    workspace_id: Option<&str>,
    patch: &AgentPatch,
    priority: Priority,
) -> Result<Applied, AgentError> {
    let (workspace, chats, mut target) = locate(inner, "agent.workspace", session_id, workspace_id)
        .await
        .map_err(AgentError::Answer)?;

    let mut patch = patch.clone();
    let requested_model = patch.model.clone();
    if let Some(model) = &requested_model {
        let remembered = inner
            .switches
            .get(session_id, model)
            .filter(|id| chats.iter().any(|chat| &chat.id == id));
        if let Some(opened) = remembered {
            if let Some((opened_target, _)) = chat_target(&workspace, &chats, &opened) {
                target = opened_target;
                patch.model = None;
                if patch.is_empty() {
                    return Ok(Applied {
                        workspace,
                        session_id: opened,
                    });
                }
            }
        }
    }

    let reads = Arc::clone(&inner.reads);
    let timings = inner.timings;
    let job_target = target.clone();
    let job_patch = patch.clone();
    // One job does it all, so two requests cannot claim each other's chat.
    let job = move |driver: &mut dyn UiDriver| -> Ran {
        let baseline: HashSet<String> = match reads.visible_sessions(&job_target.workspace_id) {
            Ok(chats) => chats.into_iter().map(|chat| chat.id).collect(),
            Err(error) => {
                tracing::error!(%error, "the open chats before an agent change could not be read");
                return Ran::BaselineFailed;
            }
        };
        let result = driver.set_agent(&job_target, &job_patch);
        let opened = match &result {
            Ok(outcome) => outcome.new_chat,
            Err(failure) => failure.new_chat,
        };
        let mut fresh = Vec::new();
        if opened {
            for check in 0..timings.chat_checks {
                if check > 0 {
                    std::thread::sleep(timings.chat_poll);
                }
                match reads.visible_sessions(&job_target.workspace_id) {
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
                        tracing::error!(%error, "the open chats after an agent change could not be read");
                    }
                }
            }
        }
        Ran::Done { result, fresh }
    };

    let (result, fresh) = match inner.ui.run(priority, job).await {
        Err(error @ UiRunError::Busy { waiting }) => {
            return Err(AgentError::Answer(busy(&error, waiting)));
        }
        Err(error @ UiRunError::Crashed) => {
            return Err(AgentError::Answer(failed(&error.to_string())));
        }
        Ok(Ran::BaselineFailed) => return Err(AgentError::Answer(internal())),
        Ok(Ran::Done { result, fresh }) => (result, fresh),
    };

    let remember = |new_session_id: &str| {
        if let Some(model) = &requested_model {
            inner.switches.put(session_id, model, new_session_id);
        }
    };
    match result {
        Err(failure) if failure.error.is_lock() => {
            Err(AgentError::Locked(failure.error.to_string()))
        }
        Err(failure) => {
            if let (true, [id]) = (failure.new_chat, fresh.as_slice()) {
                remember(id);
            }
            Err(AgentError::Answer(failed(&failure.error.to_string())))
        }
        Ok(outcome) if !outcome.new_chat => Ok(Applied {
            workspace,
            session_id: target.session_id.unwrap_or_else(|| session_id.to_owned()),
        }),
        Ok(_) => match fresh.as_slice() {
            [id] => {
                remember(id);
                Ok(Applied {
                    workspace,
                    session_id: id.clone(),
                })
            }
            [] => Err(AgentError::Answer(failed(NO_MODEL_CHAT))),
            _ => Err(AgentError::Answer(failed(SEVERAL_NEW_CHATS))),
        },
    }
}

/// `POST /api/sessions/:id/agent`: the patch, then the wait for Conductor's database to follow.
pub(crate) async fn set_agent(
    inner: Arc<Inner>,
    session_id: String,
    workspace_id: Option<String>,
    patch: AgentPatch,
    priority: Priority,
) -> WriteAnswer {
    let applied = match apply_agent(
        &inner,
        &session_id,
        workspace_id.as_deref(),
        &patch,
        priority,
    )
    .await
    {
        Ok(applied) => applied,
        Err(AgentError::Locked(text)) => return failed(&text),
        Err(AgentError::Answer(answer)) => return answer,
    };

    // The menus already confirmed the change, so after the last look the answer is given anyway.
    if patch.effort.is_some() || patch.fast.is_some() || patch.plan.is_some() {
        for look in 0..FOLLOW_CHECKS {
            if look > 0 {
                tokio::time::sleep(inner.timings.stop_poll).await;
            }
            let workspace_id = applied.workspace.id.clone();
            let chat_id = applied.session_id.clone();
            let row = match blocking(&inner.reads, "agent.follow", move |reads| {
                Ok(reads
                    .list_sessions(&workspace_id)?
                    .into_iter()
                    .find(|row| row.id == chat_id))
            })
            .await
            {
                Ok(row) => row,
                Err(answer) => return answer,
            };
            if row.is_some_and(|row| follows(&row, &patch)) {
                break;
            }
        }
    }

    let body = json!({ "ok": true, "strategy": STRATEGY, "sessionId": applied.session_id });
    with_session(
        &inner.reads,
        &applied.workspace.id,
        &applied.session_id,
        body,
    )
    .await
}

/// Every field the patch set shows in the chat's row (the model is not checked).
fn follows(row: &SessionRow, patch: &AgentPatch) -> bool {
    patch
        .effort
        .is_none_or(|effort| row.claude_effort_level.as_deref() == Some(effort.as_str()))
        && patch
            .fast
            .is_none_or(|fast| (row.fast_mode.unwrap_or(0) != 0) == fast)
        && patch
            .plan
            .is_none_or(|plan| (row.permission_mode.as_deref() == Some("plan")) == plan)
}

/// `GET /api/sessions/:id/models`: the model names off the chat's model menu, kept for the phone.
pub(crate) async fn list_models(
    inner: Arc<Inner>,
    session_id: String,
    workspace_id: Option<String>,
    priority: Priority,
) -> WriteAnswer {
    let (workspace, _, target) = match locate(
        &inner,
        "models.workspace",
        &session_id,
        workspace_id.as_deref(),
    )
    .await
    {
        Ok(found) => found,
        Err(answer) => return answer,
    };

    let models = match inner
        .ui
        .run(priority, move |driver: &mut dyn UiDriver| {
            driver.list_models(&target)
        })
        .await
    {
        Err(error @ UiRunError::Busy { waiting }) => return busy(&error, waiting),
        Err(error @ UiRunError::Crashed) => return failed(&error.to_string()),
        Ok(Err(error)) => return failed(&error.to_string()),
        Ok(Ok(models)) => models,
    };

    let reads = Arc::clone(&inner.reads);
    let names = models.clone();
    let recorded = tokio::task::spawn_blocking(move || {
        let agent_type = match reads.list_sessions(&workspace.id) {
            Ok(rows) => rows
                .into_iter()
                .find(|row| row.id == session_id)
                .and_then(|row| row.agent_type),
            Err(error) => {
                tracing::error!(%error, "the chat's agent type could not be read");
                None
            }
        }
        .unwrap_or_else(|| "unknown".to_owned());
        reads.record_models(
            &agent_type,
            &names,
            names.first().map(String::as_str),
            now_ms(),
        )
    })
    .await;
    match recorded {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%error, "the model names could not be kept"),
        Err(error) => tracing::error!(%error, "keeping the model names did not finish"),
    }

    WriteAnswer::json(
        200,
        json!({ "ok": true, "models": models, "defaultModel": models.first() }),
    )
}
