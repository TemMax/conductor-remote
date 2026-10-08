//! Session reads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use rusqlite::Row;
use serde::Serialize;

use super::background::{BackgroundFold, TaskFrameRow};
use super::{ReadError, Reads};

/// How many chats keep their fold of background-task frames; past this the table starts over.
const MAX_FOLDED_CHATS: usize = 64;

/// A background task a chat is still waiting on (the web app's `BackgroundTask`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTask {
    pub task_id: String,
    pub tool_use_id: Option<String>,
    pub description: String,
    pub task_type: String,
    pub since: String,
}

/// One open chat of a workspace: the web app's `SessionRow` without `auto_model`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionRow {
    pub id: String,
    pub status: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// The chat's current effort: Codex chats keep it in `codex_thinking_level`, the others in
    /// `claude_effort_level`; `ultra` is served as `ultracode`.
    pub claude_effort_level: Option<String>,
    pub fast_mode: Option<i64>,
    pub agent_type: Option<String>,
    pub context_used_percent: Option<f64>,
    pub unread_count: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub last_user_message_at: Option<String>,
    pub prompt_cache_ttl_ms: Option<i64>,
    pub turn_started_at: Option<String>,
    /// The tasks the chat's live agent process is still waiting on; empty without extras and for
    /// a chat without a live process.
    pub background_tasks: Vec<BackgroundTask>,
}

/// One closed chat of a workspace: the web app's `ClosedSession`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClosedSession {
    pub id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub agent_type: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Open chats of a workspace. The turn start is the earliest dispatch of the latest user turn
/// (grouped by `turn_id`), else the latest dispatch of a legacy queued message (`queue_order`,
/// no longer written). The prompt-cache lifetime is read from the newest Claude message that
/// reports a cache write; mixed-TTL writes exist, so the shorter conversation tail wins.
const OPEN_SESSIONS_SQL: &str = "\
SELECT s.id, s.status, s.title, s.model, s.permission_mode,
       s.claude_effort_level, s.codex_thinking_level, s.fast_mode, s.agent_type,
       s.context_used_percent, s.unread_count,
       s.created_at, s.updated_at, s.last_user_message_at,
       (SELECT CASE
                 -- Mixed-TTL writes exist, so the shorter conversation tail wins.
                 WHEN m.content GLOB '*\"ephemeral_5m_input_tokens\":[1-9]*' THEN 300000
                 WHEN m.content GLOB '*\"ephemeral_1h_input_tokens\":[1-9]*' THEN 3600000
               END
          FROM session_messages m
         WHERE m.session_id = s.id
           AND s.agent_type IN ('claude', 'anthropic')
           AND (m.content GLOB '*\"ephemeral_5m_input_tokens\":[1-9]*'
                OR m.content GLOB '*\"ephemeral_1h_input_tokens\":[1-9]*')
         ORDER BY m.rowid DESC LIMIT 1) AS prompt_cache_ttl_ms,
       COALESCE(
         (SELECT MIN(CASE WHEN head.sent_at IS NOT NULL THEN head.sent_at END)
            FROM session_messages head
           WHERE head.session_id = s.id
             AND head.role = 'user'
             AND head.turn_id = (
               SELECT latest.turn_id
                 FROM session_messages latest
                WHERE latest.session_id = s.id
                  AND latest.role = 'user'
                  AND latest.turn_id IS NOT NULL
                  AND latest.sent_at IS NOT NULL
                ORDER BY latest.sent_at DESC, latest.rowid DESC
                LIMIT 1
             )),
         (SELECT MAX(legacy.sent_at)
            FROM session_messages legacy
           WHERE legacy.session_id = s.id
             AND legacy.queue_order IS NOT NULL
             AND legacy.sent_at IS NOT NULL)
       ) AS turn_started_at
FROM sessions s
WHERE s.workspace_id = ? AND COALESCE(s.is_hidden, 0) = 0
ORDER BY s.created_at ASC";

/// The rows that open and close background tasks.
macro_rules! task_frame_condition {
    () => {
        "(content LIKE '{\"type\":\"system\",\"subtype\":\"task_started\"%'
   OR content LIKE '{\"type\":\"system\",\"subtype\":\"task_notification\"%')"
    };
}

/// Query F: the frames that open and close background tasks of a chat after a `rowid` and up to
/// another, oldest first.
const TASK_FRAMES_SQL: &str = concat!(
    "SELECT created_at, content FROM session_messages
WHERE session_id = ? AND ",
    task_frame_condition!(),
    "
  AND rowid > ? AND rowid <= ?
ORDER BY rowid ASC"
);

