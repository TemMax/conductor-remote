//! The fold of background-task frames, fed one row at a time, and the chat list that keeps one
//! fold per chat between builds.
//!
//! Every row comes from a temporary SQLite file of its own (the table of folds is process-wide
//! but keyed by the database path) or is built in a test.

mod support;

use std::sync::Arc;

use conductor_remote::reads::background::{
    open_background_tasks, timestamp_ms, BackgroundFold, TaskFrameRow,
};
use conductor_remote::reads::extras::commands::Commands;
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::sessions::{task_frames_read, BackgroundTask};
use conductor_remote::reads::Reads;
use conductor_remote::testing::FakeCommands;
use rusqlite::{params, Connection};
use support::TestDb;

const HOME: &str = "/Users/tester";
const WORKSPACE: &str = "fold-workspace";
const CHAT: &str = "5e1fa2c4-0000-4000-8000-000000000001";

// ---------------------------------------------------------------- the fold

fn row(created_at: &str, content: &str) -> TaskFrameRow {
    TaskFrameRow {
        created_at: created_at.to_owned(),
        content: content.to_owned(),
    }
}

fn started(task_id: &str, created_at: &str, description: &str) -> TaskFrameRow {
    row(
        created_at,
        &serde_json::json!({
            "type": "system",
            "subtype": "task_started",
            "session_id": "s",
            "task_id": task_id,
            "tool_use_id": format!("toolu_{task_id}"),
            "description": description,
            "task_type": "local_bash",
        })
        .to_string(),
    )
}

fn notified(task_id: &str, created_at: &str, status: &str) -> TaskFrameRow {
    row(
        created_at,
        &serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "status": status,
            "task_id": task_id,
            "tool_use_id": format!("toolu_{task_id}"),
        })
        .to_string(),
    )
}

fn t0() -> i64 {
    timestamp_ms("2026-09-02T10:00:00.000Z").unwrap()
}

/// The fold fed row by row gives, after every row, what `open_background_tasks` gives for the
/// rows so far.
fn assert_fold_equals_function(rows: &[TaskFrameRow], process_start: i64) {
    let mut fold = BackgroundFold::new(process_start);
    assert_eq!(fold.tasks(), open_background_tasks(&[], process_start));
    for (at, frame) in rows.iter().enumerate() {
        fold.push(frame);
        assert_eq!(
            fold.tasks(),
            open_background_tasks(&rows[..=at], process_start),
            "after row {at}"
        );
    }
}

#[test]
fn the_fold_equals_the_function_over_the_reference_cases() {
    // A started task without a notification.
    assert_fold_equals_function(
        &[started(
            "a",
            "2026-09-02T10:48:47.856Z",
            "Wait until noon before the next poll",
        )],
        t0(),
    );

    // A notification closes its own task whatever its status.
    assert_fold_equals_function(
        &[
            started("a", "2026-09-02T10:01:00.000Z", "a"),
            started("b", "2026-09-02T10:02:00.000Z", "b"),
            started("c", "2026-09-02T10:03:00.000Z", "c"),
            notified("a", "2026-09-02T10:05:00.000Z", "completed"),
            notified("c", "2026-09-02T10:06:00.000Z", "failed"),
        ],
        t0(),
    );

    // A task started before its process is dead.
    assert_fold_equals_function(
        &[
            started("old", "2026-09-02T10:01:00.000Z", "old"),
            started("new", "2026-09-02T10:06:00.000Z", "new"),
        ],
        timestamp_ms("2026-09-02T10:05:00.000Z").unwrap(),
    );

    // An abort frame is not a close.
    assert_fold_equals_function(
        &[
            started("a", "2026-09-02T10:01:00.000Z", "a"),
            row(
                "2026-09-02T10:02:00.000Z",
                r#"{"type":"error","content":"aborted by user"}"#,
            ),
        ],
        t0(),
    );

    // Frames without a task id, other frames and unparseable rows are ignored.
    assert_fold_equals_function(
        &[
            row(
                "2026-09-02T10:01:00.000Z",
                r#"{"type":"system","subtype":"background_tasks_changed"}"#,
            ),
            row(
                "2026-09-02T10:01:01.000Z",
                r#"{"type":"system","subtype":"task_started"}"#,
            ),
            row(
                "2026-09-02T10:01:01.500Z",
                r#"{"type":"system","subtype":"task_started","task_id":"  "}"#,
            ),
            row("2026-09-02T10:01:02.000Z", "not json"),
            row("2026-09-02T10:01:02.500Z", "null"),
            row(
                "2026-09-02T10:01:03.000Z",
                r#"{"type":"assistant","message":{"content":[]}}"#,
            ),
            row(
                "2026-09-02T10:01:04.000Z",
                r#"{"type":"user","subtype":"task_started","task_id":"x"}"#,
            ),
        ],
        t0(),
    );

    // A task without a description is named by its type, and without a type by "task".
    assert_fold_equals_function(
        &[
            row(
                "2026-09-02T10:01:00.000Z",
                r#"{"type":"system","subtype":"task_started","task_id":"x","task_type":"local_agent"}"#,
            ),
            row(
                "2026-09-02T10:01:01.000Z",
                r#"{"type":"system","subtype":"task_started","task_id":"y","tool_use_id":"  toolu_y  "}"#,
            ),
        ],
        0,
    );

    // A restarted task is replaced in place; a removed one goes to the end when it starts again.
    assert_fold_equals_function(
        &[
            started("a", "2026-09-02T10:01:00.000Z", "first"),
            started("b", "2026-09-02T10:02:00.000Z", "second"),
            started("a", "2026-09-02T10:03:00.000Z", "again"),
        ],
        t0(),
    );
    assert_fold_equals_function(
        &[
            started("a", "2026-09-02T10:01:00.000Z", "a"),
            started("b", "2026-09-02T10:02:00.000Z", "b"),
            notified("a", "2026-09-02T10:03:00.000Z", "completed"),
            started("a", "2026-09-02T10:04:00.000Z", "a"),
        ],
        t0(),
    );

    // A task whose start cannot be read is skipped.
    assert_fold_equals_function(
        &[
            started("a", "yesterday", "a"),
            started("b", "2026-09-02T10:01:00", "no zone"),
        ],
        0,
    );
}

