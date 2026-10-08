//! The full-text index of the chats' prose, kept in a file of the relay's own.

use std::fs::{OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::types::Value;
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::Serialize;

use crate::db::{ConductorDb, DataVersion, DbError};
use crate::transcript::{parse_message, StoredMessage, TranscriptRole};

/// Stored in the file's `meta` table. A file with another value is dropped and rebuilt.
const SCHEMA_VERSION: &str = "1";
/// Source rowids one step advances over. The cursor moves by scanned rowid, not by matched
/// rowid, so rows without prose are not scanned again.
const WINDOW_ROWS: i64 = 4000;
/// The longest chunk body, in UTF-16 code units.
const MAX_CHUNK_UNITS: usize = 64_000;
const BACKFILL_PAUSE: Duration = Duration::from_millis(5);
const NOT_RUNNING_PAUSE: Duration = Duration::from_secs(15);
/// The longest a sleeping indexer thread goes without looking at `stop`.
const SLEEP_SLICE: Duration = Duration::from_millis(20);
/// Marks around a matched word in a snippet; the phone splits on the same two characters.
const HIT_OPEN: &str = "\u{1}";
const HIT_CLOSE: &str = "\u{2}";

/// The web app's `IndexStatus`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IndexStatus {
    /// Chunks indexed so far.
    pub chunks: i64,
    /// True once the backfill has reached the newest message.
    pub ready: bool,
    /// 0 to 1 through the source rows; 1 when caught up.
    pub progress: f64,
    /// The kind of the error the last step ended with, without a path or any content. Cleared
    /// by the next step that succeeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One indexed chunk that matched.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkHit {
    pub session_id: String,
    pub src_rowid: i64,
    pub role: String,
    pub at: Option<String>,
    pub score: f64,
    pub snippet: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("search index: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("search index source: {0}")]
    Source(#[from] crate::db::DbError),
}

impl IndexError {
    /// What went wrong, in words that carry no path and no row content.
    fn kind(&self) -> String {
        match self {
            Self::Sqlite(e) => sqlite_kind("sqlite", e),
            Self::Source(DbError::Open { .. }) => "source: open".to_owned(),
            Self::Source(DbError::Query(e)) => sqlite_kind("source", e),
        }
    }
}

fn sqlite_kind(prefix: &str, error: &rusqlite::Error) -> String {
    match error.sqlite_error_code() {
        Some(code) => format!("{prefix}: {code:?}"),
        None => prefix.to_owned(),
    }
}

/// What a step knows between calls.
struct State {
    /// The last source rowid indexed; mirrors `meta.cursor`.
    cursor: i64,
    /// Rows of `chunks`: counted once at `open`, then kept by the steps.
    chunks: i64,
    /// `MAX(rowid)` of the source as the last step read it; `None` before the first step.
    source_max: Option<i64>,
    ready: bool,
    error: Option<String>,
}

/// The index file and its state.
pub struct SearchIndex {
    /// The only connection that writes.
    writer: Mutex<Connection>,
    /// Read-only, so a search never waits for a write.
    reader: Mutex<Connection>,
    state: Mutex<State>,
}

/// One piece of prose, ready to insert.
struct Chunk {
    body: String,
    session_id: String,
    src_rowid: i64,
    role: &'static str,
    at: String,
}

/// A source row the index reads.
struct SourceRow {
    message: StoredMessage,
    session_id: String,
}

/// What one read of the source returned.
struct Window {
    /// `MAX(rowid)` of the source, 0 when it is empty.
    max: i64,
    /// The cursor is above `max`: the file was replaced by a smaller one.
    replaced: bool,
    /// The last rowid of the window; `None` when no rowid lies after the cursor.
    end: Option<i64>,
    /// The window holds `WINDOW_ROWS` rowids, so more may follow.
    full: bool,
    rows: Vec<SourceRow>,
}

/// What a write did.
enum Committed {
    Applied,
    /// Another writer moved the cursor first; this step's work was dropped.
    Adopted,
}

enum Change<'a> {
    Append { chunks: &'a [Chunk], end: i64 },
    Reset,
}

/// The mode of the index file and its `-wal` and `-shm` files: they hold the chats' text.
const PRIVATE_MODE: u32 = 0o600;

/// `<path>-wal` or `<path>-shm`: SQLite's own naming.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Makes the index file private before SQLite opens it: a missing file is created with mode
/// 0600, an existing one (and its `-wal` and `-shm` files, when present) is set to 0600. A
/// failure is logged, without the path, and does not stop the open. SQLite gives the sidecar
/// files it creates later the mode of the main file.
fn restrict_permissions(path: &Path) {
    if !path.exists() {
        if let Err(error) = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
        {
            tracing::warn!(kind = ?error.kind(), "search index file could not be created private");
        }
    }
    for file in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        if !file.exists() {
            continue;
        }
        if let Err(error) = std::fs::set_permissions(&file, Permissions::from_mode(PRIVATE_MODE)) {
            tracing::warn!(kind = ?error.kind(), "search index file permissions could not be set");
        }
    }
}

impl SearchIndex {
    /// Opens the index file, creating it with mode 0600. A file of another schema is dropped
    /// and rebuilt.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        restrict_permissions(path);
        let mut writer = Connection::open(path)?;
        writer.execute_batch(
            "PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
        )?;
        {
            // Immediate, and the check inside it: two relays opening one file agree on who
            // rebuilds.
            let tx = writer.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT)")?;
            let schema: Option<String> = tx
                .query_row("SELECT v FROM meta WHERE k = 'schema'", [], |row| {
                    row.get(0)
                })
                .optional()?;
            if schema.as_deref() != Some(SCHEMA_VERSION) {
                tx.execute_batch(
                    "DROP TABLE IF EXISTS chunks;
                     DROP TABLE IF EXISTS meta;
                     CREATE TABLE meta(k TEXT PRIMARY KEY, v TEXT);
                     CREATE VIRTUAL TABLE chunks USING fts5(
                         body,
                         session_id UNINDEXED,
                         src_rowid UNINDEXED,
                         role UNINDEXED,
                         at UNINDEXED,
                         tokenize='porter unicode61'
                     );",
                )?;
                tx.execute(
                    "INSERT INTO meta(k, v) VALUES ('schema', ?1), ('cursor', '0')",
                    [SCHEMA_VERSION],
                )?;
            }
            tx.commit()?;
        }
        let cursor = stored_cursor(&writer)?;
        let chunks: i64 = writer.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;

        let reader = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        reader.busy_timeout(Duration::from_millis(5000))?;

        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
            state: Mutex::new(State {
                cursor,
                chunks,
                source_max: None,
                ready: false,
                error: None,
            }),
        })
    }

    /// Where the index stands, from memory: it reads no file.
    pub fn status(&self) -> IndexStatus {
        let state = self.lock_state();
        let progress = match state.source_max {
            None => 0.0,
            Some(0) => 1.0,
            Some(max) => (state.cursor as f64 / max as f64).min(1.0),
        };
        IndexStatus {
            chunks: state.chunks,
            ready: state.ready,
            progress,
            error: state.error.clone(),
        }
    }

    /// Indexes the next window of the source's rows; `Ok(true)` while more remain.
    pub fn index_step(&self, source: &ConductorDb) -> Result<bool, IndexError> {
        let result = self.step(source);
        let mut state = self.lock_state();
        match &result {
            Ok(more) => {
                state.ready = !*more;
                state.error = None;
            }
            Err(error) => {
                state.ready = false;
                state.error = Some(error.kind());
            }
        }
        result
    }

    fn step(&self, source: &ConductorDb) -> Result<bool, IndexError> {
        let starting = self.lock_state().cursor;
        // One read of the source per step: the maximum, the window and its rows.
        let window = source.read("search.index.window", |conn| read_window(conn, starting))?;
        self.lock_state().source_max = Some(window.max);

        if window.replaced {
            // The source holds fewer rows than were indexed: it is another file. Start over.
            self.commit(starting, Change::Reset)?;
            return Ok(true);
        }
        let Some(end) = window.end else {
            return Ok(false);
        };
        // Parsed before the write transaction opens: another relay may be waiting for it.
        let chunks = build_chunks(&window.rows);
        match self.commit(
            starting,
            Change::Append {
                chunks: &chunks,
                end,
            },
        )? {
            Committed::Applied => Ok(window.full),
            Committed::Adopted => Ok(true),
        }
    }

    /// Applies `change` and moves the cursor in one `BEGIN IMMEDIATE` transaction, unless the
    /// stored cursor is no longer `starting`.
    fn commit(&self, starting: i64, change: Change<'_>) -> Result<Committed, IndexError> {
        let mut writer = self.lock_writer();
        let tx = writer.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = stored_cursor(&tx)?;
        if stored != starting {
            tx.rollback()?;
            let chunks: i64 =
                writer.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
            let mut state = self.lock_state();
            state.cursor = stored;
            state.chunks = chunks;
            return Ok(Committed::Adopted);
        }
        let (cursor, added) = match change {
            Change::Append { chunks, end } => {
                insert_chunks(&tx, chunks)?;
                (end, Some(chunks.len() as i64))
            }
            Change::Reset => {
                tx.execute("DELETE FROM chunks", [])?;
                (0, None)
            }
        };
        tx.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('cursor', ?1)",
            [cursor.to_string()],
        )?;
        tx.commit()?;
        let mut state = self.lock_state();
        state.cursor = cursor;
        match added {
            Some(added) => state.chunks += added,
            None => state.chunks = 0,
        }
        Ok(Committed::Applied)
    }

    /// `match_expr` is an FTS5 expression; `sessions` limits the hits when given (an empty list
    /// matches nothing).
    pub fn search(
        &self,
        match_expr: &str,
        sessions: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<ChunkHit>, IndexError> {
        let mut args = vec![
            Value::Text(HIT_OPEN.to_owned()),
            Value::Text(HIT_CLOSE.to_owned()),
            Value::Text(match_expr.to_owned()),
        ];
        let scope = match sessions {
            Some([]) => return Ok(Vec::new()),
            Some(list) => {
                args.push(Value::Text(
                    serde_json::to_string(list).expect("strings serialise"),
                ));
                "AND session_id IN (SELECT value FROM json_each(?4))"
            }
            None => "",
        };
        args.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        let sql = format!(
            "SELECT session_id, src_rowid, role, at, -bm25(chunks) AS score, \
                    snippet(chunks, 0, ?1, ?2, '…', 24) AS snippet \
             FROM chunks WHERE chunks MATCH ?3 {scope} ORDER BY bm25(chunks) LIMIT ?{}",
            args.len()
        );
        let reader = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = reader.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |row| {
            let at: Option<String> = row.get(3)?;
            Ok(ChunkHit {
                session_id: row.get(0)?,
                src_rowid: row.get(1)?,
                role: row.get(2)?,
                at: at.filter(|at| !at.is_empty()),
                score: row.get(4)?,
                snippet: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_writer(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The cursor in the file; 0 when there is none.
fn stored_cursor(conn: &Connection) -> rusqlite::Result<i64> {
    let value: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'cursor'", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
}

fn read_window(conn: &Connection, cursor: i64) -> rusqlite::Result<Window> {
    let max: i64 = conn.query_row(
        "SELECT COALESCE(MAX(rowid), 0) FROM session_messages",
        [],
        |row| row.get(0),
    )?;
    let mut window = Window {
        max,
        replaced: cursor > max,
        end: None,
        full: false,
        rows: Vec::new(),
    };
    if window.replaced {
        return Ok(window);
    }
    let (end, count): (Option<i64>, i64) = conn.query_row(
        "SELECT MAX(rowid), COUNT(*) FROM \
         (SELECT rowid FROM session_messages WHERE rowid > ?1 ORDER BY rowid LIMIT ?2)",
        params![cursor, WINDOW_ROWS],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let Some(end) = end else {
        return Ok(window);
    };
    window.end = Some(end);
    window.full = count >= WINDOW_ROWS;
    let mut stmt = conn.prepare_cached(
        "SELECT rowid, id, session_id, content, created_at, sent_at, queue_order \
         FROM session_messages \
         WHERE rowid > ?1 AND rowid <= ?2 AND session_id IS NOT NULL \
           AND (role = 'user' OR content LIKE '%\"type\":\"text\"%' \
                OR content LIKE '%\"type\":\"thinking\"%') \
         ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![cursor, end], |row| {
        Ok(SourceRow {
            message: StoredMessage {
                rowid: row.get(0)?,
                id: row.get(1)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
                sent_at: row.get(5)?,
                queue_order: row.get(6)?,
            },
            session_id: row.get(2)?,
        })
    })?;
    window.rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(window)
}

/// The prose of the rows: what a person typed, what the assistant said and what it thought.
fn build_chunks(rows: &[SourceRow]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    for row in rows {
        for entry in parse_message(&row.message, None) {
            let role = match entry.role {
                TranscriptRole::User => "user",
                TranscriptRole::Assistant => "assistant",
                TranscriptRole::Thinking => "thinking",
                TranscriptRole::Tool | TranscriptRole::System => continue,
            };
            let body = clip_units(js_trim(&entry.text), MAX_CHUNK_UNITS);
            if body.is_empty() {
                continue;
            }
            chunks.push(Chunk {
                body: body.to_owned(),
                session_id: row.session_id.clone(),
                src_rowid: row.message.rowid,
                role,
                at: entry.ts,
            });
        }
    }
    chunks
}

fn insert_chunks(tx: &Transaction<'_>, chunks: &[Chunk]) -> rusqlite::Result<()> {
    let mut insert = tx.prepare_cached(
        "INSERT INTO chunks(body, session_id, src_rowid, role, at) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for chunk in chunks {
        insert.execute(params![
            chunk.body,
            chunk.session_id,
            chunk.src_rowid,
            chunk.role,
            chunk.at
        ])?;
    }
    Ok(())
}

/// `String.prototype.trim`: white space and line terminators, and the byte order mark; not
/// U+0085, which Rust counts as white space and JavaScript does not.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// The longest prefix of `text` of at most `max` UTF-16 code units. A character that would
/// cross the limit is left out whole, so a surrogate pair is never split.
fn clip_units(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (at, c) in text.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &text[..at];
        }
    }
    text
}

/// Runs the indexer on a thread of its own until `stop` is set, with a 15-second idle interval.
pub fn spawn_indexer(
    index: Arc<SearchIndex>,
    source_path: PathBuf,
    is_running: impl Fn() -> bool + Send + 'static,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    spawn_indexer_with(
        index,
        source_path,
        is_running,
        stop,
        Duration::from_secs(15),
    )
}

/// The same with the idle interval given (tests use a short one). `spawn_indexer` calls it with
/// 15 seconds.
///
/// The thread reads Conductor's file through a `ConductorDb` of its own, opened while
/// Conductor runs and dropped when it stops: a backfill holds a connection for long stretches
/// and a shared handle's lock would stall every read of the phone behind it.
pub fn spawn_indexer_with(
    index: Arc<SearchIndex>,
    source_path: PathBuf,
    is_running: impl Fn() -> bool + Send + 'static,
    stop: Arc<AtomicBool>,
    idle: Duration,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut source: Option<ConductorDb> = None;
        // The version the source had before the last step of the last pass; `None` means a
        // pass is due (a fresh connection, or the last pass did not finish).
        let mut seen: Option<DataVersion> = None;
        while !stop.load(Ordering::Relaxed) {
            if !is_running() {
                source = None;
                seen = None;
                sleep_until_stopped(NOT_RUNNING_PAUSE, &stop);
                continue;
            }
            let db = source.get_or_insert_with(|| ConductorDb::new(&source_path));
            let due = seen.is_none() || db.data_version().ok() != seen;
            if due {
                seen = run_pass(&index, db, &stop);
            }
            sleep_until_stopped(idle, &stop);
        }
    })
}

/// Steps until the index has caught up. Returns the source's version from before the last
/// step, or `None` when the pass failed or was stopped.
fn run_pass(index: &SearchIndex, db: &ConductorDb, stop: &AtomicBool) -> Option<DataVersion> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        // Before the step: a commit after this read shows as a change next time.
        let version = db.data_version().ok();
        match index.index_step(db) {
            Ok(true) => sleep_until_stopped(BACKFILL_PAUSE, stop),
            Ok(false) => return version,
            Err(error) => {
                tracing::warn!(kind = %error.kind(), "search index step failed");
                return None;
            }
        }
    }
}

/// Sleeps `total`, waking early when `stop` is set.
fn sleep_until_stopped(total: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(SLEEP_SLICE));
    }
}
