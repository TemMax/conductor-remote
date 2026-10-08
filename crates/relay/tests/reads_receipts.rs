//! The delivery reads: the cursor, the receipt of a send, the workspace of a write and the open
//! chats. Every row is inserted by the test into a synthetic database.

mod support;

use conductor_remote::reads::receipts::{DeliveryCursor, Receipt, VisibleSession, WriteWorkspace};
use conductor_remote::reads::Reads;
use rusqlite::{params, Connection};
use serde_json::json;
use support::TestDb;

const CHAT: &str = "rc-chat";
const OTHER: &str = "rc-other";

fn setup() -> (TestDb, Reads) {
    let test = TestDb::new();
    let reads = Reads::new(test.db(), test.root());
    (test, reads)
}

fn user_row(conn: &Connection, id: &str, session: &str, content: &str, turn: Option<&str>) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
         VALUES (?1, ?2, 'user', ?3, ?4)",
        params![id, session, content, turn],
    )
    .unwrap();
}

fn assistant_row(conn: &Connection, id: &str, session: &str) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content) \
         VALUES (?1, ?2, 'assistant', '{}')",
        params![id, session],
    )
    .unwrap();
}

fn outbox_row(conn: &Connection, id: &str, session: &str, text: &str, order: Option<i64>) {
    conn.execute(
        "INSERT INTO session_messages_outbox \
         (message_id, session_id, delivery_payload, mode, queue_order, state, created_at) \
         VALUES (?1, ?2, ?3, 'queue', ?4, 'pending', '2026-09-14T10:05:00.000Z')",
        params![id, session, json!({ "message": text }).to_string(), order],
    )
    .unwrap();
}

fn message(id: &str, rowid: i64, turn: Option<&str>) -> Receipt {
    Receipt::Message {
        id: id.to_owned(),
        rowid,
        turn_id: turn.map(str::to_owned),
    }
}

fn outbox(id: &str) -> Receipt {
    Receipt::Outbox { id: id.to_owned() }
}

#[test]
fn cursor_of_an_unknown_chat_is_empty() {
    let (_test, reads) = setup();
    assert_eq!(
        reads.delivery_cursor("nope").unwrap(),
        DeliveryCursor::default()
    );
}

#[test]
fn cursor_without_outbox_rows_holds_the_newest_rowid() {
    let (test, reads) = setup();
    let conn = test.conn();
    user_row(&conn, "u1", CHAT, "one", None);
    user_row(&conn, "o1", OTHER, "other", None);
    assistant_row(&conn, "a1", CHAT);
    let cursor = reads.delivery_cursor(CHAT).unwrap();
    assert_eq!(cursor.rowid, 3);
    assert!(cursor.outbox_ids.is_empty());
    assert_eq!(reads.delivery_cursor(OTHER).unwrap().rowid, 2);
}

#[test]
fn cursor_lists_the_outbox_items_of_this_chat_only() {
    let (test, reads) = setup();
    let conn = test.conn();
    user_row(&conn, "u1", CHAT, "one", None);
    outbox_row(&conn, "q1", CHAT, "queued", Some(1));
    outbox_row(&conn, "q2", CHAT, "queued too", None);
    outbox_row(&conn, "qo", OTHER, "elsewhere", Some(1));
    let cursor = reads.delivery_cursor(CHAT).unwrap();
    assert_eq!(cursor.rowid, 1);
    assert_eq!(
        cursor
            .outbox_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["q1", "q2"]
    );
}

#[test]
fn receipt_a_new_outbox_item_counts_as_delivered() {
    let (test, reads) = setup();
    let conn = test.conn();
    user_row(&conn, "u1", CHAT, "earlier", None);
    let cursor = reads.delivery_cursor(CHAT).unwrap();
    outbox_row(&conn, "q-new", CHAT, "ship it", Some(1));
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        Some(outbox("q-new"))
    );
}