#[test]
fn a_fold_keeps_what_it_was_fed_and_continues_with_later_rows() {
    let mut fold = BackgroundFold::new(t0());
    fold.push(&started("a", "2026-09-02T10:01:00.000Z", "a"));
    fold.push(&started("b", "2026-09-02T10:02:00.000Z", "b"));
    let before = fold.tasks();
    assert_eq!(before.len(), 2);
    fold.push(&notified("a", "2026-09-02T10:03:00.000Z", "completed"));
    assert_eq!(before.len(), 2, "tasks() returns a copy");
    assert_eq!(ids(&fold.tasks()), ["b"]);
}

fn ids(tasks: &[BackgroundTask]) -> Vec<&str> {
    tasks.iter().map(|task| task.task_id.as_str()).collect()
}

// ------------------------------------------------- the chat list keeps the fold

/// `YYYY-MM-DD HH:MM:SS` (UTC, the stored form) of a time in milliseconds since the epoch.
fn stored_time(ms: i64) -> String {
    let seconds = ms.div_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // Civil date of a day count (proleptic Gregorian calendar).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3_600,
        rest / 60 % 60,
        rest % 60
    )
}

fn real_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn task_started_json(task_id: &str) -> String {
    serde_json::json!({
        "type": "system",
        "subtype": "task_started",
        "task_id": task_id,
        "tool_use_id": format!("toolu_{task_id}"),
        "description": format!("Task {task_id}"),
        "task_type": "local_bash",
    })
    .to_string()
}

fn task_notification_json(task_id: &str) -> String {
    serde_json::json!({
        "type": "system",
        "subtype": "task_notification",
        "status": "completed",
        "task_id": task_id,
    })
    .to_string()
}

const NOT_A_TASK: &str = r#"{"type":"assistant","message":{"content":[]}}"#;

/// A database with one open chat and no messages.
fn chat_database() -> TestDb {
    let test = TestDb::new();
    test.conn()
        .execute(
            "INSERT INTO sessions (id, status, title, agent_type, workspace_id, created_at, \
             updated_at) VALUES (?1, 'idle', 'Waiting', 'claude', ?2, '2026-03-01 08:00:00', \
             '2026-03-01 08:00:00')",
            params![CHAT, WORKSPACE],
        )
        .unwrap();
    test
}

fn insert(conn: &Connection, id: &str, created_at: &str, content: &str) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content, created_at) \
         VALUES (?1, ?2, 'system', ?3, ?4)",
        params![id, CHAT, content, created_at],
    )
    .unwrap();
}

/// A frame time later than any process start of a test.
const LATER: &str = "2099-01-01 00:00:00";

/// Reads over `test` whose process list says the chat's agent has run for `etime`, with the
/// listing already fetched.
fn reads_with_agent(test: &TestDb, etime: &str) -> (Reads, Arc<Extras>) {
    let fake = Arc::new(FakeCommands::new());
    fake.on(
        "ps",
        &["-axo"],
        0,
        &format!("100 {etime} claude --resume={CHAT}\n"),
    );
    let commands: Arc<dyn Commands> = fake;
    let extras = Arc::new(Extras::new(commands, HOME));
    let reads = Reads::new(test.db(), test.root()).with_extras(extras.clone());
    // The first listing has no process facts and queues the process list.
    reads.list_sessions(WORKSPACE).unwrap();
    extras.wait_idle();
    (reads, extras)
}

