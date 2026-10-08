//! An invented data set for the facts that do not live in the database. Every id starts with
//! `ext-`, except the id of the chat that has a live agent process: the process list names a chat
//! by a UUID, so `LIVE_CHAT` is one.
//!
//! Two live workspaces of the repository `ext-repo`:
//! - `ext-workspace` (branch `ext-draft`) has a worktree once the test creates the directory
//!   `<root>/ext-repo/ext-dir/.git`; its active chat is `LIVE_CHAT`.
//! - `ext-workspace-bare` (branch `ext-merged`) never has one: its directory does not exist.
//!
//! Chats of `ext-workspace`: `LIVE_CHAT` holds the task frames below, and `ext-chat-idle` holds a
//! frame of an open task too but has no agent process in any test.
//!
//! Frames of `LIVE_CHAT`, oldest first:
//! - `task-old` started in 2020, before any process could have been started: never open.
//! - `task-open` started in 2099 and not finished: open once the chat has a live process.
//! - `task-done` started in 2099 and finished by a `task_notification` frame: closed.

use rusqlite::{params, Connection};

pub const REPO_ROOT: &str = "/ext/repos/ext-repo";
pub const WORKSPACE: &str = "ext-workspace";
pub const WORKSPACE_BARE: &str = "ext-workspace-bare";
pub const REPO_NAME: &str = "ext-repo";
pub const DIRECTORY: &str = "ext-dir";
pub const BRANCH_DRAFT: &str = "ext-draft";
pub const BRANCH_MERGED: &str = "ext-merged";
pub const LIVE_CHAT: &str = "0e57ce11-0000-4000-8000-000000000001";
pub const IDLE_CHAT: &str = "ext-chat-idle";

fn frame(subtype: &str, task_id: &str, description: &str) -> String {
    format!(
        "{{\"type\":\"system\",\"subtype\":\"{subtype}\",\"task_id\":\"{task_id}\",\
         \"tool_use_id\":\"toolu_{task_id}\",\"description\":\"{description}\",\
         \"task_type\":\"local_bash\"}}"
    )
}

pub fn seed(conn: &Connection) {
    conn.execute(
        "INSERT INTO repos (id, name, root_path, default_branch, remote_url) \
         VALUES ('ext-repo', ?1, ?2, 'main', 'https://github.com/ext-owner/ext-repo.git')",
        params![REPO_NAME, REPO_ROOT],
    )
    .unwrap();

    for (id, directory, branch, updated_at, active_session_id) in [
        (
            WORKSPACE,
            DIRECTORY,
            BRANCH_DRAFT,
            "2026-03-01 10:00:00",
            Some(LIVE_CHAT),
        ),
        (
            WORKSPACE_BARE,
            "ext-bare",
            BRANCH_MERGED,
            "2026-03-01 09:00:00",
            None,
        ),
    ] {
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, state, \
             created_at, updated_at, active_session_id, workspace_name) \
             VALUES (?1, ?1, 'ext-repo', ?2, ?3, 'ready', '2026-03-01 08:00:00', ?4, ?5, ?6)",
            params![
                id,
                directory,
                branch,
                updated_at,
                active_session_id,
                format!("Extras {branch}")
            ],
        )
        .unwrap();
    }

    for (id, title, created_at) in [
        (LIVE_CHAT, "Wait for the build", "2026-03-01 08:00:00"),
        (IDLE_CHAT, "Nothing running", "2026-03-01 08:30:00"),
    ] {
        conn.execute(
            "INSERT INTO sessions (id, status, title, agent_type, workspace_id, created_at, \
             updated_at) VALUES (?1, 'idle', ?2, 'claude', ?3, ?4, ?4)",
            params![id, title, WORKSPACE, created_at],
        )
        .unwrap();
    }

    let messages = [
        (
            "ext-msg-1",
            LIVE_CHAT,
            "user",
            "{\"type\":\"user\",\"message\":\"start the build\"}".to_owned(),
            "2020-01-01 00:00:00",
        ),
        (
            "ext-msg-2",
            LIVE_CHAT,
            "system",
            frame("task_started", "task-old", "A build of an earlier process"),
            "2020-01-01 00:00:01",
        ),
        (
            "ext-msg-3",
            LIVE_CHAT,
            "system",
            frame("task_started", "task-open", "Watch the build"),
            "2099-01-01 00:00:01",
        ),
        (
            "ext-msg-4",
            LIVE_CHAT,
            "system",
            frame("task_started", "task-done", "Run the linter"),
            "2099-01-01 00:00:02",
        ),
        (
            "ext-msg-5",
            LIVE_CHAT,
            "system",
            frame("task_notification", "task-done", "Run the linter"),
            "2099-01-01 00:00:03",
        ),
        (
            "ext-msg-6",
            IDLE_CHAT,
            "system",
            frame("task_started", "task-idle", "Left behind"),
            "2099-01-01 00:00:04",
        ),
    ];
    for (id, session_id, role, content, created_at) in messages {
        conn.execute(
            "INSERT INTO session_messages (id, session_id, role, content, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, session_id, role, content, created_at],
        )
        .unwrap();
    }
}
