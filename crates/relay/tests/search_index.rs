//! The full-text index: what is indexed, how it ranks and scopes, and how the background thread
//! keeps it current.
//!
//! The source is the synthetic Conductor database of `support`; the index file lives in a
//! temporary directory of its own. The scoped-search cases port the reference tests "unscoped,
//! the dense chat wins the only slot", "scoped, the slot goes to the chat in scope rather than
//! to nothing", "an empty list matches nothing, never everything" and "no list at all is the old
//! unscoped search".

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use conductor_remote::db::ConductorDb;
use conductor_remote::search::index::{spawn_indexer, spawn_indexer_with, SearchIndex};
use rusqlite::{params, Connection};
use serde_json::json;
use support::TestDb;

const STAMP: &str = "2026-09-14T10:00:00.000Z";

fn insert_at(
    conn: &Connection,
    id: &str,
    session: Option<&str>,
    role: Option<&str>,
    content: &str,
    created_at: &str,
) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, session, role, content, created_at],
    )
    .unwrap();
}

fn insert(conn: &Connection, id: &str, session: Option<&str>, role: Option<&str>, content: &str) {
    insert_at(conn, id, session, role, content, STAMP);
}

/// A typed prompt.
fn prompt(conn: &Connection, id: &str, session: &str, text: &str) {
    insert(conn, id, Some(session), Some("user"), text);
}

fn assistant(text: &str) -> String {
    json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": text }] } })
        .to_string()
}

fn thinking(text: &str) -> String {
    json!({ "type": "assistant", "message": { "content": [
        { "type": "thinking", "thinking": text }
    ] } })
    .to_string()
}

fn tool_use(text: &str) -> String {
    json!({ "type": "assistant", "message": { "content": [
        { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": text } }
    ] } })
    .to_string()
}

fn tool_result(text: &str) -> String {
    json!({ "type": "user", "message": { "content": [
        { "type": "tool_result", "tool_use_id": "t1", "content": text }
    ] } })
    .to_string()
}

fn index_file(dir: &Path) -> PathBuf {
    dir.join("search.db")
}

fn open_index(dir: &Path) -> SearchIndex {
    SearchIndex::open(&index_file(dir)).expect("open the index")
}

/// Steps until the index reports nothing more; the `Ok` value of each step.
fn drain(index: &SearchIndex, source: &ConductorDb) -> Vec<bool> {
    let mut steps = Vec::new();
    loop {
        let more = index.index_step(source).expect("index step");
        steps.push(more);
        if !more {
            return steps;
        }
    }
}

/// Runs `sql` on the index file with a connection of the test's own.
fn raw<T>(dir: &Path, sql: &str, map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>) -> T {
    Connection::open(index_file(dir))
        .unwrap()
        .query_row(sql, [], map)
        .unwrap()
}

fn count(dir: &Path, sql: &str) -> i64 {
    raw(dir, sql, |row| row.get(0))
}

fn session_ids(
    index: &SearchIndex,
    expr: &str,
    sessions: Option<&[String]>,
    limit: usize,
) -> Vec<String> {
    index
        .search(expr, sessions, limit)
        .unwrap()
        .into_iter()
        .map(|hit| hit.session_id)
        .collect()
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_backfill_over_several_windows_indexes_prose_and_nothing_else() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = test.conn();
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..8500 {
            let id = format!("m{i}");
            match i % 5 {
                0 => prompt(&tx, &id, "chat", "userword typed"),
                1 => insert(&tx, &id, Some("chat"), None, &assistant("assistword said")),
                2 => insert(
                    &tx,
                    &id,
                    Some("chat"),
                    None,
                    &thinking("thinkword pondered"),
                ),
                3 => insert(&tx, &id, Some("chat"), None, &tool_use("toolword --run")),
                _ => insert(
                    &tx,
                    &id,
                    Some("chat"),
                    Some("user"),
                    &tool_result("resultword output"),
                ),
            }
        }
        // Rows with no place in the index: no chat, a blank prompt, a bookkeeping frame.
        insert(&tx, "orphan", None, None, &assistant("orphanword"));
        prompt(&tx, "blank", "chat", "  \n\t ");
        insert(
            &tx,
            "system",
            Some("chat"),
            None,
            &json!({ "type": "system", "subtype": "init", "note": "systemword" }).to_string(),
        );
        tx.commit().unwrap();
    }
    let source = test.db();
    let index = open_index(dir.path());

    assert_eq!(index.status().chunks, 0);
    assert!(index.index_step(&source).unwrap());
    let first = index.status();
    assert!(!first.ready);
    assert_eq!(first.progress, 4000.0 / 8503.0);

    // The first window is done: two more steps, the last one finding the end.
    assert_eq!(drain(&index, &source), vec![true, false]);
    let done = index.status();
    assert_eq!(done.chunks, 5100);
    assert!(done.ready);
    assert_eq!(done.progress, 1.0);
    assert_eq!(done.error, None);

    let hits = |word: &str| index.search(word, None, 10_000).unwrap();
    assert_eq!(hits("userword").len(), 1700);
    assert_eq!(hits("assistword").len(), 1700);
    assert_eq!(hits("thinkword").len(), 1700);
    for absent in ["toolword", "resultword", "orphanword", "systemword"] {
        assert!(hits(absent).is_empty(), "{absent} must not be indexed");
    }
    for role in ["user", "assistant", "thinking"] {
        assert_eq!(
            count(
                dir.path(),
                &format!("SELECT COUNT(*) FROM chunks WHERE role = '{role}'")
            ),
            1700
        );
    }
    // Each chunk keeps its chat, its source rowid and the row's timestamp.
    let one = &hits("thinkword")[0];
    assert_eq!(one.session_id, "chat");
    assert_eq!(one.role, "thinking");
    assert_eq!(one.at.as_deref(), Some(STAMP));
    assert_eq!(one.src_rowid % 5, 3);
}

