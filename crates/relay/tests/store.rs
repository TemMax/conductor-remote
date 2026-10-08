use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use conductor_remote::state::store::{
    ChatLinkRow, DeviceRow, NewDevice, NewFirstPrompt, NewParked, ParkedStatus, Store, StoreError,
};
use rusqlite::Connection;

fn parked(session: &str, text: &str, created_at_ms: i64) -> NewParked {
    NewParked {
        workspace_id: "w1".into(),
        session_id: session.into(),
        text: text.into(),
        queue: false,
        created_at_ms,
        reason: "locked".into(),
        cursor_rowid: None,
        cursor_outbox: Vec::new(),
    }
}

fn device(id: &str, endpoint: &str, label: &str, created_at_ms: i64) -> NewDevice {
    NewDevice {
        id: id.into(),
        endpoint: endpoint.into(),
        p256dh: "key-a".into(),
        auth: "auth-a".into(),
        label: label.into(),
        created_at_ms,
    }
}

fn user_version(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn new_file_is_private_and_versioned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.db");
    let store = Store::open(&path).unwrap();
    drop(store);
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(user_version(&path), 3);
}

#[test]
fn reopening_keeps_data_and_does_not_migrate_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.db");
    {
        let store = Store::open(&path).unwrap();
        store.set_meta("k", "v").unwrap();
        store.park(&parked("s1", "hello", 10)).unwrap();
        store
            .upsert_device(&device("d1", "https://push/1", "phone", 5))
            .unwrap();
    }
    // A second migration would fail on the existing tables.
    let store = Store::open(&path).unwrap();
    assert_eq!(store.meta("k").unwrap().as_deref(), Some("v"));
    assert_eq!(store.parked().unwrap().len(), 1);
    assert_eq!(store.devices().unwrap().len(), 1);
    drop(store);
    assert_eq!(user_version(&path), 3);
}

#[test]
fn meta_gets_and_sets() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.meta("missing").unwrap(), None);
    store.set_meta("a", "1").unwrap();
    assert_eq!(store.meta("a").unwrap().as_deref(), Some("1"));
    store.set_meta("a", "2").unwrap();
    assert_eq!(store.meta("a").unwrap().as_deref(), Some("2"));
    assert_eq!(store.meta("b").unwrap(), None);
}

#[test]
fn park_inserts_a_waiting_row() {
    let store = Store::open_in_memory().unwrap();
    let mut entry = parked("s1", "hello", 100);
    entry.queue = true;
    let row = store.park(&entry).unwrap();
    assert!(row.id > 0);
    assert_eq!(row.workspace_id, "w1");
    assert_eq!(row.session_id, "s1");
    assert_eq!(row.text, "hello");
    assert!(row.queue);
    assert_eq!(row.status, ParkedStatus::Waiting);
    assert_eq!(row.attempts, 0);
    assert_eq!(row.created_at_ms, 100);
    assert_eq!(row.reason, "locked");
    assert_eq!(row.error, None);
    assert_eq!(store.parked().unwrap(), vec![row]);
}

#[test]
fn re_park_of_the_same_chat_and_text_resets_the_row() {
    let store = Store::open_in_memory().unwrap();
    let first = store.park(&parked("s1", "hello", 100)).unwrap();
    store.record_parked_failure(first.id, "boom", 2).unwrap();
    let failed = store
        .record_parked_failure(first.id, "boom", 2)
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, ParkedStatus::Failed);
    assert_eq!(failed.attempts, 2);
    assert_eq!(failed.error.as_deref(), Some("boom"));

    let again = store
        .park(&NewParked {
            workspace_id: "w2".into(),
            queue: true,
            created_at_ms: 999,
            reason: "other".into(),
            cursor_rowid: Some(7),
            cursor_outbox: vec!["o1".into()],
            ..parked("s1", "hello", 0)
        })
        .unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(again.created_at_ms, 100);
    assert_eq!(again.reason, "locked");
    assert_eq!(again.status, ParkedStatus::Waiting);
    assert_eq!(again.attempts, 0);
    assert_eq!(again.error, None);
    assert_eq!(again.workspace_id, "w2");
    assert!(again.queue);
    assert_eq!(again.cursor_rowid, Some(7));
    assert_eq!(again.cursor_outbox, vec!["o1".to_string()]);
    assert_eq!(store.parked().unwrap(), vec![again]);
}