/// How many rows, of any kind, a chat has up to a `rowid`. The `session_id` index serves it
/// without reading the text of the rows.
const ROWS_UP_TO_SQL: &str =
    "SELECT COUNT(*) FROM session_messages WHERE session_id = ? AND rowid <= ?";

/// The newest `rowid` of a chat and how many rows it has.
const NEWEST_ROW_SQL: &str =
    "SELECT MAX(rowid), COUNT(*) FROM session_messages WHERE session_id = ?";

/// The `id` of the row with a `rowid`.
const ROW_ID_SQL: &str = "SELECT id FROM session_messages WHERE rowid = ?";

/// Closed chats: strictly `is_hidden = 1`, so a NULL `is_hidden` is an open chat. The two
/// timestamp formats in `updated_at` (`YYYY-MM-DD HH:MM:SS` and ISO with `T` and `Z`) are made
/// comparable for the sort only; the values are served as stored.
const CLOSED_SESSIONS_SQL: &str = "\
SELECT id, title, model, agent_type, created_at, updated_at
FROM sessions
WHERE workspace_id = ? AND is_hidden = 1
ORDER BY REPLACE(REPLACE(updated_at, 'T', ' '), 'Z', '') DESC, created_at DESC, id ASC";

impl Reads {
    /// The open chats of a workspace, in tab order.
    pub fn list_sessions(&self, workspace_id: &str) -> Result<Vec<SessionRow>, ReadError> {
        let rows = self.db().read("list_sessions", |conn| {
            let mut stmt = conn.prepare(OPEN_SESSIONS_SQL)?;
            let rows = stmt
                .query_map([workspace_id], session_row)?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })?;
        let Some(extras) = self.extras() else {
            return Ok(rows);
        };
        // The process list never blocks: the first call has none and queues the listing.
        let mut rows = rows;
        for row in &mut rows {
            let Some(started_at) = extras.processes.agent_started_at(&row.id) else {
                continue;
            };
            row.background_tasks = self.background_tasks(&row.id, started_at)?;
        }
        Ok(rows)
    }

