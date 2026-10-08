//! Creating a workspace.
//!
//! Conductor's link (`conductor://path=…`) opens its New workspace dialog, and pressing Create
//! there makes the workspace. The link carries no request id and answers nothing: the new live row
//! in Conductor's database is the only proof it worked. The link, the press of Create and the
//! looks for that row share one job on the UI thread, so two creations never run side by side and
//! cannot claim each other's workspace. The first prompt is left to the first-prompt queue, which
//! sends it once the chat exists.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::agent::{apply_agent, AgentError};
use super::firstprompt::{self, staging_root};
use super::service::{blocking, internal, now_ms, Inner};
use super::{CreateRequest, WriteAnswer};
use crate::agent::AgentPatch;
use crate::contract::Priority;
use crate::files::attachments::staged_attachments;
use crate::reads::workspaces::Workspace;
use crate::ui::driver::{create_link, UiDriver, UiError};

/// The `strategy` of a creation: Conductor's own link.
const CREATE_STRATEGY: &str = "deeplink";
const ATTACHMENT_GONE: &str = "an attached file is no longer available; add it again";
const NEED_REPO_OR_PROMPT: &str = "need a repo or a prompt";
const NO_CHAT_YET: &str =
    "the new workspace has no chat yet, so its agent settings were not applied";
const NOT_CREATED: &str = "Conductor didn\u{2019}t create a workspace \u{2014} check it\u{2019}s \
                           running and not showing a dialog.";

/// 502 `{"ok":false,"strategy":"deeplink","error":…}`.
fn create_failed(error: &str) -> WriteAnswer {
    WriteAnswer::json(
        502,
        json!({ "ok": false, "strategy": CREATE_STRATEGY, "error": error }),
    )
}

/// What the creation job saw on the UI thread.
enum Created {
    /// The live workspaces before the link could not be read; nothing was opened.
    BaselineFailed,
    /// The link did not open, or its dialog could not be confirmed.
    LinkFailed(UiError),
    /// No new live workspace (of the repository) appeared.
    Nothing,
    Workspace(Box<Workspace>),
}

/// `POST /api/workspaces`.
pub(crate) async fn create_workspace(
    inner: Arc<Inner>,
    request: CreateRequest,
    priority: Priority,
) -> WriteAnswer {
    let Some(deps) = inner.deps.as_ref() else {
        tracing::error!("a workspace creation reached writes that were never configured");
        return internal();
    };

    // 1. Every staged file, before anything else.
    let staging = staging_root(&deps.state_dir);
    let ids = request.attachment_ids.clone();
    let staged = match tokio::task::spawn_blocking(move || staged_attachments(&staging, &ids)).await
    {
        Ok(staged) => staged,
        Err(error) => {
            tracing::error!(%error, "reading the staged attachments did not finish");
            return internal();
        }
    };
    let Some(staged) = staged else {
        return WriteAnswer::error(409, ATTACHMENT_GONE);
    };

    // 2. The objective: the attachments' tokens, then the trimmed prompt.
    let prompt = request.prompt.as_deref().unwrap_or("").trim();
    let objective = staged
        .iter()
        .map(|attachment| attachment.token.as_str())
        .chain(std::iter::once(prompt))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if objective.is_empty() && request.repo.is_none() {
        return WriteAnswer::error(400, NEED_REPO_OR_PROMPT);
    }

    // 3. The repository's checkout: a link with an unknown `path` would land the workspace in
    //    whichever repository Conductor lists first.
    let repo = match &request.repo {
        None => None,
        Some(name) => {
            let wanted = name.clone();
            let found = blocking(&inner.reads, "create.repo", move |reads| {
                Ok(reads
                    .list_repos()?
                    .into_iter()
                    .find(|repo| repo.name.as_deref() == Some(wanted.as_str())))
            })
            .await;
            let repo = match found {
                Err(answer) => return answer,
                Ok(None) => return WriteAnswer::error(404, &format!("unknown repo {name}")),
                Ok(Some(repo)) => repo,
            };
            let Some(root) = repo.root_path.filter(|root| !root.is_empty()) else {
                return WriteAnswer::error(409, &format!("{name} has no checkout path"));
            };
            Some((name.clone(), root))
        }
    };

    // 4. The link, the press of Create and the looks for the new row.
    let workspace = match create_in_repo(&inner, repo, priority).await {
        Ok(workspace) => workspace,
        Err(answer) => return answer,
    };

    // 5. The first prompt goes to the queue; the answer does not wait for it. The agent chosen on
    //    the phone is kept beside it (or a stale one cleared) and applied when the prompt is sent.
    if !objective.is_empty() {
        let agent = request.agent.as_ref().map(AgentPatch::to_json);
        if let Err(error) = deps
            .store
            .set_first_prompt_agent(&workspace.id, agent.as_deref())
        {
            tracing::error!(%error, workspace_id = %workspace.id, "could not keep a new workspace's agent settings");
            return internal();
        }
        let Some(queue) = firstprompt::queue(&inner) else {
            tracing::error!("configured writes have no first-prompt queue");
            return internal();
        };
        if let Err(error) = queue.enqueue(
            &workspace.id,
            &objective,
            request.send_immediately,
            request.attachment_ids.clone(),
            now_ms(),
        ) {
            tracing::error!(%error, workspace_id = %workspace.id, "could not queue a new workspace's first prompt");
            return internal();
        }
    }

    // Without a first prompt the agent settings go onto the new workspace's only chat now.
    let mut configured = false;
    let mut warning: Option<String> = None;
    if let (true, Some(patch)) = (objective.is_empty(), request.agent.as_ref()) {
        match apply_to_new_chat(&inner, &workspace.id, patch, priority).await {
            Ok(()) => configured = true,
            Err(text) => warning = Some(text),
        }
    }

    let workspace_id = workspace.id.clone();
    let workspace = match serde_json::to_value(workspace) {
        Ok(workspace) => workspace,
        Err(error) => {
            tracing::error!(%error, "a workspace did not serialize");
            return internal();
        }
    };
    let mut body = Map::new();
    body.insert("ok".into(), Value::Bool(true));
    body.insert("workspaceId".into(), Value::from(workspace_id));
    body.insert("workspace".into(), workspace);
    if !objective.is_empty() {
        body.insert("pendingPrompt".into(), Value::from(objective));
    }
    body.insert("sent".into(), Value::Bool(false));
    body.insert("configured".into(), Value::Bool(configured));
    if let Some(warning) = warning {
        body.insert("warning".into(), Value::from(warning));
    }
    WriteAnswer::json(200, Value::Object(body))
}