#[test]
fn the_same_text_in_another_chat_is_another_row() {
    let store = Store::open_in_memory().unwrap();
    let a = store.park(&parked("s1", "hello", 1)).unwrap();
    let b = store.park(&parked("s2", "hello", 2)).unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(store.parked().unwrap().len(), 2);
}

#[test]
fn parked_rows_come_oldest_id_first() {
    let store = Store::open_in_memory().unwrap();
    // Created times run backwards: the order is by id, not by time.
    let a = store.park(&parked("s1", "one", 30)).unwrap();
    let b = store.park(&parked("s1", "two", 20)).unwrap();
    let c = store.park(&parked("s2", "three", 10)).unwrap();
    let ids: Vec<i64> = store.parked().unwrap().iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![a.id, b.id, c.id]);
    assert!(a.id < b.id && b.id < c.id);
}

#[test]
fn parked_failure_reaches_failed_at_the_third_call() {
    let store = Store::open_in_memory().unwrap();
    let row = store.park(&parked("s1", "hello", 1)).unwrap();

    let one = store
        .record_parked_failure(row.id, "e1", 3)
        .unwrap()
        .unwrap();
    assert_eq!((one.status, one.attempts), (ParkedStatus::Waiting, 1));
    let two = store
        .record_parked_failure(row.id, "e2", 3)
        .unwrap()
        .unwrap();
    assert_eq!((two.status, two.attempts), (ParkedStatus::Waiting, 2));
    let three = store
        .record_parked_failure(row.id, "e3", 3)
        .unwrap()
        .unwrap();
    assert_eq!((three.status, three.attempts), (ParkedStatus::Failed, 3));
    assert_eq!(three.error.as_deref(), Some("e3"));
    assert_eq!(store.parked().unwrap(), vec![three]);

    assert_eq!(store.record_parked_failure(999, "x", 3).unwrap(), None);
}

#[test]
fn parked_rows_are_removed_forgotten_and_pruned() {
    let store = Store::open_in_memory().unwrap();
    let a = store.park(&parked("s1", "a", 10)).unwrap();
    store.park(&parked("s1", "b", 20)).unwrap();
    store.park(&parked("s2", "a", 30)).unwrap();
    store.park(&parked("s2", "c", 40)).unwrap();

    assert!(store.remove_parked(a.id).unwrap());
    assert!(!store.remove_parked(a.id).unwrap());
    assert_eq!(store.parked().unwrap().len(), 3);

    assert_eq!(store.forget_parked_text("s2", "a").unwrap(), 1);
    assert_eq!(store.forget_parked_text("s2", "a").unwrap(), 0);
    assert_eq!(store.forget_parked_text("s9", "b").unwrap(), 0);
    assert_eq!(store.parked().unwrap().len(), 2);

    // Created at 20 and 40 remain; the cutoff is exclusive.
    assert_eq!(store.prune_parked(20).unwrap(), 0);
    assert_eq!(store.prune_parked(21).unwrap(), 1);
    let left = store.parked().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].text, "c");

    store.park(&parked("s2", "d", 50)).unwrap();
    assert_eq!(store.forget_parked_session("s2").unwrap(), 2);
    assert_eq!(store.forget_parked_session("s2").unwrap(), 0);
    assert!(store.parked().unwrap().is_empty());
}

#[test]
fn the_cursor_round_trips() {
    let store = Store::open_in_memory().unwrap();
    let with = store
        .park(&NewParked {
            cursor_rowid: Some(42),
            cursor_outbox: vec!["a".into(), "quote \" and \u{e9}".into()],
            ..parked("s1", "with", 1)
        })
        .unwrap();
    let without = store.park(&parked("s1", "without", 2)).unwrap();
    assert_eq!(with.cursor_rowid, Some(42));
    assert_eq!(
        with.cursor_outbox,
        vec!["a".to_string(), "quote \" and \u{e9}".to_string()]
    );
    assert_eq!(without.cursor_rowid, None);
    assert!(without.cursor_outbox.is_empty());
    assert_eq!(store.parked().unwrap(), vec![with, without]);
}

