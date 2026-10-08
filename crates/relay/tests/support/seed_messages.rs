//! An invented data set for the message reads. The golden chat is `msg-chat`: user prompts,
//! assistant frames with tool calls and results, a prompt that was queued at one time and the
//! outbox of waiting prompts. Beside it: `msg-other`, a chat that must never leak into the
//! first, `msg-legacy`, which holds a prompt queued the way builds without an outbox table
//! queued it, and `msg-tree`, a chat whose workspace directory a test may create.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub const CHAT: &str = "msg-chat";
pub const OTHER: &str = "msg-other";
pub const LEGACY: &str = "msg-legacy";
pub const TREE: &str = "msg-tree";
/// The directory of `msg-tree`'s workspace and the name of its repository: with the workspaces
/// root they make the path `<root>/msg-project/msg-tree-dir` a test can give a `.git` to.
pub const TREE_REPO: &str = "msg-project";
pub const TREE_DIRECTORY: &str = "msg-tree-dir";

/// The rowid of the first row of `msg-chat`, and of its last (a frame with no entry).
pub const CHAT_FIRST_ROWID: i64 = 1;
pub const CHAT_LAST_ROWID: i64 = 9;

fn insert_chat(conn: &Connection, id: &str, workspace: &str) {
    conn.execute(
        "INSERT INTO sessions (id, status, title, workspace_id, created_at, updated_at) \
         VALUES (?1, 'idle', 'Untitled', ?2, '2026-09-14 09:00:00', '2026-09-14 09:00:00')",
        params![id, workspace],
    )
    .unwrap();
}

/// A durable row; its timestamp is the invented minute 10:00 plus `second`.
fn insert_message(
    conn: &Connection,
    id: &str,
    session: &str,
    second: u32,
    content: &str,
    sent: bool,
    queue_order: Option<i64>,
) {
    let stamp = format!("2026-09-14T10:00:{second:02}.000Z");
    conn.execute(
        "INSERT INTO session_messages (id, session_id, content, created_at, sent_at, queue_order) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            session,
            content,
            stamp,
            sent.then_some(&stamp),
            queue_order
        ],
    )
    .unwrap();
}

fn frame(value: Value) -> String {
    value.to_string()
}

/// An outbox row of `msg-` chats; its timestamp is the invented minute 10:05 plus `second`.
fn insert_outbox(
    conn: &Connection,
    id: &str,
    session: &str,
    payload: &str,
    mode: &str,
    queue_order: Option<i64>,
    second: u32,
) {
    conn.execute(
        "INSERT INTO session_messages_outbox \
         (message_id, session_id, delivery_payload, mode, queue_order, state, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
        params![
            id,
            session,
            payload,
            mode,
            queue_order,
            format!("2026-09-14T10:05:{second:02}.000Z")
        ],
    )
    .unwrap();
}

pub fn seed(conn: &Connection) {
    conn.execute(
        "INSERT INTO repos (id, name, root_path) VALUES ('msg-repo', ?1, '/msg/absent/repo')",
        [TREE_REPO],
    )
    .unwrap();
    // Neither workspace has a branch, so no worktree is ever looked up with git.
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, directory_name) \
         VALUES ('msg-workspace', 'msg-workspace', 'msg-repo', 'msg-absent-dir')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, directory_name) \
         VALUES ('msg-workspace-tree', 'msg-workspace-tree', 'msg-repo', ?1)",
        [TREE_DIRECTORY],
    )
    .unwrap();
    insert_chat(conn, CHAT, "msg-workspace");
    insert_chat(conn, OTHER, "msg-workspace");
    insert_chat(conn, LEGACY, "msg-workspace");
    insert_chat(conn, TREE, "msg-workspace-tree");

    // Rowids follow insertion order; the other chat's rows sit between the golden chat's.
    insert_message(
        conn,
        "msg-u1",
        CHAT,
        1,
        "Add a retry to the fetch helper.",
        true,
        None,
    );
    insert_message(
        conn,
        "msg-o1",
        OTHER,
        2,
        "A prompt of the other chat.",
        true,
        None,
    );
    insert_message(
        conn,
        "msg-a1",
        CHAT,
        3,
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "thinking", "thinking": "Read the helper before changing it." },
            { "type": "text", "text": "I'll read the helper first." },
            { "type": "tool_use", "id": "msg-tool-1", "name": "Read",
              "input": { "file_path": "src/fetch.ts" } }
        ] } })),
        true,
        None,
    );
    insert_message(
        conn,
        "msg-r1",
        CHAT,
        4,
        &frame(json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "msg-tool-1",
              "content": "export async function fetchJson(url) { return fetch(url) }" }
        ] } })),
        true,
        None,
    );
    insert_message(
        conn,
        "msg-a2",
        CHAT,
        5,
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "msg-tool-2", "name": "Bash",
              "input": { "description": "Run the tests", "command": "npm test" } }
        ] } })),
        true,
        None,
    );
    insert_message(
        conn,
        "msg-r2",
        CHAT,
        6,
        &frame(json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "msg-tool-2", "is_error": true,
              "content": "1 test failed: fetchJson retries" }
        ] } })),
        true,
        None,
    );
    insert_message(
        conn,
        "msg-o2",
        OTHER,
        7,
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "An answer in the other chat." }
        ] } })),
        true,
        None,
    );
    insert_message(
        conn,
        "msg-a3",
        CHAT,
        8,
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "The retry is in; the failing test needs a longer timeout." }
        ] } })),
        true,
        None,
    );
    // The end of the turn: a row with no entry, so the cursor moves past the last entry.
    insert_message(
        conn,
        "msg-end",
        CHAT,
        9,
        &frame(json!({ "type": "result", "subtype": "success" })),
        true,
        None,
    );

    // The legacy shape: queued in the row itself, not yet sent.
    insert_message(
        conn,
        "msg-l1",
        LEGACY,
        10,
        "Waiting in the row.",
        false,
        Some(1),
    );

    // The queue of the golden chat, inserted out of order. Sending order: first, second,
    // second-later (same queue_order, later creation), unnumbered. The steer row is not
    // queue mode; the next three payloads cannot be rendered.
    let message = |text: &str| json!({ "message": text }).to_string();
    insert_outbox(
        conn,
        "msg-q-second",
        CHAT,
        &message("Then update the docs."),
        "queue",
        Some(2),
        2,
    );
    insert_outbox(
        conn,
        "msg-q-steer",
        CHAT,
        &message("Stop and rethink."),
        "steer",
        Some(1),
        4,
    );
    insert_outbox(
        conn,
        "msg-q-unnumbered",
        CHAT,
        &message("  Last in line.  "),
        "queue",
        None,
        1,
    );
    insert_outbox(
        conn,
        "msg-q-first",
        CHAT,
        &message("Run the linter."),
        "queue",
        Some(1),
        3,
    );
    insert_outbox(
        conn,
        "msg-q-second-later",
        CHAT,
        &message("And bump the version."),
        "queue",
        Some(2),
        5,
    );
    insert_outbox(conn, "msg-q-broken", CHAT, "{", "queue", Some(3), 6);
    insert_outbox(conn, "msg-q-no-message", CHAT, "{}", "queue", Some(4), 7);
    insert_outbox(
        conn,
        "msg-q-blank",
        CHAT,
        &message("   "),
        "queue",
        Some(5),
        8,
    );
    // The other chat's queue.
    insert_outbox(
        conn,
        "msg-q-other",
        OTHER,
        &message("Queued in the other chat."),
        "queue",
        Some(1),
        9,
    );
}