#[test]
fn receipt_b_an_older_identical_outbox_item_does_not_count() {
    let (test, reads) = setup();
    let conn = test.conn();
    outbox_row(&conn, "q-old", CHAT, "ship it", Some(1));
    let cursor = reads.delivery_cursor(CHAT).unwrap();
    assert!(cursor.outbox_ids.contains("q-old"));
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        None
    );
    // A second identical item queued after the cursor is the one that counts.
    outbox_row(&conn, "q-new", CHAT, "ship it", Some(2));
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        Some(outbox("q-new"))
    );
}

#[test]
fn receipt_c_the_same_id_promoted_to_a_durable_row_is_a_message_receipt() {
    let (test, reads) = setup();
    let conn = test.conn();
    outbox_row(&conn, "q1", CHAT, "ship it", Some(1));
    let cursor = DeliveryCursor::default();
    // Promotion keeps the id: the item leaves the outbox and a row with that id appears.
    conn.execute(
        "DELETE FROM session_messages_outbox WHERE message_id = 'q1'",
        [],
    )
    .unwrap();
    user_row(&conn, "q1", CHAT, "ship it", Some("turn-1"));
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        Some(message("q1", 1, Some("turn-1")))
    );
}

#[test]
fn receipt_c_a_row_promoted_from_the_cursor_outbox_item_does_not_count() {
    let (test, reads) = setup();
    let conn = test.conn();
    outbox_row(&conn, "q-old", CHAT, "ship it", Some(1));
    let cursor = reads.delivery_cursor(CHAT).unwrap();
    conn.execute("DELETE FROM session_messages_outbox", [])
        .unwrap();
    user_row(&conn, "q-old", CHAT, "ship it", None);
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        None
    );
}

#[test]
fn receipt_d_the_durable_row_wins_over_a_stale_outbox_row_of_the_same_id() {
    let (test, reads) = setup();
    let conn = test.conn();
    let cursor = DeliveryCursor::default();
    outbox_row(&conn, "q1", CHAT, "ship it", Some(1));
    user_row(&conn, "q1", CHAT, "ship it", Some("turn-9"));
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        Some(message("q1", 1, Some("turn-9")))
    );
}

#[test]
fn receipt_e_a_durable_row_with_no_outbox_stage_returns_at_once() {
    let (test, reads) = setup();
    let conn = test.conn();
    user_row(&conn, "u1", CHAT, "before", None);
    let cursor = reads.delivery_cursor(CHAT).unwrap();
    user_row(&conn, "u2", CHAT, "ship it", None);
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        Some(message("u2", 2, None))
    );
}

#[test]
fn receipt_f_a_matching_row_at_or_below_the_cursor_does_not_count() {
    let (test, reads) = setup();
    let conn = test.conn();
    user_row(&conn, "u1", CHAT, "ship it", None);
    user_row(&conn, "u2", CHAT, "something else", None);
    let cursor = reads.delivery_cursor(CHAT).unwrap();
    assert_eq!(cursor.rowid, 2);
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        None
    );
    // A cursor sitting exactly on the matching row excludes it too.
    let on_row = DeliveryCursor {
        rowid: 1,
        ..DeliveryCursor::default()
    };
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &on_row)
            .unwrap(),
        None
    );
}

#[test]
fn receipt_matching_trims_both_sides_and_uses_raw_content() {
    let (test, reads) = setup();
    let conn = test.conn();
    let cursor = DeliveryCursor::default();
    user_row(&conn, "u1", CHAT, "  padded text \n", None);
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "\tpadded text  ", &cursor)
            .unwrap(),
        Some(message("u1", 1, None))
    );
    // Parsed text does not match: a JSON frame holding the text is not the text.
    user_row(
        &conn,
        "u2",
        CHAT,
        &json!({ "message": "framed" }).to_string(),
        None,
    );
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "framed", &cursor)
            .unwrap(),
        None
    );
    outbox_row(&conn, "q1", CHAT, "   queued text ", Some(1));
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, " queued text", &cursor)
            .unwrap(),
        Some(outbox("q1"))
    );
}