#[test]
fn device_upsert_inserts() {
    let store = Store::open_in_memory().unwrap();
    let row = store
        .upsert_device(&device("d1", "https://push/1", "phone", 5))
        .unwrap();
    assert_eq!(
        row,
        DeviceRow {
            id: "d1".into(),
            endpoint: "https://push/1".into(),
            p256dh: "key-a".into(),
            auth: "auth-a".into(),
            label: "phone".into(),
            created_at_ms: 5,
            last_ok_at_ms: None,
            last_error: None,
            failures: 0,
        }
    );
    assert_eq!(store.device("d1").unwrap(), Some(row.clone()));
    assert_eq!(store.device("nope").unwrap(), None);
    assert_eq!(store.devices().unwrap(), vec![row]);
}

#[test]
fn device_upsert_of_the_same_endpoint_keeps_id_and_label() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_device(&device("d1", "https://push/1", "phone", 5))
        .unwrap();
    store.record_device_failure("d1", "gone").unwrap();
    store.record_device_failure("d1", "gone").unwrap();

    let again = store
        .upsert_device(&NewDevice {
            p256dh: "key-b".into(),
            auth: "auth-b".into(),
            ..device("d2", "https://push/1", "", 99)
        })
        .unwrap();
    assert_eq!(again.id, "d1");
    assert_eq!(again.label, "phone");
    assert_eq!(again.created_at_ms, 5);
    assert_eq!(again.p256dh, "key-b");
    assert_eq!(again.auth, "auth-b");
    assert_eq!(again.failures, 0);
    assert_eq!(again.last_error, None);
    assert_eq!(store.devices().unwrap().len(), 1);

    let renamed = store
        .upsert_device(&device("d3", "https://push/1", "tablet", 100))
        .unwrap();
    assert_eq!(renamed.id, "d1");
    assert_eq!(renamed.label, "tablet");
}

#[test]
fn devices_come_oldest_first() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_device(&device("b", "e-b", "", 10)).unwrap();
    store.upsert_device(&device("a", "e-a", "", 10)).unwrap();
    store.upsert_device(&device("c", "e-c", "", 1)).unwrap();
    let ids: Vec<String> = store.devices().unwrap().into_iter().map(|d| d.id).collect();
    assert_eq!(ids, ["c", "a", "b"]);
}

#[test]
fn device_ok_and_failure_bookkeeping() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_device(&device("d1", "https://push/1", "phone", 5))
        .unwrap();

    assert_eq!(store.record_device_failure("d1", "e1").unwrap(), 1);
    assert_eq!(store.record_device_failure("d1", "e2").unwrap(), 2);
    let failing = store.device("d1").unwrap().unwrap();
    assert_eq!(failing.failures, 2);
    assert_eq!(failing.last_error.as_deref(), Some("e2"));
    assert_eq!(failing.last_ok_at_ms, None);

    store.record_device_ok("d1", 777).unwrap();
    let ok = store.device("d1").unwrap().unwrap();
    assert_eq!(ok.failures, 0);
    assert_eq!(ok.last_error, None);
    assert_eq!(ok.last_ok_at_ms, Some(777));

    assert_eq!(store.record_device_failure("gone", "e").unwrap(), 0);
    store.record_device_ok("gone", 1).unwrap();
}

#[test]
fn devices_are_removed_by_id_and_by_endpoint() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_device(&device("d1", "e1", "", 1)).unwrap();
    store.upsert_device(&device("d2", "e2", "", 2)).unwrap();

    assert!(store.remove_device("d1").unwrap());
    assert!(!store.remove_device("d1").unwrap());
    assert!(store.remove_device_by_endpoint("e2").unwrap());
    assert!(!store.remove_device_by_endpoint("e2").unwrap());
    assert!(store.devices().unwrap().is_empty());
}

