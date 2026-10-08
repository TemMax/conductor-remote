//! Close chat, workspace status, archive and Continue: each presses Conductor's own control and
//! waits for the database to show the change.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use super::agent::busy;
use super::chats::{chat_row, CHAT_NOT_FOUND, NOT_IN_WORKSPACE};
use super::service::{
    blocking, chat_target, failed, internal, workspace_target, Inner, NOT_A_TAB,
    NO_SESSION_WORKSPACE, NO_WORKSPACE,
};
use super::{WriteAnswer, STRATEGY};
use crate::contract::Priority;
use crate::reads::receipts::{VisibleSession, WriteWorkspace};
use crate::reads::sessions::SessionRow;
use crate::reads::states::workspace_title;
use crate::reads::workspaces::Workspace;
use crate::reads::{ReadError, Reads};
use crate::ui::actor::UiRunError;
use crate::ui::driver::{UiDriver, UiError};

const AGENT_RUNNING_IN_CHAT: &str =
    "The agent is still working in this chat. Confirm closing it anyway.";
const AGENTS_RUNNING: &str = "Agents are still working here. Archiving stops them.";
const STATUS_CHOICES: &str =
    "status must be one of backlog, in-progress, in-review, done, canceled";
const NO_BRANCH_TO_CONTINUE: &str = "workspace has no branch to continue";
const STILL_OPEN: &str =
    "Conductor took the close but the chat tab is still open. Try again, or close it on your Mac.";
const STILL_IN_SIDEBAR: &str = "Conductor took the archive but the workspace is still in the sidebar. Try again, or archive it on your Mac.";
const NO_NEW_BRANCH: &str =
    "Conductor did not record a new branch within 30 seconds. Check it on your Mac before retrying.";
const NOT_RECORDED: &str =
    "Conductor didn\u{2019}t record the change \u{2014} it may have been asleep. Try again.";

/// How many times the open chats are looked at after a close.
const CLOSE_LOOKS: u32 = 20;
/// How many times the workspace is looked at after a status change.
const STATUS_LOOKS: u32 = 10;
/// How many times the workspace is looked at after an archive.
const ARCHIVE_LOOKS: u32 = 20;
/// How many times the workspace is looked at after Continue.
const CONTINUE_LOOKS: u32 = 60;

/// The statuses of the status menu: the wire name and the menu label.
const STATUSES: [(&str, &str); 5] = [
    ("backlog", "Backlog"),
    ("in-progress", "In progress"),
    ("in-review", "In review"),
    ("done", "Done"),
    ("canceled", "Canceled"),
];

/// Runs `job` on the UI thread. A full queue is the busy answer, a crashed thread and a failed
/// command are 502; `confirmation` is what `NeedsConfirmation` becomes, when the command can ask.
async fn run_ui(
    inner: &Arc<Inner>,
    priority: Priority,
    confirmation: Option<WriteAnswer>,
    job: impl FnOnce(&mut dyn UiDriver) -> Result<(), UiError> + Send + 'static,
) -> Result<(), WriteAnswer> {
    match inner.ui.run(priority, job).await {
        Err(error @ UiRunError::Busy { waiting }) => Err(busy(&error, waiting)),
        Err(error @ UiRunError::Crashed) => Err(failed(&error.to_string())),
        Ok(Err(UiError::NeedsConfirmation)) if confirmation.is_some() => {
            Err(confirmation.unwrap_or_else(internal))
        }
        Ok(Err(error)) => Err(failed(&error.to_string())),
        Ok(Ok(())) => Ok(()),
    }
}

/// Reads until `holds` is true of what was read: at most `looks` reads, `pause` between two.
/// Returns the last thing read and whether it held.
async fn look_up<T, R, H>(
    inner: &Arc<Inner>,
    what: &'static str,
    pause: Duration,
    looks: u32,
    read: R,
    holds: H,
) -> Result<(T, bool), WriteAnswer>
where
    T: Send + 'static,
    R: Fn(&Reads) -> Result<T, ReadError> + Clone + Send + 'static,
    H: Fn(&T) -> bool,
{
    let mut look = 0;
    loop {
        let read = read.clone();
        let seen = blocking(&inner.reads, what, move |reads| read(reads)).await?;
        look += 1;
        if holds(&seen) {
            return Ok((seen, true));
        }
        if look >= looks {
            return Ok((seen, false));
        }
        tokio::time::sleep(pause).await;
    }
}

