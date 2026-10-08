//! The message read: the transcript after a cursor plus the queue snapshot.
//!
//! Every row comes from the invented database of `support/seed_messages.rs`. The three queue
//! cases port the reference tests "returns queue-mode outbox messages as an ordered queued
//! snapshot", "ignores an outbox payload it cannot safely render" and "keeps the legacy in-row
//! queue signal when the outbox table is absent", against a real SQLite file.

#[path = "support/seed_messages.rs"]
mod seed_messages;
mod support;

use conductor_remote::reads::messages::MessagesResponse;
use conductor_remote::reads::Reads;
use seed_messages::{CHAT, CHAT_FIRST_ROWID, CHAT_LAST_ROWID, LEGACY, OTHER, TREE};
use serde_json::{json, Value};
use support::TestDb;

/// The relay's reads over the seeded database. The `TestDb` is returned too: it owns the files.
fn seeded() -> (TestDb, Reads) {
    let test = TestDb::new();
    seed_messages::seed(&test.conn());
    let reads = Reads::new(test.db(), test.root());
    (test, reads)
}

fn read(chat: &str, after: i64) -> MessagesResponse {
    let (_test, reads) = seeded();
    reads.get_messages(chat, after).unwrap()
}

fn ids(entries: &[conductor_remote::transcript::TranscriptEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.id.as_str()).collect()
}

fn queued_ids(chat: &str) -> Vec<String> {
    read(chat, 0).queued.into_iter().map(|e| e.id).collect()
}

#[test]
fn get_messages_matches_the_golden_json() {
    let actual = serde_json::to_value(read(CHAT, 0)).unwrap();
    let golden: Value = serde_json::from_str(include_str!(
        "../../../web/tests/contract/fixtures/messages.json"
    ))
    .unwrap();
    assert_eq!(actual, golden);
}

#[test]
fn the_json_keys_are_in_the_order_of_the_wire_type() {
    let text = serde_json::to_string(&read(CHAT, 0)).unwrap();
    // The entries carry a `queued` flag of their own: the queue is the one holding an array.
    let (entries, cursor, queued) = (
        text.find("\"entries\":[").unwrap(),
        text.find("\"cursor\":9").unwrap(),
        text.find("\"queued\":[").unwrap(),
    );
    assert_eq!(entries, 1);
    assert!(entries < cursor && cursor < queued);
}

#[test]
fn a_cursor_of_zero_returns_the_whole_chat() {
    let all = read(CHAT, 0);
    assert_eq!(
        ids(&all.entries),
        [
            "msg-u1", "msg-a1:0", "msg-a1:1", "msg-a1:2", "msg-r1:0", "msg-a2:0", "msg-r2:0",
            "msg-a3:0"
        ]
    );
    assert_eq!(all.cursor, CHAT_LAST_ROWID);
}

#[test]
fn entries_are_the_rows_after_the_cursor_and_the_row_itself_is_excluded() {
    // Row 4 is `msg-r1`, which belongs to the chat: `after` is exclusive.
    let response = read(CHAT, 4);
    assert_eq!(ids(&response.entries), ["msg-a2:0", "msg-r2:0", "msg-a3:0"]);
    assert_eq!(response.cursor, CHAT_LAST_ROWID);
}

#[test]
fn the_cursor_is_the_last_row_read_even_when_it_has_no_entry() {
    // Row 9 closes the turn and renders nothing, yet the next poll must start after it.
    let response = read(CHAT, 8);
    assert!(response.entries.is_empty());
    assert_eq!(response.cursor, CHAT_LAST_ROWID);
}

#[test]
fn an_empty_result_echoes_the_cursor_it_was_given() {
    for after in [CHAT_LAST_ROWID, 40, i64::MAX] {
        let response = read(CHAT, after);
        assert!(response.entries.is_empty(), "after {after}");
        assert_eq!(response.cursor, after);
    }
}

#[test]
fn a_negative_cursor_reads_from_the_start() {
    let response = read(CHAT, -5);
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        serde_json::to_value(read(CHAT, 0)).unwrap()
    );
    assert_eq!(response.cursor, CHAT_LAST_ROWID);
}

