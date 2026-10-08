//! Merging a workspace's branch.
//!
//! `gh pr merge <branch>` resolves the pull request from its head branch, so no number is needed;
//! GitHub does the merge, nothing is pushed or checked out here. Every `gh` call runs in the
//! repository's checkout, on the blocking pool.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use super::service::{blocking, internal, locate_checkout, Inner, NO_WORKSPACE};
use super::WriteAnswer;
use crate::reads::extras::commands::{CommandError, Commands, Limits, Output};

/// How long each `gh` call may take.
const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// How much a `gh` call may print before it counts as too much.
const GH_MAX_OUTPUT: usize = 1024 * 1024;

const NO_BRANCH: &str = "workspace has no branch";
const NO_REPO_ROOT: &str = "repo root unresolved";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Squash,
    Merge,
    Rebase,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Squash => "squash",
            Method::Merge => "merge",
            Method::Rebase => "rebase",
        }
    }
}

/// What `gh repo view --json …` prints.
#[derive(Deserialize)]
struct Allowed {
    #[serde(rename = "squashMergeAllowed", default)]
    squash: bool,
    #[serde(rename = "mergeCommitAllowed", default)]
    merge: bool,
    #[serde(rename = "rebaseMergeAllowed", default)]
    rebase: bool,
}

/// `POST /api/workspaces/:id/merge`.
pub(crate) async fn merge(inner: Arc<Inner>, workspace_id: String) -> WriteAnswer {
    let Some(deps) = inner.deps.as_ref() else {
        tracing::error!("a merge was asked before the writes were configured");
        return internal();
    };
    let commands = Arc::clone(&deps.commands);

    let located = blocking(&inner.reads, "merge.workspace", move |reads| {
        let Some(workspace) = reads.write_workspace(Some(&workspace_id), None)? else {
            return Ok(None);
        };
        let checkout = locate_checkout(reads, &workspace.id)?;
        Ok(Some((workspace, checkout)))
    })
    .await;
    let (workspace, checkout) = match located {
        Err(answer) => return answer,
        Ok(None | Some((_, None))) => return WriteAnswer::error(404, NO_WORKSPACE),
        Ok(Some((workspace, Some(checkout)))) => (workspace, checkout),
    };

    let branch = workspace.branch.unwrap_or_default();
    // A branch that starts with `-` is refused before it can be read as an option.
    if branch.is_empty() || branch.starts_with('-') {
        return refused(&branch, NO_BRANCH);
    }
    let Some(root) = checkout.repo_root else {
        return refused(&branch, NO_REPO_ROOT);
    };

    let job = {
        let branch = branch.clone();
        move || run_merge(commands.as_ref(), &root, &branch)
    };
    match tokio::task::spawn_blocking(job).await {
        Ok(answer) => answer,
        Err(error) => {
            tracing::error!(%error, "a merge did not finish");
            internal()
        }
    }
}

/// 409 `{"ok":false,"branch":…,"error":…}`.
fn refused(branch: &str, error: &str) -> WriteAnswer {
    WriteAnswer::json(
        409,
        json!({ "ok": false, "branch": branch, "error": error }),
    )
}

/// The method and the merge itself. Blocking.
fn run_merge(commands: &dyn Commands, root: &str, branch: &str) -> WriteAnswer {
    let method = preferred_method(commands, root).name();
    let failed = |error: &str| {
        WriteAnswer::json(
            409,
            json!({ "ok": false, "branch": branch, "method": method, "error": error }),
        )
    };
    let flag = format!("--{method}");
    match gh(commands, root, &["pr", "merge", branch, &flag]) {
        Err(error) => failed(&error.to_string()),
        Ok(output) if output.code == Some(0) => WriteAnswer::json(
            200,
            json!({ "ok": true, "branch": branch, "method": method }),
        ),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            if !stderr.is_empty() {
                failed(stderr)
            } else if let Some(code) = output.code {
                failed(&format!("gh exited with {code}"))
            } else {
                failed("gh was stopped by a signal")
            }
        }
    }
}

/// Squash when the repository allows it, else a merge commit, else rebase; squash when `gh`
/// cannot tell (missing, not a GitHub repository, unreadable answer): the merge call surfaces any
/// real error.
fn preferred_method(commands: &dyn Commands, root: &str) -> Method {
    let args = [
        "repo",
        "view",
        "--json",
        "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed",
    ];
    let allowed = gh(commands, root, &args)
        .ok()
        .filter(|output| output.code == Some(0))
        .and_then(|output| serde_json::from_slice::<Allowed>(&output.stdout).ok());
    match allowed {
        Some(Allowed { squash: true, .. }) | None => Method::Squash,
        Some(Allowed { merge: true, .. }) => Method::Merge,
        Some(Allowed { rebase: true, .. }) => Method::Rebase,
        Some(_) => Method::Squash,
    }
}

fn gh(commands: &dyn Commands, root: &str, args: &[&str]) -> Result<Output, CommandError> {
    let limits = Limits {
        timeout: GH_TIMEOUT,
        max_stdout: GH_MAX_OUTPUT,
    };
    commands.run("gh", args, Some(Path::new(root)), limits)
}