/// The value as JSON; a value that does not serialize is the 500 answer.
fn to_json(value: &impl Serialize) -> Result<Value, WriteAnswer> {
    serde_json::to_value(value).map_err(|error| {
        tracing::error!(%error, "a workspace did not serialize");
        internal()
    })
}

/// The live workspace with this id.
fn live_workspace(reads: &Reads, workspace_id: &str) -> Result<Option<Workspace>, ReadError> {
    Ok(reads
        .list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.id == workspace_id))
}

/// What a write needs of a live workspace, from the fields the live list has.
fn write_workspace(workspace: &Workspace) -> WriteWorkspace {
    WriteWorkspace {
        id: workspace.id.clone(),
        branch: workspace.branch.clone(),
        repo_name: workspace.repo_name.clone(),
        workspace_name: workspace.workspace_name.clone(),
        directory_name: workspace.directory_name.clone(),
    }
}

// ---- close chat ----

/// A workspace's open chats and the chat its window shows.
struct OpenChats {
    chats: Vec<VisibleSession>,
    active: Option<String>,
}

impl OpenChats {
    fn read(reads: &Reads, workspace_id: &str) -> Result<OpenChats, ReadError> {
        Ok(OpenChats {
            chats: reads.visible_sessions(workspace_id)?,
            active: live_workspace(reads, workspace_id)?
                .and_then(|workspace| workspace.active_session_id),
        })
    }

    fn has(&self, session_id: &str) -> bool {
        self.chats.iter().any(|chat| chat.id == session_id)
    }

    /// The workspace's active chat when it is open, else the first open chat, else `null`.
    fn active_session_id(&self) -> Value {
        self.active
            .iter()
            .find(|id| self.has(id))
            .or_else(|| self.chats.first().map(|chat| &chat.id))
            .map_or(Value::Null, |id| Value::from(id.as_str()))
    }
}

/// What the close reads before it presses anything.
struct CloseScene {
    workspace: WriteWorkspace,
    open: OpenChats,
    /// The chat's row in `list_sessions`; `None` when it is not an open chat.
    row: Option<SessionRow>,
}

/// `DELETE /api/sessions/:id`.
pub(crate) async fn close_chat(
    inner: Arc<Inner>,
    session_id: String,
    workspace_id: Option<String>,
    close_running: bool,
    priority: Priority,
) -> WriteAnswer {
    let owner = {
        let session_id = session_id.clone();
        blocking(&inner.reads, "close.owner", move |reads| {
            Ok(chat_row(reads, &session_id)?.and_then(|row| row.workspace_id))
        })
        .await
    };
    let owner = match owner {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, CHAT_NOT_FOUND),
        Ok(Some(owner)) => owner,
    };
    if workspace_id.as_deref().is_some_and(|given| given != owner) {
        return WriteAnswer::error(409, NOT_IN_WORKSPACE);
    }

    let scene = {
        let session_id = session_id.clone();
        blocking(&inner.reads, "close.scene", move |reads| {
            let Some(workspace) = reads.write_workspace(Some(&owner), None)? else {
                return Ok(None);
            };
            let open = OpenChats::read(reads, &workspace.id)?;
            let row = reads
                .list_sessions(&workspace.id)?
                .into_iter()
                .find(|row| row.id == session_id);
            Ok(Some(CloseScene {
                workspace,
                open,
                row,
            }))
        })
        .await
    };
    let CloseScene {
        workspace,
        open,
        row,
    } = match scene {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, NO_SESSION_WORKSPACE),
        Ok(Some(scene)) => scene,
    };

    let Some(row) = row else {
        return WriteAnswer::json(
            200,
            json!({
                "ok": true,
                "alreadyClosed": true,
                "activeSessionId": open.active_session_id(),
            }),
        );
    };
    let agent_running = agent_running_answer();
    // Best effort: the dialog decides, through `NeedsConfirmation`.
    if !close_running
        && (row.status.as_deref() == Some("working") || !row.background_tasks.is_empty())
    {
        return agent_running;
    }
    let Some((target, _)) = chat_target(&workspace, &open.chats, &session_id) else {
        return WriteAnswer::error(409, NOT_A_TAB);
    };

    if let Err(answer) = run_ui(
        &inner,
        priority,
        Some(agent_running),
        move |driver: &mut dyn UiDriver| driver.close_chat(&target, close_running),
    )
    .await
    {
        return answer;
    }

    let looked = {
        let workspace_id = workspace.id.clone();
        let session_id = session_id.clone();
        look_up(
            &inner,
            "close.chats",
            inner.timings.stop_poll,
            CLOSE_LOOKS,
            move |reads| OpenChats::read(reads, &workspace_id),
            move |open| !open.has(&session_id),
        )
        .await
    };
    match looked {
        Err(answer) => answer,
        Ok((_, false)) => failed(STILL_OPEN),
        Ok((open, true)) => WriteAnswer::json(
            200,
            json!({
                "ok": true,
                "strategy": STRATEGY,
                "activeSessionId": open.active_session_id(),
            }),
        ),
    }
}