#[test]
fn receipt_ignores_assistant_rows_and_other_chats() {
    let (test, reads) = setup();
    let conn = test.conn();
    let cursor = DeliveryCursor::default();
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content) \
         VALUES ('a1', ?1, 'assistant', 'ship it')",
        [CHAT],
    )
    .unwrap();
    user_row(&conn, "o1", OTHER, "ship it", None);
    outbox_row(&conn, "qo", OTHER, "ship it", Some(1));
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        None
    );
}

#[test]
fn receipt_outbox_payloads_that_cannot_be_read_never_match() {
    let (test, reads) = setup();
    let conn = test.conn();
    let cursor = DeliveryCursor::default();
    for (id, payload) in [("b1", "{"), ("b2", "{}"), ("b3", "{\"message\": 5}")] {
        conn.execute(
            "INSERT INTO session_messages_outbox (message_id, session_id, delivery_payload, \
             mode, state, created_at) VALUES (?1, ?2, ?3, 'queue', 'pending', '2026-09-14')",
            params![id, CHAT, payload],
        )
        .unwrap();
    }
    assert_eq!(
        reads.delivery_receipt_since(CHAT, "", &cursor).unwrap(),
        None
    );
    assert_eq!(
        reads.delivery_receipt_since(CHAT, "x", &cursor).unwrap(),
        None
    );
}

#[test]
fn without_the_outbox_table_durable_rows_are_still_found() {
    let (test, reads) = setup();
    let conn = test.conn();
    conn.execute("DROP TABLE session_messages_outbox", [])
        .unwrap();
    user_row(&conn, "u1", CHAT, "before", None);
    let cursor = reads.delivery_cursor(CHAT).unwrap();
    assert_eq!(cursor.rowid, 1);
    assert!(cursor.outbox_ids.is_empty());
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        None
    );
    user_row(&conn, "u2", CHAT, "ship it", Some("t"));
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &cursor)
            .unwrap(),
        Some(message("u2", 2, Some("t")))
    );
}

#[test]
fn without_the_turn_id_column_the_receipt_has_no_turn() {
    let (test, reads) = setup();
    let conn = test.conn();
    conn.execute_batch(
        "DROP INDEX idx_session_messages_turn_id;
         DROP INDEX idx_session_messages_user_turns;
         ALTER TABLE session_messages DROP COLUMN turn_id;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content) \
         VALUES ('u1', ?1, 'user', 'ship it')",
        [CHAT],
    )
    .unwrap();
    assert_eq!(
        reads
            .delivery_receipt_since(CHAT, "ship it", &DeliveryCursor::default())
            .unwrap(),
        Some(message("u1", 1, None))
    );
}

#[test]
fn receipt_json_of_both_kinds() {
    assert_eq!(
        serde_json::to_value(outbox("q1")).unwrap(),
        json!({ "kind": "outbox", "id": "q1" })
    );
    assert_eq!(
        serde_json::to_value(message("m1", 7, Some("t1"))).unwrap(),
        json!({ "kind": "message", "id": "m1", "rowid": 7, "turnId": "t1" })
    );
    assert_eq!(
        serde_json::to_value(message("m1", 7, None)).unwrap(),
        json!({ "kind": "message", "id": "m1", "rowid": 7, "turnId": null })
    );
}

fn workspace(conn: &Connection, id: &str, state: &str, repo: Option<&str>) {
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, state, \
         workspace_name) VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            repo,
            format!("{id}-dir"),
            format!("{id}-branch"),
            state,
            format!("{id} name")
        ],
    )
    .unwrap();
}

fn session(conn: &Connection, id: &str, workspace: &str, hidden: Option<i64>, created: &str) {
    conn.execute(
        "INSERT INTO sessions (id, status, title, workspace_id, is_hidden, created_at, updated_at) \
         VALUES (?1, 'idle', ?2, ?3, ?4, ?5, ?5)",
        params![id, format!("{id} title"), workspace, hidden, created],
    )
    .unwrap();
}