#[test]
fn a_database_from_a_newer_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA user_version = 4").unwrap();
    }
    let error = match Store::open(&path) {
        Ok(_) => panic!("a newer database must be refused"),
        Err(error) => error,
    };
    assert!(matches!(error, StoreError::Io(_)));
    assert!(error.to_string().contains("newer version"), "{error}");
    assert_eq!(user_version(&path), 4);
}

fn first_prompt(workspace: &str, text: &str, created_at_ms: i64) -> NewFirstPrompt {
    NewFirstPrompt {
        workspace_id: workspace.into(),
        text: text.into(),
        send_immediately: true,
        attachment_ids: vec!["a1".into(), "a2".into()],
        created_at_ms,
    }
}

#[test]
fn a_version_1_file_is_upgraded_to_2_and_keeps_its_parked_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.db");
    {
        // The tables migration 1 creates, built by hand.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE parked_prompts (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               workspace_id TEXT NOT NULL, session_id TEXT NOT NULL, text TEXT NOT NULL,
               queue INTEGER NOT NULL DEFAULT 0,
               status TEXT NOT NULL CHECK (status IN ('waiting', 'failed')),
               attempts INTEGER NOT NULL DEFAULT 0,
               created_at_ms INTEGER NOT NULL, reason TEXT NOT NULL, error TEXT,
               cursor_rowid INTEGER, cursor_outbox TEXT NOT NULL DEFAULT '[]');
             CREATE UNIQUE INDEX parked_by_chat_text ON parked_prompts (session_id, text);
             CREATE TABLE push_devices (
               id TEXT PRIMARY KEY, endpoint TEXT NOT NULL UNIQUE, p256dh TEXT NOT NULL,
               auth TEXT NOT NULL, label TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
               last_ok_at_ms INTEGER, last_error TEXT, failures INTEGER NOT NULL DEFAULT 0);
             INSERT INTO meta (key, value) VALUES ('k', 'v');
             INSERT INTO parked_prompts (workspace_id, session_id, text, status, attempts,
                                         created_at_ms, reason)
               VALUES ('w1', 's1', 'old prompt', 'failed', 3, 42, 'locked');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    assert_eq!(user_version(&path), 1);

    let store = Store::open(&path).unwrap();
    let rows = store.parked().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "old prompt");
    assert_eq!(rows[0].status, ParkedStatus::Failed);
    assert_eq!(rows[0].attempts, 3);
    assert_eq!(store.meta("k").unwrap().as_deref(), Some("v"));
    // The new tables exist and work.
    store
        .upsert_first_prompt(&first_prompt("w1", "hi", 1))
        .unwrap();
    assert_eq!(store.join_chats("b", "w1", "a", "t", "c").unwrap(), Ok(()));
    drop(store);
    assert_eq!(user_version(&path), 3);
}

#[test]
fn upsert_first_prompt_inserts_a_waiting_entry() {
    let store = Store::open_in_memory().unwrap();
    let row = store
        .upsert_first_prompt(&first_prompt("w1", "hello", 100))
        .unwrap();
    assert_eq!(row.workspace_id, "w1");
    assert_eq!(row.text, "hello");
    assert!(row.send_immediately);
    assert_eq!(row.attachment_ids, vec!["a1".to_string(), "a2".to_string()]);
    assert_eq!(row.status, ParkedStatus::Waiting);
    assert_eq!((row.attempts, row.early_attempts), (0, 0));
    assert_eq!(row.created_at_ms, 100);
    assert_eq!(row.last_attempt_at_ms, None);
    assert_eq!(row.error, None);
    assert_eq!(store.first_prompt("w1").unwrap(), Some(row));
    assert_eq!(store.first_prompt("other").unwrap(), None);
}

#[test]
fn upsert_first_prompt_replaces_the_whole_entry() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_first_prompt(&first_prompt("w1", "one", 100))
        .unwrap();
    store.record_first_prompt_attempt("w1", false, 150).unwrap();
    store.record_first_prompt_attempt("w1", true, 160).unwrap();
    store.fail_first_prompt("w1", "boom").unwrap();

    let mut again = first_prompt("w1", "two", 200);
    again.send_immediately = false;
    again.attachment_ids = Vec::new();
    let row = store.upsert_first_prompt(&again).unwrap();
    assert_eq!(row.text, "two");
    assert!(!row.send_immediately);
    assert!(row.attachment_ids.is_empty());
    assert_eq!(row.status, ParkedStatus::Waiting);
    assert_eq!((row.attempts, row.early_attempts), (0, 0));
    assert_eq!(row.created_at_ms, 200);
    assert_eq!(row.last_attempt_at_ms, None);
    assert_eq!(row.error, None);
    assert_eq!(store.first_prompts().unwrap(), vec![row]);
}

