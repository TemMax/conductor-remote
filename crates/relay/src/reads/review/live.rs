//! The lookup of a live workspace's worktree and base branch.

use super::super::workspaces::resolve_worktree;
use super::super::{ReadError, Reads};

/// Where a live workspace's review reads run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTarget {
    pub worktree: Option<String>,
    pub base_branch: String,
}

/// The columns of the query, in the order the SQL selects them.
struct TargetRow {
    directory_name: Option<String>,
    branch: Option<String>,
    intended_target_branch: Option<String>,
    repo_name: Option<String>,
    repo_root: Option<String>,
    default_branch: Option<String>,
}

/// The joins, the state filter and the order of the workspace list, narrowed to one id.
const LIVE_TARGET_SQL: &str = "\
SELECT w.directory_name, w.branch, w.intended_target_branch,
       r.name AS repo_name, r.root_path AS repo_root, r.default_branch AS default_branch
FROM workspaces w
LEFT JOIN repos r ON r.id = w.repository_id
LEFT JOIN sessions s ON s.id = w.active_session_id
WHERE w.state IN ('ready', 'setting_up') AND w.id = ?
ORDER BY (w.pinned_at IS NULL), w.updated_at DESC
LIMIT 1";

impl Reads {
    /// A workspace in state `ready` or `setting_up`, by id; `None` for any other.
    pub fn live_target(&self, workspace_id: &str) -> Result<Option<LiveTarget>, ReadError> {
        let row = self.db().read("live target", |conn| {
            let mut stmt = conn.prepare(LIVE_TARGET_SQL)?;
            let mut rows = stmt.query_map([workspace_id], |r| {
                Ok(TargetRow {
                    directory_name: r.get(0)?,
                    branch: r.get(1)?,
                    intended_target_branch: r.get(2)?,
                    repo_name: r.get(3)?,
                    repo_root: r.get(4)?,
                    default_branch: r.get(5)?,
                })
            })?;
            rows.next().transpose()
        })?;

        // File-system and git work happens here, with the database lock released.
        Ok(row.map(|row| {
            let worktree = resolve_worktree(
                self.workspaces_root(),
                row.repo_name.as_deref(),
                row.directory_name.as_deref(),
                row.branch.as_deref(),
                row.repo_root.as_deref(),
            )
            .map(|p| p.to_string_lossy().into_owned());
            let base_branch = [
                row.intended_target_branch.as_deref(),
                row.default_branch.as_deref(),
            ]
            .into_iter()
            .flatten()
            .find(|b| !b.is_empty())
            .unwrap_or("main")
            .to_owned();
            LiveTarget {
                worktree,
                base_branch,
            }
        }))
    }
}