#[test]
fn polling_with_the_returned_cursor_neither_skips_nor_repeats() {
    let (test, reads) = seeded();
    let first = reads.get_messages(CHAT, 0).unwrap();
    assert_eq!(first.entries.len(), 8);

    // Nothing new: the next poll is empty and the cursor stays.
    let second = reads.get_messages(CHAT, first.cursor).unwrap();
    assert!(second.entries.is_empty());
    assert_eq!(second.cursor, first.cursor);

    // A row written after the first poll is the only entry of the next one.
    let conn = test.conn();
    conn.execute(
        "INSERT INTO session_messages (id, session_id, content, created_at, sent_at) \
         VALUES ('msg-u2', ?1, 'One more thing.', '2026-09-14T10:00:12.000Z', '2026-09-14T10:00:12.000Z')",
        [CHAT],
    )
    .unwrap();
    let third = reads.get_messages(CHAT, second.cursor).unwrap();
    assert_eq!(ids(&third.entries), ["msg-u2"]);
    assert_eq!(third.cursor, third.entries[0].rowid);
    assert!(third.cursor > first.cursor);
}

#[test]
fn a_cursor_in_the_middle_of_a_chat_yields_the_rest_of_the_full_read() {
    let (_test, reads) = seeded();
    let full = reads.get_messages(CHAT, 0).unwrap().entries;
    for after in CHAT_FIRST_ROWID..=CHAT_LAST_ROWID {
        let rest = reads.get_messages(CHAT, after).unwrap().entries;
        let expected: Vec<_> = full.iter().filter(|e| e.rowid > after).cloned().collect();
        assert_eq!(rest, expected, "after {after}");
    }
}

#[test]
fn another_chats_rows_never_appear() {
    let all = read(CHAT, 0);
    assert!(all
        .entries
        .iter()
        .all(|e| !e.id.starts_with("msg-o") && !e.id.starts_with("msg-l")));

    let other = read(OTHER, 0);
    assert_eq!(ids(&other.entries), ["msg-o1", "msg-o2:0"]);
    assert_eq!(other.cursor, 7);
}

#[test]
fn another_chats_queue_never_appears() {
    assert!(!queued_ids(CHAT).contains(&"msg-q-other".to_owned()));
    assert_eq!(queued_ids(OTHER), ["msg-q-other"]);
    assert!(queued_ids(LEGACY).is_empty());
}

#[test]
fn an_unknown_chat_is_not_an_error_and_is_empty() {
    let response = read("msg-nobody", 12);
    assert!(response.entries.is_empty());
    assert_eq!(response.cursor, 12);
    assert!(response.queued.is_empty());
}

#[test]
fn the_queue_is_in_sending_order_and_leaves_out_other_modes() {
    // Same `queue_order` falls back to creation time; no `queue_order` comes last; the steer
    // row is another mode; the payloads of the last three rows cannot be rendered.
    assert_eq!(
        queued_ids(CHAT),
        [
            "msg-q-first",
            "msg-q-second",
            "msg-q-second-later",
            "msg-q-unnumbered"
        ]
    );
}

#[test]
fn a_queued_entry_has_the_shape_of_a_prompt_that_is_not_yet_in_the_transcript() {
    let queued = serde_json::to_value(read(CHAT, 0).queued).unwrap();
    assert_eq!(
        queued[0],
        json!({
            "id": "msg-q-first",
            "rowid": 0,
            "role": "user",
            "text": "Run the linter.",
            "ts": "2026-09-14T10:05:03.000Z",
            "queued": true
        })
    );
    // The text is trimmed.
    assert_eq!(queued[3]["text"], "Last in line.");
}

#[test]
fn the_queue_is_the_full_snapshot_whatever_the_cursor() {
    let expected = queued_ids(CHAT);
    for after in [0, 5, CHAT_LAST_ROWID, 500] {
        let queued: Vec<_> = read(CHAT, after).queued.into_iter().map(|e| e.id).collect();
        assert_eq!(queued, expected, "after {after}");
    }
}

