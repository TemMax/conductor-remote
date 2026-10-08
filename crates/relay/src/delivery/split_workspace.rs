//! A split into a new workspace: the fork.
//!
//! The source's code is captured as a snapshot before anything is created; Conductor then makes
//! the workspace, and once its worktree is the fresh checkout Conductor leaves, the snapshot is
//! installed into it and the transcript is written beside it. The snapshot is released on every
//! way out. Whatever fails, the new workspace is left as it is: the relay never archives, deletes
//! or resets it.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rusqlite::OptionalExtension;
use serde_json::json;

use super::chats::{attachment_json, attachment_prompt, Counts};
use super::create::create_in_repo;
use super::service::{blocking, internal, now_ms, Inner};
use super::WriteAnswer;
use crate::contract::Priority;
use crate::files::attachments::write_attachment;
use crate::fork::{self, Snapshot};
use crate::reads::extras::commands::Commands;
use crate::reads::{ReadError, Reads};

const NO_CHECKOUT: &str = "the source workspace has no repository checkout to fork";
const NOT_READY: &str = "the new workspace was not ready for the code (its folder is missing, on \
                         another branch, or already changed)";
const REPO_NOT_A_FOLDER: &str = "the repository's name is not a folder name, so the new \
                                 workspace's folder cannot be found";
const SOURCE_UNCONFIRMED: &str = "the source workspace's folder could not be confirmed: it is not \
                                  on the workspace's branch";
const DESTINATION_SQL: &str = "SELECT directory_name, branch FROM workspaces WHERE id = ?";

/// Where the fork takes the code from.
pub(super) struct ForkSource {
    /// The source workspace's worktree.
    pub(super) worktree: PathBuf,
    /// `repos.name` of the source.
    pub(super) repo_name: Option<String>,
    /// `repos.root_path` of the source.
    pub(super) repo_root: Option<String>,
    /// `workspaces.directory_name` of the source.
    pub(super) directory_name: Option<String>,
    /// `workspaces.branch` of the source.
    pub(super) branch: Option<String>,
}

/// What a split has ready before its destination is known.
pub(super) struct Prepared {
    /// The attachment's file name.
    pub(super) name: String,
    /// The rendered transcript, header included.
    pub(super) transcript: String,
    pub(super) counts: Counts,
    /// The request's prompt.
    pub(super) prompt: Option<String>,
}

/// A captured snapshot on its way into a new workspace of the repository `(repo_name, repo_root)`.
struct Fork {
    commands: Arc<dyn Commands>,
    snapshot: Arc<Snapshot>,
    repo_name: String,
    repo_root: String,
    prepared: Prepared,
}

/// One look at the new workspace's row.
struct Look {
    /// The row's `directory_name`, when it has one.
    directory_name: Option<String>,
    /// The worktree and its branch, once it is the checkout a fork waits for.
    ready: Option<(PathBuf, String)>,
}

/// 502 `{"error": "Workspace <label> was created, but its code fork failed: <reason>"}`.
fn fork_failed(label: &str, reason: &str) -> WriteAnswer {
    WriteAnswer::error(
        502,
        &format!("Workspace {label} was created, but its code fork failed: {reason}"),
    )
}