fn agent_running_answer() -> WriteAnswer {
    WriteAnswer::json(
        409,
        json!({ "ok": false, "agentRunning": true, "error": AGENT_RUNNING_IN_CHAT }),
    )
}

// ---- workspace status ----

/// `POST /api/workspaces/:id/status`.
pub(crate) async fn set_workspace_status(
    inner: Arc<Inner>,
    workspace_id: String,
    status: String,
    priority: Priority,
) -> WriteAnswer {
    let Some(&(_, label)) = STATUSES.iter().find(|(name, _)| *name == status) else {
        return WriteAnswer::error(400, STATUS_CHOICES);
    };
    let workspace = {
        let workspace_id = workspace_id.clone();
        blocking(&inner.reads, "status.workspace", move |reads| {
            live_workspace(reads, &workspace_id)
        })
        .await
    };
    let workspace = match workspace {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, NO_WORKSPACE),
        Ok(Some(workspace)) => workspace,
    };

    if workspace.manual_status.as_deref() != Some(status.as_str()) {
        let target = workspace_target(&write_workspace(&workspace));
        let row = workspace_title(
            workspace.workspace_name.as_deref(),
            workspace.pr_title.as_deref(),
            workspace.branch.as_deref(),
            workspace.directory_name.as_deref(),
            &workspace.id,
        );
        if let Err(answer) = run_ui(&inner, priority, None, move |driver: &mut dyn UiDriver| {
            driver.set_status(&target, &row, label)
        })
        .await
        {
            return answer;
        }
    }

    let looked = {
        let status = status.clone();
        look_up(
            &inner,
            "status.recorded",
            inner.timings.stop_poll,
            STATUS_LOOKS,
            move |reads| live_workspace(reads, &workspace_id),
            move |seen| {
                seen.as_ref()
                    .is_some_and(|workspace| workspace.manual_status.as_deref() == Some(&status))
            },
        )
        .await
    };
    match looked {
        Err(answer) => answer,
        Ok((seen, false)) => {
            let observed = seen
                .and_then(|workspace| workspace.manual_status)
                .filter(|observed| !observed.is_empty());
            match observed {
                Some(observed) => failed(&format!(
                    "Conductor recorded the status as \u{201c}{observed}\u{201d}, not \u{201c}{status}\u{201d}."
                )),
                None => failed(NOT_RECORDED),
            }
        }
        Ok((seen, true)) => match seen.as_ref().map(to_json) {
            Some(Ok(workspace)) => {
                WriteAnswer::json(200, json!({ "ok": true, "workspace": workspace }))
            }
            Some(Err(answer)) => answer,
            None => internal(),
        },
    }
}

// ---- archive ----

