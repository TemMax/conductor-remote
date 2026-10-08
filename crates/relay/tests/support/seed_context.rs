//! An invented data set for the context breakdown. Every id starts with `ctx-`. The golden chat
//! is `ctx-chat`: prompts, assistant frames with thinking and tool calls, tool results, a turn
//! that ends in a result frame, a compaction boundary, a second turn after it, and a prompt
//! that has not been answered yet. Beside it: `ctx-other`, whose rows sit between the golden
//! chat's and must never leak into it, and `ctx-closed`, a chat that was closed (hidden).

use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub const CHAT: &str = "ctx-chat";
pub const OTHER: &str = "ctx-other";
pub const CLOSED: &str = "ctx-closed";

/// What Conductor stored for `ctx-chat` after its last completed turn.
pub const CHAT_TOKENS: i64 = 9_000;
pub const CHAT_PERCENT: f64 = 7.5;

pub fn insert_chat(conn: &Connection, id: &str, hidden: bool, tokens: i64, percent: f64) {
    conn.execute(
        "INSERT INTO sessions \
         (id, status, title, workspace_id, is_hidden, context_token_count, context_used_percent, \
          created_at, updated_at) \
         VALUES (?1, 'idle', 'Untitled', 'ctx-workspace', ?2, ?3, ?4, \
                 '2026-09-14 09:00:00', '2026-09-14 09:00:00')",
        params![id, i64::from(hidden), tokens, percent],
    )
    .unwrap();
}

/// A row of a `ctx-` chat; its timestamp is the invented hour 10:00 plus `second` seconds.
pub fn insert_message(
    conn: &Connection,
    id: &str,
    session: &str,
    second: u32,
    role: &str,
    content: &str,
) {
    let stamp = format!(
        "2026-09-14T10:{:02}:{:02}.000Z",
        second / 60 % 60,
        second % 60
    );
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content, created_at, sent_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![id, session, role, content, stamp],
    )
    .unwrap();
}

pub fn frame(value: Value) -> String {
    value.to_string()
}

pub fn seed(conn: &Connection) {
    // Neither the repository nor the workspace has a directory or a branch, so the worktree
    // of a chat is never found and git is never run.
    conn.execute(
        "INSERT INTO repos (id, name, root_path) VALUES ('ctx-repo', 'ctx-project', '/ctx/absent/repo')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, directory_name) \
         VALUES ('ctx-workspace', 'ctx-workspace', 'ctx-repo', 'ctx-absent-dir')",
        [],
    )
    .unwrap();
    insert_chat(conn, CHAT, false, CHAT_TOKENS, CHAT_PERCENT);
    insert_chat(conn, OTHER, false, 500, 1.5);
    insert_chat(conn, CLOSED, true, 4_000, 3.5);

    // First turn.
    insert_message(
        conn,
        "ctx-u1",
        CHAT,
        1,
        "user",
        "Add a retry to the fetch helper.",
    );
    insert_message(
        conn,
        "ctx-o1",
        OTHER,
        2,
        "user",
        "A prompt of the other chat.",
    );
    insert_message(
        conn,
        "ctx-a1",
        CHAT,
        3,
        "assistant",
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "thinking", "thinking": "Read the helper before changing it.",
              "signature": "ctx-opaque-signature-that-is-not-counted" },
            { "type": "text", "text": "I'll read the helper first." },
            { "type": "tool_use", "id": "ctx-tool-1", "name": "Read",
              "input": { "file_path": "src/fetch.ts" } }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-r1",
        CHAT,
        4,
        "user",
        &frame(json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "ctx-tool-1",
              "content": "export async function fetchJson(url) { return fetch(url) }" }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-a2",
        CHAT,
        5,
        "assistant",
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "ctx-tool-2", "name": "Bash",
              "input": { "description": "Run the tests", "command": "npm test" } }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-o2",
        OTHER,
        6,
        "assistant",
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "An answer in the other chat." }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-r2",
        CHAT,
        7,
        "user",
        &frame(json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "ctx-tool-2", "is_error": true,
              "content": "1 test failed: fetchJson retries" }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-a3",
        CHAT,
        8,
        "assistant",
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "The retry is in; the failing test needs a longer timeout." }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-end1",
        CHAT,
        9,
        "assistant",
        &frame(json!({ "type": "result", "subtype": "success" })),
    );

    // The window is compacted: what came before the boundary is no longer counted.
    insert_message(
        conn,
        "ctx-boundary",
        CHAT,
        10,
        "system",
        &frame(json!({ "type": "system", "subtype": "compact_boundary" })),
    );
    insert_message(
        conn,
        "ctx-summary",
        CHAT,
        11,
        "user",
        "Summary of the session so far: the fetch helper retries, one test needs a longer timeout.",
    );

    // Second turn, after the compaction.
    insert_message(
        conn,
        "ctx-u2",
        CHAT,
        12,
        "user",
        "Raise the timeout of the failing test.",
    );
    insert_message(
        conn,
        "ctx-a4",
        CHAT,
        13,
        "assistant",
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "thinking", "thinking": "The test waits one second; two will do." },
            { "type": "text", "text": "Raising the timeout." },
            { "type": "tool_use", "id": "ctx-tool-3", "name": "Edit",
              "input": { "file_path": "src/fetch.test.ts", "old_string": "1000", "new_string": "2000" } }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-r3",
        CHAT,
        14,
        "user",
        &frame(json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "ctx-tool-3",
              "content": "The file src/fetch.test.ts has been updated." }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-a5",
        CHAT,
        15,
        "assistant",
        &frame(json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "The timeout is two seconds now." }
        ] } })),
    );
    insert_message(
        conn,
        "ctx-end2",
        CHAT,
        16,
        "assistant",
        &frame(json!({ "type": "result", "subtype": "success" })),
    );

    // A prompt that no turn has completed yet: it is part of what a fork copies, not of the
    // counted window.
    insert_message(
        conn,
        "ctx-u3",
        CHAT,
        17,
        "user",
        "Now document the retry in the readme.",
    );

    // The closed chat has rows too; they are never read.
    insert_message(
        conn,
        "ctx-c1",
        CLOSED,
        18,
        "user",
        "A prompt of the closed chat.",
    );
}