fn tasks_of(reads: &Reads) -> Vec<String> {
    let sessions = reads.list_sessions(WORKSPACE).unwrap();
    assert_eq!(sessions.len(), 1);
    sessions[0]
        .background_tasks
        .iter()
        .map(|task| task.task_id.clone())
        .collect()
}

#[test]
fn a_build_after_new_frames_reads_only_those() {
    let test = chat_database();
    let conn = test.conn();
    insert(&conn, "m1", LATER, &task_started_json("a"));
    insert(&conn, "m2", LATER, &task_started_json("b"));
    insert(&conn, "m3", LATER, NOT_A_TASK);
    let (reads, _extras) = reads_with_agent(&test, "10:00");
    let read = || task_frames_read(test.path(), CHAT);
    assert_eq!(read(), 0, "no process was known at the first build");

    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(read(), 2, "the first build reads every task frame");

    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(read(), 2, "nothing new: nothing read");

    insert(&conn, "m4", LATER, &task_notification_json("a"));
    insert(&conn, "m5", LATER, NOT_A_TASK);
    insert(&conn, "m6", LATER, &task_started_json("c"));
    assert_eq!(tasks_of(&reads), ["b", "c"]);
    assert_eq!(read(), 4, "only the two new task frames were read");

    assert_eq!(tasks_of(&reads), ["b", "c"]);
    assert_eq!(read(), 4);
}

#[test]
fn the_kept_fold_gives_what_a_fresh_one_over_all_the_rows_gives() {
    let test = chat_database();
    let conn = test.conn();
    let (reads, _extras) = reads_with_agent(&test, "10:00");
    let mut frames = Vec::new();
    for (at, content) in [
        task_started_json("a"),
        task_started_json("b"),
        task_notification_json("a"),
        task_started_json("a"),
        NOT_A_TASK.to_owned(),
        task_notification_json("b"),
        task_started_json("c"),
    ]
    .into_iter()
    .enumerate()
    {
        insert(&conn, &format!("m{at}"), LATER, &content);
        frames.push(row(LATER, &content));
        // One build after every row, so every step continues the kept fold.
        let expected: Vec<String> = open_background_tasks(&frames, 0)
            .into_iter()
            .map(|task| task.task_id)
            .collect();
        assert_eq!(tasks_of(&reads), expected, "after row {at}");
    }
}

#[test]
fn a_restarted_process_gives_a_fresh_fold() {
    let test = chat_database();
    let conn = test.conn();
    // Started five minutes ago: open for a process of ten minutes, dead for one of five seconds.
    let five_minutes_ago = stored_time(real_now_ms() - 5 * 60_000);
    insert(&conn, "m1", &five_minutes_ago, &task_started_json("old"));
    insert(&conn, "m2", LATER, &task_started_json("new"));
    let read = || task_frames_read(test.path(), CHAT);

    let (before, _extras) = reads_with_agent(&test, "10:00");
    assert_eq!(tasks_of(&before), ["old", "new"]);
    assert_eq!(read(), 2);
    assert_eq!(tasks_of(&before), ["old", "new"]);
    assert_eq!(read(), 2);

    // The same chat after its agent was restarted: a new process start, the same rows.
    let (after, _extras) = reads_with_agent(&test, "00:05");
    assert_eq!(tasks_of(&after), ["new"]);
    assert_eq!(read(), 4, "the fold started again from the first frame");
    assert_eq!(tasks_of(&after), ["new"]);
    assert_eq!(read(), 4);
}