/// Whether `name` is one normal path component: joined to a directory it stays inside it.
fn single_component(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Reads where Conductor put the workspace `workspace_id` and asks git whether that folder is
/// ready. The folder is `<workspaces root>/<repo_name>/<directory_name>` and nothing else: the
/// code is installed over it, so it is never searched for by branch. Blocking.
fn look(
    reads: &Reads,
    commands: &dyn Commands,
    workspace_id: &str,
    repo_name: &str,
) -> Result<Look, ReadError> {
    type Row = (Option<String>, Option<String>);
    let row: Option<Row> = reads.db().read("writes.fork_destination", |conn| {
        conn.query_row(DESTINATION_SQL, [workspace_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()
    })?;
    let (directory_name, branch) = row.unwrap_or_default();
    let directory_name = directory_name.filter(|name| !name.is_empty());
    let branch = branch.filter(|branch| !branch.is_empty());
    let ready = match (directory_name.as_deref(), branch) {
        (Some(directory), Some(branch))
            if single_component(repo_name) && single_component(directory) =>
        {
            let destination = reads.workspaces_root().join(repo_name).join(directory);
            fork::ready(commands, &destination, &branch).then_some((destination, branch))
        }
        _ => None,
    };
    Ok(Look {
        directory_name,
        ready,
    })
}

/// `POST /api/sessions/:id/split` with a workspace destination, once the transcript is rendered.
pub(super) async fn into_workspace(
    inner: &Arc<Inner>,
    source: ForkSource,
    prepared: Prepared,
    priority: Priority,
) -> WriteAnswer {
    let Some(deps) = inner.deps.as_ref() else {
        tracing::error!("a fork reached writes that were never configured");
        return internal();
    };
    let commands = Arc::clone(&deps.commands);

    let non_empty = |value: Option<String>| value.filter(|value| !value.is_empty());
    let (Some(repo_name), Some(repo_root)) =
        (non_empty(source.repo_name), non_empty(source.repo_root))
    else {
        return WriteAnswer::error(409, NO_CHECKOUT);
    };
    if !single_component(&repo_name) {
        return WriteAnswer::error(409, REPO_NOT_A_FOLDER);
    }

    // A folder in its normal place is the workspace's own; any other was found by searching for
    // the branch's name, and is only the workspace's when it has that branch checked out.
    let worktree = source.worktree;
    let normal = non_empty(source.directory_name)
        .filter(|directory| single_component(directory))
        .map(|directory| {
            inner
                .reads
                .workspaces_root()
                .join(&repo_name)
                .join(directory)
        });
    if normal.as_ref() != Some(&worktree) {
        let on_branch = match non_empty(source.branch) {
            None => Ok(false),
            Some(branch) => {
                let commands = Arc::clone(&commands);
                let worktree = worktree.clone();
                tokio::task::spawn_blocking(move || fork::on_branch(&*commands, &worktree, &branch))
                    .await
            }
        };
        match on_branch {
            Ok(true) => {}
            Ok(false) => return WriteAnswer::error(409, SOURCE_UNCONFIRMED),
            Err(error) => {
                tracing::error!(%error, "a fork's source check did not finish");
                return internal();
            }
        }
    }

    let captured = {
        let commands = Arc::clone(&commands);
        tokio::task::spawn_blocking(move || fork::capture(&*commands, &worktree, now_ms())).await
    };
    let snapshot = match captured {
        Ok(Ok(snapshot)) => Arc::new(snapshot),
        Ok(Err(reason)) => {
            return WriteAnswer::error(
                502,
                &format!("Could not snapshot the source workspace: {reason}"),
            )
        }
        Err(error) => {
            tracing::error!(%error, "a fork's snapshot did not finish");
            return internal();
        }
    };

    // From here on every way out goes through the release below.
    let fork = Fork {
        commands: Arc::clone(&commands),
        snapshot: Arc::clone(&snapshot),
        repo_name,
        repo_root,
        prepared,
    };
    let answer = install(inner, fork, priority).await;

    match tokio::task::spawn_blocking(move || fork::release(&*commands, &snapshot)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "a fork's snapshot could not be released"),
        Err(error) => tracing::warn!(%error, "a fork's snapshot release did not finish"),
    }
    answer
}

/// Has Conductor make the workspace, installs the snapshot and the transcript into it and looks
/// for its chat. Never releases the snapshot: the caller does, whatever this answers.
async fn install(inner: &Arc<Inner>, fork: Fork, priority: Priority) -> WriteAnswer {
    let Fork {
        commands,
        snapshot,
        repo_name,
        repo_root,
        prepared,
    } = fork;
    let timings = inner.timings;

    let workspace =
        match create_in_repo(inner, Some((repo_name.clone(), repo_root)), priority).await {
            Ok(workspace) => workspace,
            Err(answer) => return answer,
        };

    // The new worktree: Conductor writes the row first and checks the branch out after.
    let mut directory_name = workspace
        .directory_name
        .clone()
        .filter(|name| !name.is_empty());
    let mut ready = None;
    for check in 0..timings.create_checks {
        if check > 0 {
            tokio::time::sleep(timings.create_poll).await;
        }
        let looked = {
            let commands = Arc::clone(&commands);
            let workspace_id = workspace.id.clone();
            let repo_name = repo_name.clone();
            blocking(&inner.reads, "fork.destination", move |reads| {
                look(reads, &*commands, &workspace_id, &repo_name)
            })
            .await
        };
        // A later look may still see the worktree, so a failed one is only logged.
        let Ok(looked) = looked else {
            continue;
        };
        if looked.directory_name.is_some() {
            directory_name = looked.directory_name;
        }
        if looked.ready.is_some() {
            ready = looked.ready;
            break;
        }
    }
    let label = directory_name.unwrap_or_else(|| workspace.id.clone());
    let Some((destination, branch)) = ready else {
        return fork_failed(&label, NOT_READY);
    };

    let materialized = {
        let destination = destination.clone();
        tokio::task::spawn_blocking(move || {
            fork::materialize(&*commands, &snapshot, &destination, &branch)
        })
        .await
    };
    match materialized {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => return fork_failed(&label, &reason.to_string()),
        Err(error) => {
            tracing::error!(%error, workspace_id = %workspace.id, "a fork's code install did not finish");
            return internal();
        }
    }

    let Prepared {
        name,
        transcript,
        counts,
        prompt,
    } = prepared;
    let written = tokio::task::spawn_blocking(move || {
        write_attachment(&destination, &name, transcript.as_bytes(), false)
    })
    .await;
    let no_transcript = || {
        WriteAnswer::error(
            502,
            &format!(
                "Workspace {label} was created with the code, but its transcript could not be \
                 written"
            ),
        )
    };
    let written = match written {
        Ok(Ok(written)) => written,
        Ok(Err(error)) => {
            tracing::error!(%error, workspace_id = %workspace.id, "a fork's transcript could not be written");
            return no_transcript();
        }
        Err(error) => {
            tracing::error!(%error, workspace_id = %workspace.id, "a fork's transcript write did not finish");
            return no_transcript();
        }
    };

    // The new workspace's chat; without one the phone opens the workspace and waits for it.
    let mut session_id = None;
    for check in 0..timings.chat_checks {
        if check > 0 {
            tokio::time::sleep(timings.chat_poll).await;
        }
        let workspace_id = workspace.id.clone();
        let chats = blocking(&inner.reads, "fork.chats", move |reads| {
            reads.visible_sessions(&workspace_id)
        })
        .await;
        // A failed look is logged and counts as no chat.
        if let Some(first) = chats.ok().and_then(|chats| chats.into_iter().next()) {
            session_id = Some(first.id);
            break;
        }
    }

    WriteAnswer::json(
        200,
        json!({
            "ok": true,
            "destination": "workspace",
            "sessionId": session_id,
            "workspaceId": workspace.id,
            "text": attachment_prompt(&written.token, prompt.as_deref()),
            "attachment": attachment_json(&written, &counts),
        }),
    )
}