#[test]
fn a_hit_has_no_time_only_when_the_row_has_none() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    let conn = test.conn();
    insert_at(&conn, "a", Some("chat"), Some("user"), "datedword", STAMP);
    insert_at(&conn, "b", Some("chat"), Some("user"), "undatedword", "");
    let index = open_index(dir.path());
    drain(&index, &test.db());

    let dated = index.search("datedword", None, 10).unwrap();
    assert_eq!(dated[0].at.as_deref(), Some(STAMP));
    let undated = index.search("undatedword", None, 10).unwrap();
    assert_eq!(undated[0].at, None);
}

#[test]
fn text_is_trimmed_and_clipped_to_64000_units_without_splitting_a_pair() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    let conn = test.conn();
    // 64,001 units: the pair would end at unit 64,001, so it is cut off whole.
    prompt(&conn, "a", "chat", &format!("{}😀", "a".repeat(63_999)));
    // Exactly 64,000 units: kept.
    prompt(&conn, "b", "chat", &format!("{}😀", "a".repeat(63_998)));
    prompt(&conn, "c", "chat", &"a".repeat(70_000));
    prompt(&conn, "d", "chat", "\u{feff} \n padded words \t ");
    let index = open_index(dir.path());
    drain(&index, &test.db());

    let length = |rowid: i64| {
        count(
            dir.path(),
            &format!("SELECT length(body) FROM chunks WHERE src_rowid = {rowid}"),
        )
    };
    assert_eq!(length(1), 63_999);
    assert_eq!(
        length(2),
        63_999,
        "the pair stays: 63,998 letters and one character"
    );
    assert_eq!(length(3), 64_000);
    let body: String = raw(
        dir.path(),
        "SELECT body FROM chunks WHERE src_rowid = 4",
        |row| row.get(0),
    );
    assert_eq!(body, "padded words");
    let emoji = count(
        dir.path(),
        "SELECT COUNT(*) FROM chunks WHERE src_rowid = 1 AND body LIKE '%😀%'",
    );
    assert_eq!(emoji, 0);
    let kept = count(
        dir.path(),
        "SELECT COUNT(*) FROM chunks WHERE src_rowid = 2 AND body LIKE '%😀'",
    );
    assert_eq!(kept, 1);
}

