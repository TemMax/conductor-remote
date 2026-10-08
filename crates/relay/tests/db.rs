mod support;

use std::sync::atomic::{AtomicUsize, Ordering};

use conductor_remote::db::{ConductorDb, DbError};
use support::TestDb;

fn assert_send_sync<T: Send + Sync>() {}

fn count(db: &ConductorDb, table: &'static str) -> i64 {
    db.read("count", |conn| {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
    })
    .unwrap()
}

fn insert_repo(conn: &rusqlite::Connection, id: &str) {
    conn.execute("INSERT INTO repos (id, name) VALUES (?1, ?1)", [id])
        .unwrap();
}

fn repo_ids(db: &ConductorDb) -> Vec<String> {
    db.read("repo ids", |conn| {
        let mut stmt = conn.prepare("SELECT id FROM repos ORDER BY id")?;
        let ids = stmt.query_map([], |r| r.get(0))?.collect();
        ids
    })
    .unwrap()
}

#[test]
fn handle_is_send_and_sync() {
    assert_send_sync::<ConductorDb>();
}

#[test]
fn missing_file_is_an_open_error_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.db");
    let db = ConductorDb::new(&path);
    assert_eq!(db.path(), path);

    let err = db.read("missing", |_| Ok(())).unwrap_err();
    match &err {
        DbError::Open { path: p, .. } => assert_eq!(p, &path),
        other => panic!("expected an Open error, got {other:?}"),
    }
    assert!(err.to_string().contains(path.to_str().unwrap()));
    assert!(db.data_version().is_err());
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn read_returns_rows_inserted_through_the_writer() {
    let test = TestDb::new();
    insert_repo(&test.conn(), "r1");
    insert_repo(&test.conn(), "r2");
    let db = test.db();
    assert_eq!(repo_ids(&db), ["r1", "r2"]);

    // A later commit is visible to the same handle.
    insert_repo(&test.conn(), "r3");
    assert_eq!(repo_ids(&db), ["r1", "r2", "r3"]);
}

#[test]
fn a_write_through_read_fails() {
    let test = TestDb::new();
    let db = test.db();
    let err = db
        .read("write", |conn| {
            conn.execute("INSERT INTO repos (id, name) VALUES ('x', 'x')", [])
        })
        .unwrap_err();
    assert!(matches!(err, DbError::Query(_)), "got {err:?}");
    assert_eq!(count(&db, "repos"), 0);
    assert_eq!(
        test.conn()
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn a_replaced_file_is_read_through_a_new_connection() {
    let test = TestDb::new();
    insert_repo(&test.conn(), "old");
    let db = test.db();
    assert_eq!(repo_ids(&db), ["old"]);
    let before = db.data_version().unwrap();

    // Leave no WAL content behind to be applied to the replacement.
    test.conn()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();

    let other_dir = tempfile::tempdir_in(test.dir()).unwrap();
    let other_path = other_dir.path().join("conductor.db");
    {
        let other = rusqlite::Connection::open(&other_path).unwrap();
        other.pragma_update(None, "journal_mode", "wal").unwrap();
        other.execute_batch(support::SCHEMA).unwrap();
        insert_repo(&other, "new-1");
        insert_repo(&other, "new-2");
    }
    std::fs::rename(&other_path, test.path()).unwrap();

    assert_eq!(repo_ids(&db), ["new-1", "new-2"]);
    let after = db.data_version().unwrap();
    assert!(after.generation > before.generation);
}

#[test]
fn close_then_read_opens_a_new_connection() {
    let test = TestDb::new();
    insert_repo(&test.conn(), "r1");
    let db = test.db();
    assert_eq!(repo_ids(&db), ["r1"]);
    let before = db.data_version().unwrap();
    // Without a close, the connection is kept.
    assert_eq!(db.data_version().unwrap().generation, before.generation);

    db.close();
    assert_eq!(repo_ids(&db), ["r1"]);
    assert!(db.data_version().unwrap().generation > before.generation);
}

#[test]
fn version_changes_only_when_another_connection_commits() {
    let test = TestDb::new();
    let db = test.db();
    let first = db.data_version().unwrap();
    assert_eq!(db.data_version().unwrap(), first);
    assert_eq!(count(&db, "repos"), 0);
    assert_eq!(db.data_version().unwrap(), first);

    insert_repo(&test.conn(), "r1");
    let second = db.data_version().unwrap();
    assert_eq!(second.generation, first.generation);
    assert_ne!(second.version, first.version);
    assert_eq!(db.data_version().unwrap(), second);
}

#[test]
fn a_failing_closure_returns_its_error_and_runs_once() {
    let test = TestDb::new();
    let db = test.db();
    let calls = AtomicUsize::new(0);
    let err = db
        .read("failing", |conn| {
            calls.fetch_add(1, Ordering::SeqCst);
            conn.query_row("SELECT * FROM no_such_table", [], |_| Ok(()))
        })
        .unwrap_err();
    assert!(matches!(err, DbError::Query(_)), "got {err:?}");
    assert!(err.to_string().contains("no_such_table"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The handle is still usable afterwards.
    assert_eq!(count(&db, "repos"), 0);
}

#[test]
fn the_schema_file_creates_all_five_tables() {
    let test = TestDb::new();
    let mut names: Vec<String> = test
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names.retain(|n| !n.starts_with("sqlite_"));
    assert_eq!(
        names,
        [
            "repos",
            "session_messages",
            "session_messages_outbox",
            "sessions",
            "workspaces"
        ]
    );
    let mode: String = test
        .conn()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert!(test.root().is_dir());
}