    /// The tasks the live agent process of a chat (started at `started_at`) waits on. The fold of
    /// the chat's task frames is kept between calls, so a call reads only the frames written
    /// since the last one; see `Carried` for when it starts over.
    fn background_tasks(
        &self,
        session_id: &str,
        started_at: i64,
    ) -> Result<Vec<BackgroundTask>, ReadError> {
        let key = (self.db().path().to_path_buf(), session_id.to_owned());
        // Taken out of the table while it is used: a second build of the chat meanwhile starts
        // over from the first frame, and whichever finishes last is kept.
        let kept = folds().remove(&key);
        let (mut carried, frames) = self.db().read("list task frames", |conn| {
            let mut carried = match kept {
                Some(kept) if kept.process_started_at_ms == started_at => {
                    let rows: i64 = conn.query_row(
                        ROWS_UP_TO_SQL,
                        rusqlite::params![session_id, kept.read_up_to],
                        |r| r.get(0),
                    )?;
                    // New rows always take a `rowid` above the newest one, so the rows up to the
                    // newest one the fold has read are the same ones when they are as many and the
                    // newest is still the same row.
                    let newest = row_id_at(conn, kept.read_up_to)?;
                    (u64::try_from(rows) == Ok(kept.rows) && newest == kept.newest_id)
                        .then_some(kept)
                }
                _ => None,
            }
            .unwrap_or_else(|| Carried::new(started_at));
            // Taken before the frames are read, which stop at it, so the rows counted are the
            // rows read.
            let (read_up_to, rows): (Option<i64>, i64) =
                conn.query_row(NEWEST_ROW_SQL, [session_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let read_up_to = read_up_to.unwrap_or(0);
            let mut stmt = conn.prepare(TASK_FRAMES_SQL)?;
            let frames = stmt
                .query_map(
                    rusqlite::params![session_id, carried.read_up_to, read_up_to],
                    |r| {
                        Ok(TaskFrameRow {
                            created_at: r.get(0)?,
                            content: r.get(1)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            carried.read_up_to = read_up_to;
            carried.rows = u64::try_from(rows).unwrap_or(0);
            carried.newest_id = row_id_at(conn, read_up_to)?;
            Ok((carried, frames))
        })?;
        for frame in &frames {
            carried.fold.push(frame);
        }
        count_frames_read(&key, frames.len() as u64);
        let tasks = carried.fold.tasks();
        let mut table = folds();
        if table.len() >= MAX_FOLDED_CHATS && !table.contains_key(&key) {
            table.clear();
        }
        table.insert(key, carried);
        Ok(tasks)
    }

    /// The closed chats of a workspace, most recent first.
    pub fn list_closed_sessions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ClosedSession>, ReadError> {
        let rows = self.db().read("list_closed_sessions", |conn| {
            let mut stmt = conn.prepare(CLOSED_SESSIONS_SQL)?;
            let rows = stmt
                .query_map([workspace_id], |row| {
                    Ok(ClosedSession {
                        id: row.get("id")?,
                        title: row.get("title")?,
                        model: row.get("model")?,
                        agent_type: row.get("agent_type")?,
                        created_at: row.get("created_at")?,
                        updated_at: row.get("updated_at")?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })?;
        Ok(rows)
    }
}

type FoldKey = (PathBuf, String);

/// What a chat keeps between two builds of the chat list: the fold of its task frames, the
/// process start it was made for, the `rowid` of its newest row at the last read (`read_up_to`,
/// where the next read starts: every row up to it has been looked at) with how many rows of any
/// kind there were up to it and the `id` of that row. It is thrown away, and the fold starts from the first frame, when the agent
/// process has another start (the agent was restarted) or when the rows up to `read_up_to` are no
/// longer those counted (rows were deleted, and their `rowid`s may be reused). The table of chats
/// starts over when it is full.
struct Carried {
    fold: BackgroundFold,
    process_started_at_ms: i64,
    read_up_to: i64,
    rows: u64,
    newest_id: Option<rusqlite::types::Value>,
}

impl Carried {
    fn new(process_started_at_ms: i64) -> Self {
        Self {
            fold: BackgroundFold::new(process_started_at_ms),
            process_started_at_ms,
            read_up_to: 0,
            rows: 0,
            newest_id: None,
        }
    }
}

/// The `id` of the row with a `rowid`, if there is one.
fn row_id_at(
    conn: &rusqlite::Connection,
    rowid: i64,
) -> rusqlite::Result<Option<rusqlite::types::Value>> {
    use rusqlite::OptionalExtension;
    conn.query_row(ROW_ID_SQL, [rowid], |r| r.get(0)).optional()
}

/// The process-wide table of folds, never held across a database read.
fn folds() -> MutexGuard<'static, HashMap<FoldKey, Carried>> {
    static TABLE: OnceLock<Mutex<HashMap<FoldKey, Carried>>> = OnceLock::new();
    TABLE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn frames_read() -> MutexGuard<'static, HashMap<FoldKey, u64>> {
    static COUNTS: OnceLock<Mutex<HashMap<FoldKey, u64>>> = OnceLock::new();
    COUNTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn count_frames_read(key: &FoldKey, frames: u64) {
    *frames_read().entry(key.clone()).or_insert(0) += frames;
}

/// How many task frames the chat-list builds have read for the chat since the process started.
/// For tests.
#[doc(hidden)]
pub fn task_frames_read(db_path: &Path, session_id: &str) -> u64 {
    frames_read()
        .get(&(db_path.to_path_buf(), session_id.to_owned()))
        .copied()
        .unwrap_or(0)
}

fn session_row(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    let agent_type: Option<String> = row.get("agent_type")?;
    let claude_effort: Option<String> = row.get("claude_effort_level")?;
    let codex_effort: Option<String> = row.get("codex_thinking_level")?;
    let effort = if agent_type.as_deref() == Some("codex") {
        codex_effort
    } else {
        claude_effort
    };
    Ok(SessionRow {
        id: row.get("id")?,
        status: row.get("status")?,
        title: row.get("title")?,
        model: row.get("model")?,
        permission_mode: row.get("permission_mode")?,
        claude_effort_level: effort.map(|e| {
            if e == "ultra" {
                "ultracode".to_owned()
            } else {
                e
            }
        }),
        fast_mode: row.get("fast_mode")?,
        agent_type,
        context_used_percent: row.get("context_used_percent")?,
        unread_count: row.get("unread_count")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_user_message_at: row.get("last_user_message_at")?,
        prompt_cache_ttl_ms: row.get("prompt_cache_ttl_ms")?,
        turn_started_at: row.get("turn_started_at")?,
        background_tasks: Vec::new(),
    })
}