/// The reference's scope cases: a dense chat, a quiet one and one that does not match.
fn scoped_fixture() -> (TestDb, tempfile::TempDir, SearchIndex) {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    let conn = test.conn();
    prompt(&conn, "m1", "busy", "lamp lamp lamp lamp lamp");
    prompt(&conn, "m2", "quiet", "the lamp is on the desk");
    prompt(&conn, "m3", "other", "nothing about lights here");
    let index = open_index(dir.path());
    drain(&index, &test.db());
    (test, dir, index)
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

#[test]
fn unscoped_the_dense_chat_wins_the_only_slot() {
    let (_test, _dir, index) = scoped_fixture();
    assert_eq!(session_ids(&index, "\"lamp\"", None, 1), ["busy"]);
}

#[test]
fn scoped_the_slot_goes_to_the_chat_in_scope_rather_than_to_nothing() {
    let (_test, _dir, index) = scoped_fixture();
    let quiet = strings(&["quiet"]);
    assert_eq!(session_ids(&index, "\"lamp\"", Some(&quiet), 1), ["quiet"]);
    let two = strings(&["quiet", "other"]);
    assert_eq!(session_ids(&index, "\"lamp\"", Some(&two), 300), ["quiet"]);
}

#[test]
fn an_empty_list_matches_nothing_never_everything() {
    let (_test, _dir, index) = scoped_fixture();
    assert!(index.search("\"lamp\"", Some(&[]), 300).unwrap().is_empty());
}

#[test]
fn no_list_at_all_is_the_unscoped_search() {
    let (_test, _dir, index) = scoped_fixture();
    assert_eq!(
        session_ids(&index, "\"lamp\"", None, 300),
        ["busy", "quiet"]
    );
}

#[test]
fn snippets_carry_the_hit_markers_and_scores_rank_best_first() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    let conn = test.conn();
    prompt(&conn, "m1", "chat", "Please replace the lamp on the desk");
    let long = format!("{} lamp {}", "before ".repeat(40), "after ".repeat(40));
    prompt(&conn, "m2", "chat", &long);
    prompt(&conn, "m3", "chat", "lamp lamp lamp");
    let index = open_index(dir.path());
    drain(&index, &test.db());

    // Stemming: the plural matches, and the marks sit around the word as written.
    let hits = index.search("\"lamps\"", None, 10).unwrap();
    assert_eq!(hits.len(), 3);
    assert!(hits.windows(2).all(|pair| pair[0].score >= pair[1].score));
    assert_eq!(hits[0].src_rowid, 3);
    let short = hits.iter().find(|h| h.src_rowid == 1).unwrap();
    assert_eq!(
        short.snippet,
        "Please replace the \u{1}lamp\u{2} on the desk"
    );
    let clipped = hits.iter().find(|h| h.src_rowid == 2).unwrap();
    assert!(clipped.snippet.contains("\u{1}lamp\u{2}"));
    assert!(clipped.snippet.starts_with('…'), "{}", clipped.snippet);
}

#[test]
fn the_cursor_survives_a_restart() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = test.conn();
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..4500 {
            prompt(&tx, &format!("m{i}"), "chat", &format!("restartword {i}"));
        }
        tx.commit().unwrap();
    }
    let source = test.db();
    {
        let index = open_index(dir.path());
        assert!(index.index_step(&source).unwrap());
        assert_eq!(index.status().chunks, 4000);
    }

    // A new handle on the same file: the count comes from the file, the cursor too.
    let index = open_index(dir.path());
    let reopened = index.status();
    assert_eq!(reopened.chunks, 4000);
    assert!(!reopened.ready);
    assert_eq!(reopened.progress, 0.0);
    assert_eq!(drain(&index, &source), vec![false]);
    assert_eq!(index.status().chunks, 4500);
    assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM chunks"), 4500);
    assert_eq!(
        count(dir.path(), "SELECT COUNT(DISTINCT src_rowid) FROM chunks"),
        4500
    );

    // Rows that arrive while the relay is down are picked up from where it stopped.
    prompt(&test.conn(), "late", "chat", "restartword late");
    drop(index);
    let index = open_index(dir.path());
    assert_eq!(drain(&index, &source), vec![false]);
    assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM chunks"), 4501);
}

#[test]
fn a_changed_schema_drops_and_rebuilds_the_file() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = test.conn();
        for i in 0..3 {
            prompt(&conn, &format!("m{i}"), "chat", "schemaword");
        }
    }
    let source = test.db();
    {
        let index = open_index(dir.path());
        drain(&index, &source);
        assert_eq!(index.status().chunks, 3);
    }
    assert_eq!(
        count(
            dir.path(),
            "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'schema'"
        ),
        1
    );
    assert_eq!(
        count(
            dir.path(),
            "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'cursor'"
        ),
        3
    );

    Connection::open(index_file(dir.path()))
        .unwrap()
        .execute("UPDATE meta SET v = '99' WHERE k = 'schema'", [])
        .unwrap();
    let index = open_index(dir.path());
    assert_eq!(index.status().chunks, 0);
    assert!(index.search("schemaword", None, 10).unwrap().is_empty());
    assert_eq!(
        count(
            dir.path(),
            "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'schema'"
        ),
        1
    );
    assert_eq!(
        count(
            dir.path(),
            "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'cursor'"
        ),
        0
    );

    // The backfill starts over from the first row.
    drain(&index, &source);
    assert_eq!(index.status().chunks, 3);
    assert_eq!(index.search("schemaword", None, 10).unwrap().len(), 3);
}