#[test]
fn first_prompts_are_listed_oldest_first() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_first_prompt(&first_prompt("w-late", "c", 30))
        .unwrap();
    store
        .upsert_first_prompt(&first_prompt("w-early", "a", 10))
        .unwrap();
    store
        .upsert_first_prompt(&first_prompt("w-mid", "b", 20))
        .unwrap();
    let ids: Vec<_> = store
        .first_prompts()
        .unwrap()
        .into_iter()
        .map(|row| row.workspace_id)
        .collect();
    assert_eq!(ids, ["w-early", "w-mid", "w-late"]);
}

#[test]
fn first_prompt_attempts_count_early_and_late_separately() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_first_prompt(&first_prompt("w1", "x", 1))
        .unwrap();
    let row = store
        .record_first_prompt_attempt("w1", true, 500)
        .unwrap()
        .unwrap();
    assert_eq!((row.attempts, row.early_attempts), (0, 1));
    assert_eq!(row.last_attempt_at_ms, Some(500));
    store.record_first_prompt_attempt("w1", true, 510).unwrap();
    let row = store
        .record_first_prompt_attempt("w1", false, 520)
        .unwrap()
        .unwrap();
    assert_eq!((row.attempts, row.early_attempts), (1, 2));
    assert_eq!(row.last_attempt_at_ms, Some(520));
    assert_eq!(row.status, ParkedStatus::Waiting);
    assert_eq!(store.first_prompt("w1").unwrap(), Some(row));
    assert_eq!(
        store.record_first_prompt_attempt("gone", false, 1).unwrap(),
        None
    );
}

#[test]
fn a_first_prompt_can_fail_and_lose_its_attachments() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_first_prompt(&first_prompt("w1", "x", 1))
        .unwrap();
    let row = store.clear_first_prompt_attachments("w1").unwrap().unwrap();
    assert!(row.attachment_ids.is_empty());
    assert_eq!(row.status, ParkedStatus::Waiting);

    let row = store.fail_first_prompt("w1", "gave up").unwrap().unwrap();
    assert_eq!(row.status, ParkedStatus::Failed);
    assert_eq!(row.error.as_deref(), Some("gave up"));
    assert_eq!(store.first_prompt("w1").unwrap(), Some(row));

    assert_eq!(store.fail_first_prompt("gone", "e").unwrap(), None);
    assert_eq!(store.clear_first_prompt_attachments("gone").unwrap(), None);
}

#[test]
fn first_prompts_are_removed_and_pruned() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_first_prompt(&first_prompt("w1", "a", 10))
        .unwrap();
    store
        .upsert_first_prompt(&first_prompt("w2", "b", 20))
        .unwrap();
    store
        .upsert_first_prompt(&first_prompt("w3", "c", 30))
        .unwrap();

    assert_eq!(store.prune_first_prompts(20).unwrap(), 1);
    assert_eq!(store.first_prompt("w1").unwrap(), None);
    assert!(store.first_prompt("w2").unwrap().is_some());

    assert!(store.remove_first_prompt("w2").unwrap());
    assert!(!store.remove_first_prompt("w2").unwrap());
    assert_eq!(store.first_prompts().unwrap().len(), 1);
}

fn link(session: &str, workspace: &str, previous: &str, title: &str, created: &str) -> ChatLinkRow {
    ChatLinkRow {
        session_id: session.into(),
        workspace_id: workspace.into(),
        previous_session_id: previous.into(),
        title: title.into(),
        created_at: created.into(),
    }
}