fn expected(id: &str, repo: Option<&str>) -> WriteWorkspace {
    WriteWorkspace {
        id: id.to_owned(),
        branch: Some(format!("{id}-branch")),
        repo_name: repo.map(str::to_owned),
        workspace_name: Some(format!("{id} name")),
        directory_name: Some(format!("{id}-dir")),
    }
}

#[test]
fn write_workspace_by_id_and_by_session() {
    let (test, reads) = setup();
    let conn = test.conn();
    conn.execute(
        "INSERT INTO repos (id, name) VALUES ('rc-repo', 'rc-project')",
        [],
    )
    .unwrap();
    workspace(&conn, "w-ready", "ready", Some("rc-repo"));
    workspace(&conn, "w-setup", "setting_up", None);
    session(&conn, "s-ready", "w-ready", None, "2026-09-14 09:00:00");
    session(&conn, "s-setup", "w-setup", None, "2026-09-14 09:00:00");

    assert_eq!(
        reads.write_workspace(Some("w-ready"), None).unwrap(),
        Some(expected("w-ready", Some("rc-project")))
    );
    assert_eq!(
        reads.write_workspace(Some("w-setup"), None).unwrap(),
        Some(expected("w-setup", None))
    );
    assert_eq!(
        reads.write_workspace(None, Some("s-ready")).unwrap(),
        Some(expected("w-ready", Some("rc-project")))
    );
    assert_eq!(
        reads.write_workspace(None, Some("s-setup")).unwrap(),
        Some(expected("w-setup", None))
    );
    // The workspace id wins over the session's.
    assert_eq!(
        reads
            .write_workspace(Some("w-setup"), Some("s-ready"))
            .unwrap(),
        Some(expected("w-setup", None))
    );
    assert_eq!(reads.write_workspace(None, None).unwrap(), None);
}

#[test]
fn write_workspace_is_none_for_an_archived_or_unknown_workspace() {
    let (test, reads) = setup();
    let conn = test.conn();
    workspace(&conn, "w-archived", "archived", None);
    session(
        &conn,
        "s-archived",
        "w-archived",
        None,
        "2026-09-14 09:00:00",
    );
    assert_eq!(
        reads.write_workspace(Some("w-archived"), None).unwrap(),
        None
    );
    assert_eq!(
        reads.write_workspace(None, Some("s-archived")).unwrap(),
        None
    );
    assert_eq!(
        reads.write_workspace(Some("w-unknown"), None).unwrap(),
        None
    );
    assert_eq!(
        reads.write_workspace(None, Some("s-unknown")).unwrap(),
        None
    );
    // An unknown id does not fall back to the session.
    assert_eq!(
        reads
            .write_workspace(Some("w-unknown"), Some("s-archived"))
            .unwrap(),
        None
    );
}

#[test]
fn visible_sessions_skip_hidden_chats_and_keep_creation_order() {
    let (test, reads) = setup();
    let conn = test.conn();
    workspace(&conn, "w1", "ready", None);
    workspace(&conn, "w2", "ready", None);
    session(&conn, "s-third", "w1", None, "2026-09-14 09:03:00");
    session(&conn, "s-first", "w1", Some(0), "2026-09-14 09:01:00");
    session(&conn, "s-hidden", "w1", Some(1), "2026-09-14 09:02:00");
    session(&conn, "s-second", "w1", Some(0), "2026-09-14 09:02:30");
    session(&conn, "s-elsewhere", "w2", None, "2026-09-14 09:00:00");
    let open = reads.visible_sessions("w1").unwrap();
    assert_eq!(
        open,
        ["s-first", "s-second", "s-third"]
            .map(|id| VisibleSession {
                id: id.to_owned(),
                title: Some(format!("{id} title")),
                status: Some("idle".to_owned()),
            })
            .to_vec()
    );
    assert!(reads.visible_sessions("nope").unwrap().is_empty());
}