#[test]
fn status_while_running_when_done_and_after_an_error() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = test.conn();
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..4500 {
            prompt(&tx, &format!("m{i}"), "chat", "statusword");
        }
        tx.commit().unwrap();
    }
    let source = test.db();
    let index = open_index(dir.path());

    let before = index.status();
    assert_eq!(
        (before.chunks, before.ready, before.progress),
        (0, false, 0.0)
    );
    assert_eq!(before.error, None);

    assert!(index.index_step(&source).unwrap());
    let running = index.status();
    assert_eq!(running.chunks, 4000);
    assert!(!running.ready);
    assert_eq!(running.progress, 4000.0 / 4500.0);

    assert!(!index.index_step(&source).unwrap());
    let done = index.status();
    assert_eq!(done.chunks, 4500);
    assert!(done.ready);
    assert_eq!(done.progress, 1.0);

    // A source that cannot be opened: the error is named by its kind, with no path in it.
    let missing = test.dir().join("absent-directory").join("gone.db");
    let broken = ConductorDb::new(&missing);
    assert!(index.index_step(&broken).is_err());
    let failed = index.status();
    let error = failed.error.expect("the failed step is reported");
    assert!(
        !error.contains("absent-directory") && !error.contains("gone.db"),
        "{error}"
    );
    assert!(!failed.ready);
    assert_eq!(failed.chunks, 4500);

    // The next step that works clears it.
    assert!(!index.index_step(&source).unwrap());
    let healed = index.status();
    assert_eq!(healed.error, None);
    assert!(healed.ready);
}

#[test]
fn an_empty_source_is_caught_up_at_once() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    let index = open_index(dir.path());
    assert_eq!(drain(&index, &test.db()), vec![false]);
    let status = index.status();
    assert_eq!(
        (status.chunks, status.ready, status.progress),
        (0, true, 1.0)
    );
}

#[test]
fn a_writer_that_moved_the_cursor_first_is_adopted_not_repeated() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = test.conn();
        for i in 0..10 {
            prompt(&conn, &format!("m{i}"), "chat", "sharedword");
        }
    }
    let source = test.db();
    // Two relays on one index file: both start from cursor 0.
    let first = open_index(dir.path());
    let second = open_index(dir.path());
    assert_eq!(drain(&first, &source), vec![false]);

    // The second one read the same window; its write is dropped and it adopts the cursor.
    assert!(second.index_step(&source).unwrap());
    assert_eq!(second.status().chunks, 10);
    assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM chunks"), 10);
    assert!(!second.index_step(&source).unwrap());
    assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM chunks"), 10);
    assert!(second.status().ready);
}

#[test]
fn a_source_whose_rowids_went_back_is_reindexed() {
    let old = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = old.conn();
        for i in 0..5 {
            prompt(&conn, &format!("o{i}"), "chat", "oldword");
        }
    }
    let source = old.db();
    let index = open_index(dir.path());
    drain(&index, &source);
    assert_eq!(index.status().chunks, 5);

    // Another file takes the place of the first: fewer rows, so the cursor is above its end.
    let new = TestDb::new();
    {
        let conn = new.conn();
        for i in 0..2 {
            prompt(&conn, &format!("n{i}"), "chat", "newword");
        }
    }
    std::fs::rename(new.path(), old.path()).unwrap();
    for suffix in ["-wal", "-shm"] {
        let mut name = old.path().as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }

    // The step that sees it drops everything and asks to be called again.
    assert!(index.index_step(&source).unwrap());
    assert_eq!(index.status().chunks, 0);
    assert!(index.search("oldword", None, 10).unwrap().is_empty());
    assert_eq!(
        count(
            dir.path(),
            "SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'cursor'"
        ),
        0
    );

    assert!(!index.index_step(&source).unwrap());
    let status = index.status();
    assert_eq!(
        (status.chunks, status.ready, status.progress),
        (2, true, 1.0)
    );
    assert_eq!(index.search("newword", None, 10).unwrap().len(), 2);
    assert!(index.search("oldword", None, 10).unwrap().is_empty());
}