#[test]
fn join_chats_links_a_chat_to_its_predecessor() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.chat_links("w1").unwrap().is_empty());
    let joined = store
        .join_chats("b", "w1", "a", "First chat", "2026-01-01 10:00:00")
        .unwrap();
    assert_eq!(joined, Ok(()));
    assert_eq!(
        store.chat_links("w1").unwrap(),
        vec![link("b", "w1", "a", "First chat", "2026-01-01 10:00:00")]
    );
    assert!(store.chat_links("w2").unwrap().is_empty());
}

#[test]
fn join_chats_is_idempotent_for_the_same_pair() {
    let store = Store::open_in_memory().unwrap();
    store.join_chats("b", "w1", "a", "T", "C").unwrap().unwrap();
    // A retry carries whatever the previous chat's metadata says now; nothing changes.
    let again = store.join_chats("b", "w1", "a", "Renamed", "D").unwrap();
    assert_eq!(again, Ok(()));
    assert_eq!(
        store.chat_links("w1").unwrap(),
        vec![link("b", "w1", "a", "T", "C")]
    );
}

#[test]
fn join_chats_refuses_a_chat_that_already_belongs_to_a_conversation() {
    let store = Store::open_in_memory().unwrap();
    store.join_chats("b", "w1", "a", "T", "C").unwrap().unwrap();
    // Same previous, other workspace: not the same pair.
    assert_eq!(
        store.join_chats("b", "w2", "a", "T", "C").unwrap(),
        Err("This chat already belongs to a conversation".to_string())
    );
    // Other previous.
    assert_eq!(
        store.join_chats("b", "w1", "z", "T", "C").unwrap(),
        Err("This chat already belongs to a conversation".to_string())
    );
    assert_eq!(store.chat_links("w1").unwrap().len(), 1);
}

#[test]
fn join_chats_refuses_a_previous_chat_that_already_continues_elsewhere() {
    let store = Store::open_in_memory().unwrap();
    store.join_chats("b", "w1", "a", "T", "C").unwrap().unwrap();
    assert_eq!(
        store.join_chats("c", "w1", "a", "T", "C").unwrap(),
        Err(
            "This chat already continues in another tab. Refresh to open the latest conversation."
                .to_string()
        )
    );
    assert_eq!(store.chat_links("w1").unwrap().len(), 1);
}

#[test]
fn join_chats_refuses_a_cycle() {
    let store = Store::open_in_memory().unwrap();
    // a <- b <- c
    store.join_chats("b", "w1", "a", "T", "C").unwrap().unwrap();
    store.join_chats("c", "w1", "b", "T", "C").unwrap().unwrap();
    let cycle = "Chat history cannot contain a cycle".to_string();
    // a would continue c, which descends from a.
    assert_eq!(
        store.join_chats("a", "w1", "c", "T", "C").unwrap(),
        Err(cycle.clone())
    );
    // A chat cannot continue itself.
    assert_eq!(
        store.join_chats("d", "w1", "d", "T", "C").unwrap(),
        Err(cycle)
    );
    assert_eq!(store.chat_links("w1").unwrap().len(), 2);
}

#[test]
fn join_chats_refuses_a_previous_link_of_another_workspace() {
    let store = Store::open_in_memory().unwrap();
    store.join_chats("b", "w1", "a", "T", "C").unwrap().unwrap();
    assert_eq!(
        store.join_chats("c", "w2", "b", "T", "C").unwrap(),
        Err("Chats must share a workspace".to_string())
    );
    assert!(store.chat_links("w2").unwrap().is_empty());
}

#[test]
fn join_chats_inherits_the_title_and_time_of_the_previous_link() {
    let store = Store::open_in_memory().unwrap();
    store
        .join_chats("b", "w1", "a", "Original title", "2026-01-01 10:00:00")
        .unwrap()
        .unwrap();
    // The caller passes b's own title and time; the chain keeps a's.
    store
        .join_chats("c", "w1", "b", "Second title", "2026-02-02 11:00:00")
        .unwrap()
        .unwrap();
    store
        .join_chats("d", "w1", "c", "Third title", "2026-03-03 12:00:00")
        .unwrap()
        .unwrap();
    let links = store.chat_links("w1").unwrap();
    assert_eq!(
        links,
        vec![
            link("b", "w1", "a", "Original title", "2026-01-01 10:00:00"),
            link("c", "w1", "b", "Original title", "2026-01-01 10:00:00"),
            link("d", "w1", "c", "Original title", "2026-01-01 10:00:00"),
        ]
    );
}

