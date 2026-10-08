#![allow(dead_code)]
//! A small invented data set for the search results. Every id starts with `srch-`.
//!
//! Repos: `srch-one` (explicit emoji icon), `srch-two` (a GitHub remote), `srch-three` (a root
//! directory under `root`, where a test may put an icon file).
//!
//! Workspaces, with the chat each owns:
//! - `srch-live` (one, `ready`, updated 2026-09-02): chats `srch-chat-live` and `srch-chat-live-2`
//! - `srch-archived` (one, `archived`, 2026-09-01): `srch-chat-archived`
//! - `srch-unknown` (two, no state, 2026-08-31): `srch-chat-unknown`
//! - `srch-quiet` (two, `ready`): `srch-chat-quiet`
//! - `srch-calm` (two, `ready`): `srch-chat-calm`
//! - `srch-shelved` (three, `archived`): `srch-chat-shelved`
//! - `srch-beacon` (two, `ready`): no chat
//! - `srch-under` and `srch-underx` (two, `ready`): no chat; their names differ in one character
//!
//! `srch-chat-orphan` belongs to no workspace.

use std::path::Path;

use rusqlite::{params, Connection};

/// The repo named `srch-three` keeps its root here, under the workspaces root.
pub fn repo_three_root(root: &Path) -> std::path::PathBuf {
    root.join("_repos").join("srch-three")
}

pub fn seed(conn: &Connection, root: &Path) {
    let three_root = repo_three_root(root).to_string_lossy().into_owned();
    #[allow(clippy::type_complexity)]
    let repos: [(&str, &str, Option<&str>, Option<&str>, Option<&str>); 3] = [
        ("srch-repo-one", "srch-one", Some("emoji:🔦"), None, None),
        (
            "srch-repo-two",
            "srch-two",
            None,
            None,
            Some("git@github.com:acme/lamps.git"),
        ),
        (
            "srch-repo-three",
            "srch-three",
            None,
            Some(&three_root),
            None,
        ),
    ];
    for (id, name, icon, root_path, remote_url) in repos {
        conn.execute(
            "INSERT INTO repos (id, name, icon, root_path, remote_url) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, icon, root_path, remote_url],
        )
        .expect("insert repo");
    }

    // id, repo, directory, branch, state, updated_at, workspace_name, pr_title
    #[allow(clippy::type_complexity)]
    let workspaces: [(
        &str,
        &str,
        &str,
        Option<&str>,
        Option<&str>,
        &str,
        Option<&str>,
        Option<&str>,
    ); 9] = [
        (
            "srch-live",
            "srch-repo-one",
            "wick-v1",
            Some("feat/wick"),
            Some("ready"),
            "2026-09-02",
            Some("Lantern current"),
            None,
        ),
        (
            "srch-archived",
            "srch-repo-one",
            "wick-v2",
            Some("fix/wick"),
            Some("archived"),
            "2026-09-01",
            Some("Lantern old"),
            None,
        ),
        (
            "srch-unknown",
            "srch-repo-two",
            "wick-v3",
            Some("try/wick"),
            None,
            "2026-08-31",
            Some("Lantern unknown"),
            None,
        ),
        (
            "srch-quiet",
            "srch-repo-two",
            "shelf",
            Some("shelf"),
            Some("ready"),
            "2026-07-01",
            Some("Quiet shelf"),
            None,
        ),
        (
            "srch-calm",
            "srch-repo-two",
            "harbour",
            Some("harbour"),
            Some("ready"),
            "2026-07-02",
            Some("Calm harbour"),
            None,
        ),
        (
            "srch-shelved",
            "srch-repo-three",
            "attic",
            Some("attic"),
            Some("archived"),
            "2026-06-01",
            Some("Shelved attic"),
            None,
        ),
        (
            "srch-beacon",
            "srch-repo-two",
            "tower",
            Some("tower"),
            Some("ready"),
            "2026-05-01",
            Some("Beacon"),
            None,
        ),
        (
            "srch-under",
            "srch-repo-two",
            "cellar-a",
            Some("cellar-a"),
            Some("ready"),
            "2026-05-02",
            Some("under_score tool"),
            None,
        ),
        (
            "srch-underx",
            "srch-repo-two",
            "cellar-b",
            Some("cellar-b"),
            Some("ready"),
            "2026-05-03",
            Some("underXscore tool"),
            None,
        ),
    ];
    for (id, repo, directory, branch, state, updated_at, name, pr_title) in workspaces {
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, state,
                                     created_at, updated_at, workspace_name, pr_title)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?7, ?6, ?8, ?9)",
            params![
                id,
                repo,
                directory,
                branch,
                state,
                updated_at,
                "2026-01-01",
                name,
                pr_title
            ],
        )
        .expect("insert workspace");
    }

    let sessions: [(&str, Option<&str>, &str); 8] = [
        ("srch-chat-live", Some("srch-live"), "Trim the wick"),
        ("srch-chat-live-2", Some("srch-live"), "Second wick chat"),
        ("srch-chat-archived", Some("srch-archived"), "Old wick talk"),
        (
            "srch-chat-unknown",
            Some("srch-unknown"),
            "Unknown wick talk",
        ),
        ("srch-chat-quiet", Some("srch-quiet"), "Shelf talk"),
        ("srch-chat-calm", Some("srch-calm"), "Harbour talk"),
        ("srch-chat-shelved", Some("srch-shelved"), "Attic talk"),
        ("srch-chat-orphan", None, "Nowhere"),
    ];
    for (id, workspace_id, title) in sessions {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, title) VALUES (?1, ?2, ?3)",
            params![id, workspace_id, title],
        )
        .expect("insert session");
    }
}

/// A typed prompt of `session`, stamped `created_at`; its source rowid follows the insert order.
pub fn say(conn: &Connection, id: &str, session: &str, created_at: &str, text: &str) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content, created_at)
         VALUES (?1, ?2, 'user', ?3, ?4)",
        params![id, session, text, created_at],
    )
    .expect("insert message");
}
