//! Attachments: uploads into a chat, and the staging of files for a workspace yet to be created.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use serde_json::json;

use super::service::{blocking, internal, locate_checkout, Inner};
use super::WriteAnswer;
use crate::files::attachments::{self, Written, MAX_ATTACHMENT_BYTES};
use crate::reads::{ReadError, Reads};

/// The directory under the state directory that holds files staged for a workspace to come.
const STAGING_DIR: &str = "attachment-staging";

const UNAVAILABLE: &str = "attachments are unavailable";
const NO_WORKSPACE: &str = "workspace for session not found";
const NOT_IN_WORKSPACE: &str = "chat is not in that workspace";
const NO_WORKTREE: &str = "worktree path unresolved";
const NO_NAME: &str = "missing attachment name";
const EMPTY: &str = "empty attachment";
const TOO_LARGE: &str = "attachments are limited to 25 MB";

/// `POST /api/sessions/:id/attachments?workspaceId=`.
pub(crate) async fn upload_attachment(
    inner: Arc<Inner>,
    session_id: String,
    workspace_id: Option<String>,
    name: String,
    bytes: Bytes,
) -> WriteAnswer {
    if inner.deps.is_none() {
        return unavailable();
    }
    let worktree = match blocking(&inner.reads, "attachments.worktree", move |reads| {
        worktree_of(reads, &session_id, workspace_id.as_deref())
    })
    .await
    {
        Ok(Ok(worktree)) => worktree,
        Ok(Err(answer)) | Err(answer) => return answer,
    };
    if let Some(answer) = refuse(&name, &bytes) {
        return answer;
    }
    match write(worktree, name, bytes, false).await {
        Ok(written) => WriteAnswer::json(
            200,
            json!({
                "ok": true,
                "attachment": {
                    "name": written.name,
                    "path": written.path,
                    "bytes": written.bytes,
                    "token": written.token,
                },
            }),
        ),
        Err(answer) => answer,
    }
}

/// `POST /api/attachments`.
pub(crate) async fn stage_attachment(inner: Arc<Inner>, name: String, bytes: Bytes) -> WriteAnswer {
    let Some(deps) = &inner.deps else {
        return unavailable();
    };
    if let Some(answer) = refuse(&name, &bytes) {
        return answer;
    }
    match write(deps.state_dir.join(STAGING_DIR), name, bytes, true).await {
        Ok(written) => WriteAnswer::json(
            201,
            json!({
                "ok": true,
                "attachment": {
                    "stageId": written.id,
                    "name": written.name,
                    "path": written.path,
                    "bytes": written.bytes,
                    "token": written.token,
                },
            }),
        ),
        Err(answer) => answer,
    }
}

/// `DELETE /api/attachments/:id`.
pub(crate) async fn discard_staged(inner: Arc<Inner>, stage_id: String) -> WriteAnswer {
    let Some(deps) = &inner.deps else {
        return unavailable();
    };
    let root = deps.state_dir.join(STAGING_DIR);
    let removed =
        tokio::task::spawn_blocking(move || attachments::discard_staged(&root, &stage_id)).await;
    match removed {
        Ok(removed) => WriteAnswer::json(if removed { 200 } else { 404 }, json!({ "ok": true })),
        Err(error) => {
            tracing::error!(%error, "discarding a staged attachment did not finish");
            internal()
        }
    }
}

fn unavailable() -> WriteAnswer {
    WriteAnswer::error(503, UNAVAILABLE)
}

/// The worktree an upload goes to, or the answer that refuses it. Blocking.
///
/// The workspace is the one asked for, else the chat's; the chat must belong to it.
fn worktree_of(
    reads: &Reads,
    session_id: &str,
    workspace_id: Option<&str>,
) -> Result<Result<PathBuf, WriteAnswer>, ReadError> {
    let Some(workspace) = reads.write_workspace(workspace_id, Some(session_id))? else {
        return Ok(Err(WriteAnswer::error(404, NO_WORKSPACE)));
    };
    let owner = reads.write_workspace(None, Some(session_id))?;
    if owner.is_none_or(|owner| owner.id != workspace.id) {
        return Ok(Err(WriteAnswer::error(409, NOT_IN_WORKSPACE)));
    }
    let worktree = locate_checkout(reads, &workspace.id)?.and_then(|checkout| checkout.worktree);
    Ok(worktree.ok_or_else(|| WriteAnswer::error(409, NO_WORKTREE)))
}

/// The answer that refuses a name or a body, if either is not acceptable.
fn refuse(name: &str, bytes: &Bytes) -> Option<WriteAnswer> {
    if name.is_empty() {
        Some(WriteAnswer::error(400, NO_NAME))
    } else if bytes.is_empty() {
        Some(WriteAnswer::error(400, EMPTY))
    } else if bytes.len() > MAX_ATTACHMENT_BYTES {
        Some(WriteAnswer::error(413, TOO_LARGE))
    } else {
        None
    }
}

/// Writes the attachment under `root` on the blocking pool; a failure is logged and answers 500.
async fn write(
    root: PathBuf,
    name: String,
    bytes: Bytes,
    private: bool,
) -> Result<Written, WriteAnswer> {
    match tokio::task::spawn_blocking(move || {
        attachments::write_attachment(&root, &name, &bytes, private)
    })
    .await
    {
        Ok(Ok(written)) => Ok(written),
        Ok(Err(error)) => {
            tracing::error!(%error, "could not write an attachment");
            Err(internal())
        }
        Err(error) => {
            tracing::error!(%error, "writing an attachment did not finish");
            Err(internal())
        }
    }
}
