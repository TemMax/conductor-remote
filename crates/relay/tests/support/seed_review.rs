//! A small invented data set for the review reads: one repository and three workspaces.
//!
//! `LIVE` is ready, in the directory `rev-live-dir` under the repository's folder of the
//! workspaces root; the test makes that a real repository. `ARCHIVED` is not live. `NO_WORKTREE`
//! is ready, but its directory does not exist and it has no branch to find a worktree by.

use rusqlite::{params, Connection};

/// The repository's name and id.
pub const REPO: &str = "rev-repo";
/// A ready workspace; directory `rev-live-dir`, branch `rev-branch`, target branch `main`.
pub const LIVE: &str = "rev-live";
/// An archived workspace.
pub const ARCHIVED: &str = "rev-archived";
/// A ready workspace whose directory does not exist and that has no branch.
pub const NO_WORKTREE: &str = "rev-no-worktree";

/// Inserts the repository, with `repo_root` as its root path, and the three workspaces.
pub fn seed(conn: &Connection, repo_root: &str) {
    conn.execute(
        "INSERT INTO repos (id, name, root_path, default_branch) VALUES (?1, ?1, ?2, 'main')",
        params![REPO, repo_root],
    )
    .expect("insert repo");

    // (id, directory, branch, state, intended target branch)
    let workspaces = [
        (
            LIVE,
            "rev-live-dir",
            Some("rev-branch"),
            "ready",
            Some("main"),
        ),
        (
            ARCHIVED,
            "rev-archived-dir",
            Some("rev-archived-branch"),
            "archived",
            Some("main"),
        ),
        (NO_WORKTREE, "rev-missing-dir", None, "ready", None),
    ];
    for (id, directory, branch, state, target) in workspaces {
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, state,
                                     created_at, updated_at, intended_target_branch)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5, '2026-01-01 00:00:00', '2026-01-02 00:00:00', ?6)",
            params![id, REPO, directory, branch, state, target],
        )
        .expect("insert workspace");
    }
}