/// `POST /api/workspaces/:id/archive`.
pub(crate) async fn archive_workspace(
    inner: Arc<Inner>,
    workspace_id: String,
    stop_agents: bool,
    priority: Priority,
) -> WriteAnswer {
    /// What the archive reads before it presses anything.
    enum Scene {
        Live {
            workspace: Box<Workspace>,
            working: usize,
        },
        Archived(Box<crate::reads::workspaces::SearchWorkspace>),
        Gone,
    }
    let scene = {
        let workspace_id = workspace_id.clone();
        blocking(&inner.reads, "archive.workspace", move |reads| {
            if let Some(workspace) = live_workspace(reads, &workspace_id)? {
                let working = reads
                    .list_sessions(&workspace_id)?
                    .iter()
                    .filter(|row| row.status.as_deref() == Some("working"))
                    .count();
                return Ok(Scene::Live {
                    workspace: Box::new(workspace),
                    working,
                });
            }
            Ok(match reads.get_any_workspace(&workspace_id)? {
                Some(found) if found.archived => Scene::Archived(Box::new(found)),
                _ => Scene::Gone,
            })
        })
        .await
    };
    let (workspace, working) = match scene {
        Err(answer) => return answer,
        Ok(Scene::Gone) => return WriteAnswer::error(404, NO_WORKSPACE),
        Ok(Scene::Archived(found)) => {
            return match to_json(&*found) {
                Ok(workspace) => WriteAnswer::json(
                    200,
                    json!({ "ok": true, "alreadyArchived": true, "workspace": workspace }),
                ),
                Err(answer) => answer,
            };
        }
        Ok(Scene::Live { workspace, working }) => (*workspace, working),
    };

    if working > 0 && !stop_agents {
        let error = if working == 1 {
            "1 agent is still working here. Archiving stops them.".to_owned()
        } else {
            format!("{working} agents are still working here. Archiving stops them.")
        };
        return WriteAnswer::json(
            409,
            json!({ "ok": false, "agentsRunning": true, "error": error }),
        );
    }

    let target = workspace_target(&write_workspace(&workspace));
    let confirmation = WriteAnswer::json(
        409,
        json!({ "ok": false, "agentsRunning": true, "error": AGENTS_RUNNING }),
    );
    if let Err(answer) = run_ui(
        &inner,
        priority,
        Some(confirmation),
        move |driver: &mut dyn UiDriver| driver.archive(&target, stop_agents),
    )
    .await
    {
        return answer;
    }

    let looked = look_up(
        &inner,
        "archive.recorded",
        inner.timings.stop_poll,
        ARCHIVE_LOOKS,
        move |reads| reads.get_any_workspace(&workspace_id),
        |seen| seen.as_ref().is_some_and(|found| found.archived),
    )
    .await;
    match looked {
        Err(answer) => answer,
        Ok((_, false)) => failed(STILL_IN_SIDEBAR),
        Ok((seen, true)) => match seen.as_ref().map(to_json) {
            Some(Ok(workspace)) => WriteAnswer::json(
                200,
                json!({ "ok": true, "strategy": STRATEGY, "workspace": workspace }),
            ),
            Some(Err(answer)) => answer,
            None => internal(),
        },
    }
}

// ---- continue ----

/// `POST /api/workspaces/:id/continue`.
pub(crate) async fn continue_workspace(
    inner: Arc<Inner>,
    workspace_id: String,
    session_id: Option<String>,
    priority: Priority,
) -> WriteAnswer {
    let scene = {
        let workspace_id = workspace_id.clone();
        blocking(&inner.reads, "continue.workspace", move |reads| {
            let Some(workspace) = live_workspace(reads, &workspace_id)? else {
                return Ok(None);
            };
            let chats = reads.visible_sessions(&workspace_id)?;
            Ok(Some((workspace, chats)))
        })
        .await
    };
    let (workspace, chats) = match scene {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, NO_WORKSPACE),
        Ok(Some(found)) => found,
    };
    if session_id
        .as_deref()
        .is_some_and(|given| !chats.iter().any(|chat| chat.id == given))
    {
        return WriteAnswer::error(409, NOT_A_TAB);
    }
    let previous = workspace.branch.clone().unwrap_or_default();
    if previous.is_empty() {
        return WriteAnswer::json(409, json!({ "ok": false, "error": NO_BRANCH_TO_CONTINUE }));
    }

    let write = write_workspace(&workspace);
    let target = session_id
        .as_deref()
        .or(workspace
            .active_session_id
            .as_deref()
            .filter(|active| chats.iter().any(|chat| chat.id == *active)))
        .and_then(|id| chat_target(&write, &chats, id))
        .map_or_else(|| workspace_target(&write), |(target, _)| target);
    if let Err(answer) = run_ui(&inner, priority, None, move |driver: &mut dyn UiDriver| {
        driver.press_continue(&target)
    })
    .await
    {
        return answer;
    }

    let looked = {
        let previous = previous.clone();
        look_up(
            &inner,
            "continue.branch",
            inner.timings.chat_poll,
            CONTINUE_LOOKS,
            move |reads| live_workspace(reads, &workspace_id),
            move |seen| {
                seen.as_ref().is_some_and(|workspace| {
                    workspace
                        .branch
                        .as_deref()
                        .is_some_and(|branch| !branch.is_empty() && branch != previous)
                })
            },
        )
        .await
    };
    match looked {
        Err(answer) => answer,
        Ok((_, false)) => failed(NO_NEW_BRANCH),
        Ok((seen, true)) => match seen.as_ref().map(to_json) {
            Some(Ok(workspace)) => WriteAnswer::json(
                200,
                json!({ "ok": true, "previousBranch": previous, "workspace": workspace }),
            ),
            Some(Err(answer)) => answer,
            None => internal(),
        },
    }
}