#[test]
fn a_chat_with_only_a_queue_has_no_entries_and_keeps_the_cursor() {
    let test = TestDb::new();
    let conn = test.conn();
    seed_messages::seed(&conn);
    conn.execute("DELETE FROM session_messages WHERE session_id = ?1", [CHAT])
        .unwrap();
    let response = Reads::new(test.db(), test.root())
        .get_messages(CHAT, 3)
        .unwrap();
    assert!(response.entries.is_empty());
    assert_eq!(response.cursor, 3);
    assert_eq!(response.queued.len(), 4);
}

#[test]
fn an_outbox_payload_that_cannot_be_rendered_is_dropped() {
    // Not JSON, no `message`, and a blank `message` are in the queue-mode rows of the seed;
    // only the four renderable ones remain, and nothing fails.
    let test = TestDb::new();
    let conn = test.conn();
    seed_messages::seed(&conn);
    let in_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_messages_outbox WHERE session_id = ?1 AND mode = 'queue'",
            [CHAT],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(in_table, 7);
    let response = Reads::new(test.db(), test.root())
        .get_messages(CHAT, 0)
        .unwrap();
    assert_eq!(response.queued.len(), 4);
    for dropped in [
        "msg-q-broken",
        "msg-q-no-message",
        "msg-q-blank",
        "msg-q-steer",
    ] {
        assert!(
            !response.queued.iter().any(|e| e.id == dropped),
            "{dropped}"
        );
    }
}

#[test]
fn without_the_outbox_table_the_queue_is_empty_and_the_legacy_row_keeps_its_flag() {
    let test = TestDb::new();
    let conn = test.conn();
    seed_messages::seed(&conn);
    conn.execute("DROP TABLE session_messages_outbox", [])
        .unwrap();
    let reads = Reads::new(test.db(), test.root());

    let legacy = reads.get_messages(LEGACY, 0).unwrap();
    assert_eq!(legacy.queued, []);
    assert_eq!(legacy.entries.len(), 1);
    assert_eq!(legacy.entries[0].id, "msg-l1");
    assert_eq!(legacy.entries[0].text, "Waiting in the row.");
    assert!(legacy.entries[0].queued);

    // The transcript of a chat with an outbox is read as before.
    let chat = reads.get_messages(CHAT, 0).unwrap();
    assert_eq!(chat.queued, []);
    assert_eq!(chat.entries.len(), 8);
    assert_eq!(chat.cursor, CHAT_LAST_ROWID);
}

#[test]
fn the_legacy_flag_is_also_set_while_the_outbox_exists() {
    let response = read(LEGACY, 0);
    assert!(response.entries[0].queued);
    assert!(response.queued.is_empty());
}

#[test]
fn the_worktree_of_the_chats_workspace_shortens_paths_in_tool_details() {
    let test = TestDb::new();
    let conn = test.conn();
    seed_messages::seed(&conn);
    let worktree = test
        .root()
        .join(seed_messages::TREE_REPO)
        .join(seed_messages::TREE_DIRECTORY);
    let worktree_text = worktree.to_str().unwrap();
    let content = json!({ "type": "assistant", "message": { "content": [{
        "type": "tool_use", "id": "msg-tool-tree", "name": "Bash",
        "input": { "command": format!("cd {worktree_text} && ls {worktree_text}/src") }
    }] } })
    .to_string();
    conn.execute(
        "INSERT INTO session_messages (id, session_id, content, created_at, sent_at) \
         VALUES ('msg-t1', ?1, ?2, '2026-09-14T10:00:11.000Z', '2026-09-14T10:00:11.000Z')",
        rusqlite::params![TREE, content],
    )
    .unwrap();
    let reads = Reads::new(test.db(), test.root());
    let detail = |reads: &Reads| {
        reads
            .get_messages(TREE, 0)
            .unwrap()
            .entries
            .remove(0)
            .detail
            .unwrap()
    };

    // The directory is not a worktree yet (it has no `.git`): the path is left alone.
    assert_eq!(
        detail(&reads),
        format!("cd {worktree_text} && ls {worktree_text}/src")
    );

    std::fs::create_dir_all(worktree.join(".git")).unwrap();
    assert_eq!(detail(&reads), "ls src");
}