#[test]
fn deleted_newest_frames_and_reused_rowids_rebuild_the_fold() {
    let test = chat_database();
    let conn = test.conn();
    insert(&conn, "m1", LATER, &task_started_json("a"));
    insert(&conn, "m2", LATER, &task_started_json("b"));
    insert(&conn, "m3", LATER, &task_notification_json("a"));
    let (reads, _extras) = reads_with_agent(&test, "10:00");
    let read = || task_frames_read(test.path(), CHAT);
    assert_eq!(tasks_of(&reads), ["b"]);
    assert_eq!(read(), 3);

    // The newest two frames go; three rows come, the first two taking the freed rowids 2 and 3,
    // so no row is newer than the last one the fold has read.
    conn.execute("DELETE FROM session_messages WHERE id IN ('m2', 'm3')", [])
        .unwrap();
    insert(&conn, "m4", LATER, &task_started_json("c"));
    insert(&conn, "m5", LATER, NOT_A_TASK);
    insert(&conn, "m6", LATER, NOT_A_TASK);
    let rowid_of_m4: i64 = conn
        .query_row(
            "SELECT rowid FROM session_messages WHERE id = 'm4'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rowid_of_m4, 2, "the rowid of a deleted frame is reused");

    assert_eq!(tasks_of(&reads), ["a", "c"]);
    assert_eq!(read(), 5, "the two remaining task frames were read again");
    assert_eq!(tasks_of(&reads), ["a", "c"]);
    assert_eq!(read(), 5);
}

#[test]
fn a_deleted_non_frame_row_and_an_inserted_one_rebuild_the_fold_with_the_same_answer() {
    let test = chat_database();
    let conn = test.conn();
    insert(&conn, "m1", LATER, &task_started_json("a"));
    insert(&conn, "m2", LATER, NOT_A_TASK);
    insert(&conn, "m3", LATER, &task_started_json("b"));
    insert(&conn, "m4", LATER, NOT_A_TASK);
    let (reads, _extras) = reads_with_agent(&test, "10:00");
    let read = || task_frames_read(test.path(), CHAT);
    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(read(), 2);

    // A row in the middle goes and a new one comes: the rows up to the newest one read are fewer.
    conn.execute("DELETE FROM session_messages WHERE id = 'm2'", [])
        .unwrap();
    insert(&conn, "m5", LATER, NOT_A_TASK);
    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(read(), 4, "the rows changed: the fold started again");
    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(read(), 4);

    // The newest row goes and another takes its rowid: as many rows up to it, other rows.
    conn.execute("DELETE FROM session_messages WHERE id = 'm5'", [])
        .unwrap();
    insert(&conn, "m6", LATER, NOT_A_TASK);
    let rowid_of = |id: &str| -> i64 {
        conn.query_row(
            "SELECT rowid FROM session_messages WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        rowid_of("m6"),
        5,
        "the rowid of the deleted newest row is reused"
    );
    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(
        read(),
        6,
        "the newest row is another one: the fold started again"
    );
    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(read(), 6);
}

#[test]
fn added_non_frame_rows_keep_the_fold_and_only_new_frames_are_read() {
    let test = chat_database();
    let conn = test.conn();
    insert(&conn, "m1", LATER, &task_started_json("a"));
    insert(&conn, "m2", LATER, NOT_A_TASK);
    let (reads, _extras) = reads_with_agent(&test, "10:00");
    let read = || task_frames_read(test.path(), CHAT);
    assert_eq!(tasks_of(&reads), ["a"]);
    assert_eq!(read(), 1);

    insert(&conn, "m3", LATER, NOT_A_TASK);
    insert(&conn, "m4", LATER, NOT_A_TASK);
    assert_eq!(tasks_of(&reads), ["a"]);
    assert_eq!(
        read(),
        1,
        "only non-frame rows came: the fold was kept and nothing read"
    );

    insert(&conn, "m5", LATER, NOT_A_TASK);
    insert(&conn, "m6", LATER, &task_started_json("b"));
    insert(&conn, "m7", LATER, NOT_A_TASK);
    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(read(), 2, "the fold was kept and read only the new frame");
    assert_eq!(tasks_of(&reads), ["a", "b"]);
    assert_eq!(read(), 2);
}

#[test]
fn a_build_starts_after_the_newest_row_already_read_not_after_the_last_frame() {
    let test = chat_database();
    let conn = test.conn();
    // One early task frame, then a long history of other rows.
    insert(&conn, "m1", LATER, &task_started_json("early"));
    for at in 2..=40 {
        insert(&conn, &format!("m{at}"), LATER, NOT_A_TASK);
    }
    let (reads, _extras) = reads_with_agent(&test, "10:00");
    let read = || task_frames_read(test.path(), CHAT);
    assert_eq!(tasks_of(&reads), ["early"]);
    assert_eq!(read(), 1);

    // A row the first build already looked at (and found not to be a frame) now holds a task
    // frame, with the same id, rowid and number of rows, so the kept fold stays valid. A read
    // that starts after the newest row already examined does not see it; one that starts after
    // the early frame would.
    let content = task_started_json("unseen");
    conn.execute(
        "UPDATE session_messages SET content = ?1 WHERE id = 'm20'",
        [&content],
    )
    .unwrap();
    insert(&conn, "m41", LATER, NOT_A_TASK);
    assert_eq!(tasks_of(&reads), ["early"]);
    assert_eq!(
        read(),
        1,
        "the second build read no row before the previous newest one"
    );

    // A frame inserted after it is the only one read.
    insert(&conn, "m42", LATER, &task_started_json("late"));
    assert_eq!(tasks_of(&reads), ["early", "late"]);
    assert_eq!(read(), 2, "only the frame after the previous read was read");
    assert_eq!(tasks_of(&reads), ["early", "late"]);
    assert_eq!(read(), 2);
}