#[test]
fn spawn_indexer_with_indexes_new_rows_after_a_data_change_and_stops_when_asked() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = test.conn();
        for i in 0..3 {
            prompt(&conn, &format!("m{i}"), "chat", "threadword");
        }
    }
    let index = Arc::new(open_index(dir.path()));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_indexer_with(
        Arc::clone(&index),
        test.path().to_path_buf(),
        || true,
        Arc::clone(&stop),
        Duration::from_millis(50),
    );

    wait_until("the first pass", || {
        let status = index.status();
        status.ready && status.chunks == 3
    });

    // A commit by another connection changes the data version: the next pass picks it up.
    {
        let conn = test.conn();
        prompt(&conn, "later1", "chat", "threadword");
        insert(
            &conn,
            "later2",
            Some("chat"),
            None,
            &assistant("threadword again"),
        );
    }
    wait_until("the second pass", || index.status().chunks == 5);
    assert!(index.status().ready);
    assert_eq!(index.search("threadword", None, 10).unwrap().len(), 5);

    stop.store(true, Ordering::Relaxed);
    let asked = Instant::now();
    handle.join().unwrap();
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "{:?}",
        asked.elapsed()
    );
}

#[test]
fn the_indexer_leaves_conductor_alone_while_it_is_not_running() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    prompt(&test.conn(), "m1", "chat", "idleword");
    let index = Arc::new(open_index(dir.path()));
    let stop = Arc::new(AtomicBool::new(false));
    // Conductor is not running; the default thread waits 15 seconds between looks.
    let handle = spawn_indexer(
        Arc::clone(&index),
        test.path().to_path_buf(),
        || false,
        Arc::clone(&stop),
    );

    std::thread::sleep(Duration::from_millis(300));
    let status = index.status();
    assert_eq!((status.chunks, status.ready), (0, false));
    assert_eq!(status.progress, 0.0);

    // The long sleep ends when asked to.
    stop.store(true, Ordering::Relaxed);
    let asked = Instant::now();
    handle.join().unwrap();
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "{:?}",
        asked.elapsed()
    );
}

#[test]
fn a_failing_step_is_kept_in_the_status_and_retried() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(open_index(dir.path()));
    let stop = Arc::new(AtomicBool::new(false));
    // The file Conductor would own does not exist yet.
    let source_path = test.dir().join("later.db");
    let handle = spawn_indexer_with(
        Arc::clone(&index),
        source_path.clone(),
        || true,
        Arc::clone(&stop),
        Duration::from_millis(50),
    );
    wait_until("the error", || index.status().error.is_some());
    assert!(!index.status().ready);

    // It appears: the retry after the idle interval succeeds and clears the error.
    std::fs::copy(test.path(), &source_path).unwrap();
    let conn = Connection::open(&source_path).unwrap();
    prompt(&conn, "m1", "chat", "retryword");
    drop(conn);
    wait_until("the retry", || {
        let status = index.status();
        status.error.is_none() && status.ready && status.chunks == 1
    });

    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn wal_file(dir: &Path) -> PathBuf {
    dir.join("search.db-wal")
}

#[test]
fn a_new_index_file_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let _index = open_index(dir.path());
    assert_eq!(mode_of(&index_file(dir.path())), 0o600);
}

#[test]
fn an_existing_world_readable_index_file_becomes_private_on_open() {
    let dir = tempfile::tempdir().unwrap();
    drop(open_index(dir.path()));
    let file = index_file(dir.path());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    // A leftover write-ahead log and shared-memory file, as a crashed relay leaves them.
    let wal = wal_file(dir.path());
    let shm = dir.path().join("search.db-shm");
    for side in [&wal, &shm] {
        std::fs::write(side, b"").unwrap();
        std::fs::set_permissions(side, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    assert_eq!(mode_of(&file), 0o644);

    let _index = open_index(dir.path());
    assert_eq!(mode_of(&file), 0o600);
    assert_eq!(mode_of(&wal), 0o600);
    assert_eq!(mode_of(&shm), 0o600);
}

#[test]
fn the_write_ahead_log_after_writes_is_private() {
    let test = TestDb::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let conn = test.conn();
        prompt(&conn, "m1", "chat", "secretword typed");
    }
    let index = open_index(dir.path());
    drain(&index, &test.db());
    assert_eq!(index.status().chunks, 1);

    let wal = wal_file(dir.path());
    assert!(wal.exists(), "the index is in WAL mode and still open");
    assert_eq!(mode_of(&wal), 0o600);
    assert_eq!(mode_of(&index_file(dir.path())), 0o600);
}