/// Appends `count` invented plain-prompt rows to `chat`, after whatever the database holds, and
/// returns their row ids in order. `blank_last` makes the last row render nothing.
fn append_rows(
    conn: &rusqlite::Connection,
    chat: &str,
    count: usize,
    blank_last: bool,
) -> Vec<i64> {
    let tx = conn.unchecked_transaction().unwrap();
    let mut rowids = Vec::with_capacity(count);
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO session_messages (id, session_id, content, created_at, sent_at) \
                 VALUES (?1, ?2, ?3, '2026-09-15T08:00:00.000Z', '2026-09-15T08:00:00.000Z')",
            )
            .unwrap();
        for n in 0..count {
            let content = if blank_last && n + 1 == count {
                String::new()
            } else {
                format!("Invented prompt {n}.")
            };
            insert
                .execute(rusqlite::params![format!("bulk-{chat}-{n}"), chat, content])
                .unwrap();
            rowids.push(tx.last_insert_rowid());
        }
    }
    tx.commit().unwrap();
    rowids
}

#[test]
fn a_chat_longer_than_one_batch_is_read_whole() {
    // Rows of another chat follow the long one; they must never appear in its answer.
    let test = TestDb::new();
    let conn = test.conn();
    seed_messages::seed(&conn);
    let long = "msg-long";
    conn.execute(
        "INSERT INTO sessions (id, status, title, workspace_id, created_at, updated_at) \
         VALUES (?1, 'idle', 'Untitled', 'msg-workspace', '2026-09-14 09:00:00', '2026-09-14 09:00:00')",
        [long],
    )
    .unwrap();
    let count = 1_237; // two full batches of 500 and a remainder
    let rowids = append_rows(&conn, long, count, true);
    append_rows(&conn, OTHER, 3, false);
    let reads = Reads::new(test.db(), test.root());

    // The whole chat: one entry per row except the blank last one, in row order, and the cursor
    // is the last row, which has no entry.
    let whole = reads.get_messages(long, 0).unwrap();
    assert_eq!(whole.entries.len(), count - 1);
    let expected_ids: Vec<String> = (0..count - 1).map(|n| format!("bulk-{long}-{n}")).collect();
    assert_eq!(
        whole
            .entries
            .iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>(),
        expected_ids
    );
    assert_eq!(
        whole.entries.iter().map(|e| e.rowid).collect::<Vec<_>>(),
        rowids[..count - 1]
    );
    assert_eq!(whole.cursor, *rowids.last().unwrap());

    // A cursor in the middle of a batch: the rows after it, none repeated, none skipped.
    let middle = rowids[699];
    let rest = reads.get_messages(long, middle).unwrap();
    assert_eq!(rest.entries.len(), count - 1 - 700);
    assert_eq!(rest.entries[0].id, format!("bulk-{long}-700"));
    assert_eq!(rest.entries, whole.entries[700..]);
    assert_eq!(rest.cursor, *rowids.last().unwrap());

    // A cursor on the last row of a batch, and one batch plus a blank row left.
    let on_edge = reads.get_messages(long, rowids[499]).unwrap();
    assert_eq!(on_edge.entries, whole.entries[500..]);
    assert_eq!(on_edge.cursor, whole.cursor);
}

#[test]
fn a_chat_of_exactly_one_batch_is_read_whole() {
    let test = TestDb::new();
    let conn = test.conn();
    seed_messages::seed(&conn);
    let exact = "msg-exact";
    conn.execute(
        "INSERT INTO sessions (id, status, title, workspace_id, created_at, updated_at) \
         VALUES (?1, 'idle', 'Untitled', 'msg-workspace', '2026-09-14 09:00:00', '2026-09-14 09:00:00')",
        [exact],
    )
    .unwrap();
    let rowids = append_rows(&conn, exact, 500, false);
    let reads = Reads::new(test.db(), test.root());

    let whole = reads.get_messages(exact, 0).unwrap();
    assert_eq!(whole.entries.len(), 500);
    assert_eq!(
        whole.entries.iter().map(|e| e.rowid).collect::<Vec<_>>(),
        rowids
    );
    assert_eq!(whole.cursor, *rowids.last().unwrap());

    // Nothing is left after the full batch: the cursor stays.
    let after_all = reads.get_messages(exact, whole.cursor).unwrap();
    assert!(after_all.entries.is_empty());
    assert_eq!(after_all.cursor, whole.cursor);
}