#[test]
fn a_parked_agent_is_set_read_replaced_and_cleared() {
    let store = Store::open_in_memory().unwrap();
    let a = store.park(&parked("s1", "a", 10)).unwrap();
    let b = store.park(&parked("s1", "b", 20)).unwrap();
    assert_eq!(store.parked_agent(a.id).unwrap(), None);

    store
        .set_parked_agent(a.id, Some(r#"{"model":"x"}"#))
        .unwrap();
    assert_eq!(
        store.parked_agent(a.id).unwrap().as_deref(),
        Some(r#"{"model":"x"}"#)
    );
    assert_eq!(store.parked_agent(b.id).unwrap(), None);

    store
        .set_parked_agent(a.id, Some(r#"{"model":"y"}"#))
        .unwrap();
    assert_eq!(
        store.parked_agent(a.id).unwrap().as_deref(),
        Some(r#"{"model":"y"}"#)
    );

    store.set_parked_agent(a.id, None).unwrap();
    assert_eq!(store.parked_agent(a.id).unwrap(), None);
    // Clearing what is not there is fine.
    store.set_parked_agent(a.id, None).unwrap();
}

#[test]
fn the_agent_of_a_parked_prompt_goes_with_the_row() {
    let store = Store::open_in_memory().unwrap();
    let removed = store.park(&parked("s1", "removed", 10)).unwrap();
    let forgotten_session = store.park(&parked("s2", "x", 20)).unwrap();
    let forgotten_text = store.park(&parked("s3", "y", 30)).unwrap();
    let pruned = store.park(&parked("s4", "z", 5)).unwrap();
    let kept = store.park(&parked("s5", "kept", 40)).unwrap();
    for row in [
        &removed,
        &forgotten_session,
        &forgotten_text,
        &pruned,
        &kept,
    ] {
        store.set_parked_agent(row.id, Some("{}")).unwrap();
    }

    store.remove_parked(removed.id).unwrap();
    assert_eq!(store.parked_agent(removed.id).unwrap(), None);

    store.forget_parked_session("s2").unwrap();
    assert_eq!(store.parked_agent(forgotten_session.id).unwrap(), None);

    store.forget_parked_text("s3", "y").unwrap();
    assert_eq!(store.parked_agent(forgotten_text.id).unwrap(), None);

    assert_eq!(store.prune_parked(10).unwrap(), 1);
    assert_eq!(store.parked_agent(pruned.id).unwrap(), None);

    assert_eq!(store.parked_agent(kept.id).unwrap().as_deref(), Some("{}"));
}

#[test]
fn a_re_park_of_the_same_chat_and_text_keeps_the_agent() {
    let store = Store::open_in_memory().unwrap();
    let first = store.park(&parked("s1", "hello", 10)).unwrap();
    store.set_parked_agent(first.id, Some("{}")).unwrap();
    let again = store.park(&parked("s1", "hello", 20)).unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(store.parked_agent(first.id).unwrap().as_deref(), Some("{}"));
}

#[test]
fn a_first_prompt_agent_is_set_read_replaced_and_cleared() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.first_prompt_agent("w1").unwrap(), None);

    store.set_first_prompt_agent("w1", Some("a")).unwrap();
    store.set_first_prompt_agent("w2", Some("other")).unwrap();
    assert_eq!(
        store.first_prompt_agent("w1").unwrap().as_deref(),
        Some("a")
    );

    store.set_first_prompt_agent("w1", Some("b")).unwrap();
    assert_eq!(
        store.first_prompt_agent("w1").unwrap().as_deref(),
        Some("b")
    );
    assert_eq!(
        store.first_prompt_agent("w2").unwrap().as_deref(),
        Some("other")
    );

    store.set_first_prompt_agent("w1", None).unwrap();
    assert_eq!(store.first_prompt_agent("w1").unwrap(), None);
    store.set_first_prompt_agent("w1", None).unwrap();
}

#[test]
fn the_agent_of_a_first_prompt_goes_with_the_entry() {
    let store = Store::open_in_memory().unwrap();
    for (workspace, created) in [("w1", 10), ("w2", 20), ("w3", 30)] {
        store
            .upsert_first_prompt(&first_prompt(workspace, "t", created))
            .unwrap();
        store.set_first_prompt_agent(workspace, Some("{}")).unwrap();
    }

    assert!(store.remove_first_prompt("w1").unwrap());
    assert_eq!(store.first_prompt_agent("w1").unwrap(), None);

    assert_eq!(store.prune_first_prompts(21).unwrap(), 1);
    assert_eq!(store.first_prompt_agent("w2").unwrap(), None);

    assert_eq!(
        store.first_prompt_agent("w3").unwrap().as_deref(),
        Some("{}")
    );
}

#[test]
fn upserting_a_first_prompt_keeps_its_agent() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_first_prompt(&first_prompt("w1", "one", 10))
        .unwrap();
    store.set_first_prompt_agent("w1", Some("{}")).unwrap();
    store
        .upsert_first_prompt(&first_prompt("w1", "two", 20))
        .unwrap();
    assert_eq!(
        store.first_prompt_agent("w1").unwrap().as_deref(),
        Some("{}")
    );
}

#[test]
fn a_version_2_file_is_upgraded_to_3_and_keeps_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.db");
    {
        // The tables migrations 1 and 2 create, built by hand.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE parked_prompts (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               workspace_id TEXT NOT NULL, session_id TEXT NOT NULL, text TEXT NOT NULL,
               queue INTEGER NOT NULL DEFAULT 0,
               status TEXT NOT NULL CHECK (status IN ('waiting', 'failed')),
               attempts INTEGER NOT NULL DEFAULT 0,
               created_at_ms INTEGER NOT NULL, reason TEXT NOT NULL, error TEXT,
               cursor_rowid INTEGER, cursor_outbox TEXT NOT NULL DEFAULT '[]');
             CREATE UNIQUE INDEX parked_by_chat_text ON parked_prompts (session_id, text);
             CREATE TABLE push_devices (
               id TEXT PRIMARY KEY, endpoint TEXT NOT NULL UNIQUE, p256dh TEXT NOT NULL,
               auth TEXT NOT NULL, label TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
               last_ok_at_ms INTEGER, last_error TEXT, failures INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE first_prompts (
               workspace_id TEXT PRIMARY KEY, text TEXT NOT NULL,
               send_immediately INTEGER NOT NULL DEFAULT 1,
               attachment_ids TEXT NOT NULL DEFAULT '[]',
               status TEXT NOT NULL CHECK (status IN ('waiting', 'failed')),
               attempts INTEGER NOT NULL DEFAULT 0, early_attempts INTEGER NOT NULL DEFAULT 0,
               created_at_ms INTEGER NOT NULL, last_attempt_at_ms INTEGER, error TEXT);
             CREATE TABLE chat_links (
               session_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL,
               previous_session_id TEXT NOT NULL UNIQUE, title TEXT NOT NULL DEFAULT '',
               created_at TEXT NOT NULL);
             INSERT INTO parked_prompts (workspace_id, session_id, text, status, attempts,
                                         created_at_ms, reason)
               VALUES ('w1', 's1', 'old prompt', 'waiting', 1, 42, 'locked');
             INSERT INTO first_prompts (workspace_id, text, status, created_at_ms)
               VALUES ('w2', 'old first', 'waiting', 43);
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }
    assert_eq!(user_version(&path), 2);

    let store = Store::open(&path).unwrap();
    let rows = store.parked().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "old prompt");
    let first = store.first_prompt("w2").unwrap().unwrap();
    assert_eq!(first.text, "old first");
    // The new tables exist and work.
    assert_eq!(store.parked_agent(rows[0].id).unwrap(), None);
    store.set_parked_agent(rows[0].id, Some("{}")).unwrap();
    store.set_first_prompt_agent("w2", Some("{}")).unwrap();
    drop(store);
    assert_eq!(user_version(&path), 3);
}