/// Has Conductor make a workspace in the repository `(name, root)`, or wherever Conductor puts one
/// when `repo` is `None`: the link, the press of Create and the looks for the new row in one job.
/// `Err` is the answer to give.
pub(crate) async fn create_in_repo(
    inner: &Arc<Inner>,
    repo: Option<(String, String)>,
    priority: Priority,
) -> Result<Workspace, WriteAnswer> {
    // The link opens Conductor's New workspace dialog and does not make the workspace; it carries
    // no prompt, because Conductor would send it too, and the first prompt goes through the queue.
    let link = create_link(None, repo.as_ref().map(|(_, root)| root.as_str()));
    let repo_name = repo.map(|(name, _)| name);
    let reads = Arc::clone(&inner.reads);
    let timings = inner.timings;
    let job = move |driver: &mut dyn UiDriver| -> Created {
        let before: HashSet<String> = match reads.list_workspaces() {
            Ok(workspaces) => workspaces
                .into_iter()
                .map(|workspace| workspace.id)
                .collect(),
            Err(error) => {
                tracing::error!(%error, "the live workspaces before a creation could not be read");
                return Created::BaselineFailed;
            }
        };
        if let Err(error) = driver.open_link(&link) {
            return Created::LinkFailed(error);
        }
        // Without this press the dialog stays open and no workspace appears. A missing dialog is
        // reported like a failed link.
        if let Err(error) = driver.confirm_create() {
            return Created::LinkFailed(error);
        }
        for _ in 0..timings.create_checks {
            std::thread::sleep(timings.create_poll);
            match reads.list_workspaces() {
                Ok(workspaces) => {
                    let created = workspaces.into_iter().find(|workspace| {
                        !before.contains(&workspace.id)
                            && repo_name
                                .as_deref()
                                .is_none_or(|name| workspace.repo_name.as_deref() == Some(name))
                    });
                    if let Some(workspace) = created {
                        return Created::Workspace(Box::new(workspace));
                    }
                }
                // A later look may still see the row, so a failed one is only logged.
                Err(error) => {
                    tracing::error!(%error, "the live workspaces after a creation could not be read");
                }
            }
        }
        Created::Nothing
    };
    match inner.ui.run(priority, job).await {
        Err(error) => Err(create_failed(&error.to_string())),
        Ok(Created::BaselineFailed) => Err(internal()),
        Ok(Created::LinkFailed(error)) => Err(create_failed(&error.to_string())),
        Ok(Created::Nothing) => Err(create_failed(NOT_CREATED)),
        Ok(Created::Workspace(workspace)) => Ok(*workspace),
    }
}

/// Applies `patch` to the chat of a workspace that was just created: the chat is looked for up to
/// `create_checks` times until exactly one is listed. `Err` is the warning to give.
async fn apply_to_new_chat(
    inner: &Arc<Inner>,
    workspace_id: &str,
    patch: &AgentPatch,
    priority: Priority,
) -> Result<(), String> {
    let timings = inner.timings;
    let mut chat = None;
    for check in 0..timings.create_checks {
        if check > 0 {
            tokio::time::sleep(timings.create_poll).await;
        }
        let id = workspace_id.to_owned();
        let chats = blocking(&inner.reads, "create.chats", move |reads| {
            reads.visible_sessions(&id)
        })
        .await;
        // A later look may still see the chat, so a failed one is only logged.
        if let Ok(chats) = chats {
            if let [only] = chats.as_slice() {
                chat = Some(only.id.clone());
                break;
            }
        }
    }
    let Some(chat) = chat else {
        return Err(NO_CHAT_YET.to_owned());
    };
    match apply_agent(inner, &chat, Some(workspace_id), patch, priority).await {
        Ok(_) => Ok(()),
        Err(AgentError::Locked(text)) => Err(text),
        Err(AgentError::Answer(answer)) => Err(answer.body["error"]
            .as_str()
            .unwrap_or("internal error")
            .to_owned()),
    }
}
