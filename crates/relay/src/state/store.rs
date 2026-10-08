//! The relay's own database: parked prompts, first prompts, chat links, push devices and a
//! key-value table.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};

/// The schema version this build writes and understands.
const SCHEMA_VERSION: i64 = 3;

const MIGRATION_1: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
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
  id TEXT PRIMARY KEY, endpoint TEXT NOT NULL UNIQUE, p256dh TEXT NOT NULL, auth TEXT NOT NULL,
  label TEXT NOT NULL, created_at_ms INTEGER NOT NULL, last_ok_at_ms INTEGER, last_error TEXT,
  failures INTEGER NOT NULL DEFAULT 0);
";

const MIGRATION_2: &str = "
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
";

const MIGRATION_3: &str = "
CREATE TABLE parked_agents (parked_id INTEGER PRIMARY KEY, agent TEXT NOT NULL);
CREATE TRIGGER parked_agents_cleanup AFTER DELETE ON parked_prompts
  BEGIN DELETE FROM parked_agents WHERE parked_id = OLD.id; END;
CREATE TABLE first_prompt_agents (workspace_id TEXT PRIMARY KEY, agent TEXT NOT NULL);
CREATE TRIGGER first_prompt_agents_cleanup AFTER DELETE ON first_prompts
  BEGIN DELETE FROM first_prompt_agents WHERE workspace_id = OLD.workspace_id; END;
";

const PARKED_COLUMNS: &str = "id, workspace_id, session_id, text, queue, status, attempts, \
     created_at_ms, reason, error, cursor_rowid, cursor_outbox";

const FIRST_PROMPT_COLUMNS: &str = "workspace_id, text, send_immediately, attachment_ids, status, \
     attempts, early_attempts, created_at_ms, last_attempt_at_ms, error";

const DEVICE_COLUMNS: &str = "id, endpoint, p256dh, auth, label, created_at_ms, last_ok_at_ms, \
     last_error, failures";

/// The relay's own database: `relay.db` in the state directory. One connection behind a mutex;
/// every call is a short statement, so callers on async tasks may call it directly.
pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("relay database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("relay database: {0}")]
    Io(#[from] io::Error),
    #[error("relay database: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkedStatus {
    Waiting,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewParked {
    pub workspace_id: String,
    pub session_id: String,
    /// Trimmed by the caller.
    pub text: String,
    pub queue: bool,
    pub created_at_ms: i64,
    pub reason: String,
    /// The delivery cursor taken before the first UI run: the transcript rowid and the outbox ids.
    pub cursor_rowid: Option<i64>,
    pub cursor_outbox: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedRow {
    pub id: i64,
    pub workspace_id: String,
    pub session_id: String,
    pub text: String,
    pub queue: bool,
    pub status: ParkedStatus,
    pub attempts: u32,
    pub created_at_ms: i64,
    pub reason: String,
    pub error: Option<String>,
    pub cursor_rowid: Option<i64>,
    pub cursor_outbox: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewFirstPrompt {
    pub workspace_id: String,
    pub text: String,
    pub send_immediately: bool,
    pub attachment_ids: Vec<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstPromptRow {
    pub workspace_id: String,
    pub text: String,
    pub send_immediately: bool,
    pub attachment_ids: Vec<String>,
    pub status: ParkedStatus,
    pub attempts: u32,
    pub early_attempts: u32,
    pub created_at_ms: i64,
    pub last_attempt_at_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatLinkRow {
    pub session_id: String,
    pub workspace_id: String,
    pub previous_session_id: String,
    pub title: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDevice {
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub label: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRow {
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub label: String,
    pub created_at_ms: i64,
    pub last_ok_at_ms: Option<i64>,
    pub last_error: Option<String>,
    pub failures: u32,
}

/// A `parked_prompts` row as SQLite holds it, before the JSON column is decoded.
struct RawParked {
    id: i64,
    workspace_id: String,
    session_id: String,
    text: String,
    queue: bool,
    status: String,
    attempts: u32,
    created_at_ms: i64,
    reason: String,
    error: Option<String>,
    cursor_rowid: Option<i64>,
    cursor_outbox: String,
}

impl RawParked {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<RawParked> {
        Ok(RawParked {
            id: row.get(0)?,
            workspace_id: row.get(1)?,
            session_id: row.get(2)?,
            text: row.get(3)?,
            queue: row.get(4)?,
            status: row.get(5)?,
            attempts: row.get(6)?,
            created_at_ms: row.get(7)?,
            reason: row.get(8)?,
            error: row.get(9)?,
            cursor_rowid: row.get(10)?,
            cursor_outbox: row.get(11)?,
        })
    }

    fn decode(self) -> Result<ParkedRow, StoreError> {
        let status = match self.status.as_str() {
            "waiting" => ParkedStatus::Waiting,
            "failed" => ParkedStatus::Failed,
            other => {
                return Err(StoreError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown parked status {other:?}"),
                )))
            }
        };
        Ok(ParkedRow {
            id: self.id,
            workspace_id: self.workspace_id,
            session_id: self.session_id,
            text: self.text,
            queue: self.queue,
            status,
            attempts: self.attempts,
            created_at_ms: self.created_at_ms,
            reason: self.reason,
            error: self.error,
            cursor_rowid: self.cursor_rowid,
            cursor_outbox: serde_json::from_str(&self.cursor_outbox)?,
        })
    }
}

/// A `first_prompts` row as SQLite holds it, before the JSON and status columns are decoded.
struct RawFirstPrompt {
    workspace_id: String,
    text: String,
    send_immediately: bool,
    attachment_ids: String,
    status: String,
    attempts: u32,
    early_attempts: u32,
    created_at_ms: i64,
    last_attempt_at_ms: Option<i64>,
    error: Option<String>,
}

impl RawFirstPrompt {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<RawFirstPrompt> {
        Ok(RawFirstPrompt {
            workspace_id: row.get(0)?,
            text: row.get(1)?,
            send_immediately: row.get(2)?,
            attachment_ids: row.get(3)?,
            status: row.get(4)?,
            attempts: row.get(5)?,
            early_attempts: row.get(6)?,
            created_at_ms: row.get(7)?,
            last_attempt_at_ms: row.get(8)?,
            error: row.get(9)?,
        })
    }

    fn decode(self) -> Result<FirstPromptRow, StoreError> {
        let status = match self.status.as_str() {
            "waiting" => ParkedStatus::Waiting,
            "failed" => ParkedStatus::Failed,
            other => {
                return Err(StoreError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown first prompt status {other:?}"),
                )))
            }
        };
        Ok(FirstPromptRow {
            workspace_id: self.workspace_id,
            text: self.text,
            send_immediately: self.send_immediately,
            attachment_ids: serde_json::from_str(&self.attachment_ids)?,
            status,
            attempts: self.attempts,
            early_attempts: self.early_attempts,
            created_at_ms: self.created_at_ms,
            last_attempt_at_ms: self.last_attempt_at_ms,
            error: self.error,
        })
    }
}

fn chat_link_from_row(row: &Row<'_>) -> rusqlite::Result<ChatLinkRow> {
    Ok(ChatLinkRow {
        session_id: row.get(0)?,
        workspace_id: row.get(1)?,
        previous_session_id: row.get(2)?,
        title: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn device_from_row(row: &Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        id: row.get(0)?,
        endpoint: row.get(1)?,
        p256dh: row.get(2)?,
        auth: row.get(3)?,
        label: row.get(4)?,
        created_at_ms: row.get(5)?,
        last_ok_at_ms: row.get(6)?,
        last_error: row.get(7)?,
        failures: row.get(8)?,
    })
}

impl Store {
    /// Creates the file with mode 0600 when missing (its directory must exist), then opens it with
    /// `journal_mode = WAL`, `synchronous = NORMAL`, `busy_timeout = 2000` and migrates it.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        // `create` without `truncate`: an existing file is left as it is, mode included.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(2000))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        Store::migrated(conn)
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(std::time::Duration::from_millis(2000))?;
        Store::migrated(conn)
    }

    fn migrated(mut conn: Connection) -> Result<Store, StoreError> {
        // Immediate, and the version read inside it: two relays opening one file agree on who
        // migrates.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "relay database is from a newer version",
            )));
        }
        if version < 1 {
            tx.execute_batch(MIGRATION_1)?;
            tx.execute_batch("PRAGMA user_version = 1")?;
        }
        if version < 2 {
            tx.execute_batch(MIGRATION_2)?;
            tx.execute_batch("PRAGMA user_version = 2")?;
        }
        if version < 3 {
            tx.execute_batch(MIGRATION_3)?;
            tx.execute_batch("PRAGMA user_version = 3")?;
        }
        tx.commit()?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic in another caller leaves no half-written statement behind: SQLite rolled it back.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Inserts, or — for the same chat and text — resets the existing row to `waiting` with
    /// 0 attempts and no error, and replaces `workspace_id`, `queue` and the cursor; its id,
    /// `created_at_ms` and `reason` stay.
    pub fn park(&self, entry: &NewParked) -> Result<ParkedRow, StoreError> {
        let outbox = serde_json::to_string(&entry.cursor_outbox)?;
        let sql = format!(
            "INSERT INTO parked_prompts
               (workspace_id, session_id, text, queue, status, attempts, created_at_ms, reason,
                error, cursor_rowid, cursor_outbox)
             VALUES (?1, ?2, ?3, ?4, 'waiting', 0, ?5, ?6, NULL, ?7, ?8)
             ON CONFLICT (session_id, text) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               queue = excluded.queue,
               status = 'waiting',
               attempts = 0,
               error = NULL,
               cursor_rowid = excluded.cursor_rowid,
               cursor_outbox = excluded.cursor_outbox
             RETURNING {PARKED_COLUMNS}"
        );
        let raw = self.conn().query_row(
            &sql,
            params![
                entry.workspace_id,
                entry.session_id,
                entry.text,
                entry.queue,
                entry.created_at_ms,
                entry.reason,
                entry.cursor_rowid,
                outbox,
            ],
            RawParked::from_row,
        )?;
        raw.decode()
    }

    /// Every row, oldest id first.
    pub fn parked(&self) -> Result<Vec<ParkedRow>, StoreError> {
        let raws = {
            let conn = self.conn();
            let mut stmt = conn.prepare(&format!(
                "SELECT {PARKED_COLUMNS} FROM parked_prompts ORDER BY id"
            ))?;
            let rows = stmt.query_map([], RawParked::from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        raws.into_iter().map(RawParked::decode).collect()
    }

    /// Adds one attempt; at `max_attempts` the row becomes `failed` with `error`. `None` when the
    /// row is gone.
    pub fn record_parked_failure(
        &self,
        id: i64,
        error: &str,
        max_attempts: u32,
    ) -> Result<Option<ParkedRow>, StoreError> {
        let sql = format!(
            "UPDATE parked_prompts SET
               attempts = attempts + 1,
               status = CASE WHEN attempts + 1 >= ?2 THEN 'failed' ELSE status END,
               error = CASE WHEN attempts + 1 >= ?2 THEN ?3 ELSE error END
             WHERE id = ?1
             RETURNING {PARKED_COLUMNS}"
        );
        let raw = self
            .conn()
            .query_row(&sql, params![id, max_attempts, error], RawParked::from_row)
            .optional()?;
        raw.map(RawParked::decode).transpose()
    }

    pub fn remove_parked(&self, id: i64) -> Result<bool, StoreError> {
        let n = self
            .conn()
            .execute("DELETE FROM parked_prompts WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    pub fn forget_parked_session(&self, session_id: &str) -> Result<usize, StoreError> {
        Ok(self.conn().execute(
            "DELETE FROM parked_prompts WHERE session_id = ?1",
            [session_id],
        )?)
    }

    pub fn forget_parked_text(&self, session_id: &str, text: &str) -> Result<usize, StoreError> {
        Ok(self.conn().execute(
            "DELETE FROM parked_prompts WHERE session_id = ?1 AND text = ?2",
            [session_id, text],
        )?)
    }

    /// Removes rows created before `created_before_ms`.
    pub fn prune_parked(&self, created_before_ms: i64) -> Result<usize, StoreError> {
        Ok(self.conn().execute(
            "DELETE FROM parked_prompts WHERE created_at_ms < ?1",
            [created_before_ms],
        )?)
    }

    /// Sets (`Some`) or clears (`None`) the agent settings kept with a parked prompt.
    pub fn set_parked_agent(&self, parked_id: i64, agent: Option<&str>) -> Result<(), StoreError> {
        let conn = self.conn();
        match agent {
            Some(agent) => conn.execute(
                "INSERT INTO parked_agents (parked_id, agent) VALUES (?1, ?2)
                 ON CONFLICT (parked_id) DO UPDATE SET agent = excluded.agent",
                params![parked_id, agent],
            )?,
            None => conn.execute(
                "DELETE FROM parked_agents WHERE parked_id = ?1",
                [parked_id],
            )?,
        };
        Ok(())
    }

    pub fn parked_agent(&self, parked_id: i64) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT agent FROM parked_agents WHERE parked_id = ?1",
                [parked_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Inserts, or replaces the whole entry of the workspace: `waiting`, no attempts, no error.
    pub fn upsert_first_prompt(
        &self,
        entry: &NewFirstPrompt,
    ) -> Result<FirstPromptRow, StoreError> {
        let attachments = serde_json::to_string(&entry.attachment_ids)?;
        let sql = format!(
            "INSERT INTO first_prompts
               (workspace_id, text, send_immediately, attachment_ids, status, attempts,
                early_attempts, created_at_ms, last_attempt_at_ms, error)
             VALUES (?1, ?2, ?3, ?4, 'waiting', 0, 0, ?5, NULL, NULL)
             ON CONFLICT (workspace_id) DO UPDATE SET
               text = excluded.text,
               send_immediately = excluded.send_immediately,
               attachment_ids = excluded.attachment_ids,
               status = 'waiting',
               attempts = 0,
               early_attempts = 0,
               created_at_ms = excluded.created_at_ms,
               last_attempt_at_ms = NULL,
               error = NULL
             RETURNING {FIRST_PROMPT_COLUMNS}"
        );
        let raw = self.conn().query_row(
            &sql,
            params![
                entry.workspace_id,
                entry.text,
                entry.send_immediately,
                attachments,
                entry.created_at_ms,
            ],
            RawFirstPrompt::from_row,
        )?;
        raw.decode()
    }

    /// Every entry, oldest first (`created_at_ms`, then `workspace_id`).
    pub fn first_prompts(&self) -> Result<Vec<FirstPromptRow>, StoreError> {
        let raws = {
            let conn = self.conn();
            let mut stmt = conn.prepare(&format!(
                "SELECT {FIRST_PROMPT_COLUMNS} FROM first_prompts
                 ORDER BY created_at_ms, workspace_id"
            ))?;
            let rows = stmt.query_map([], RawFirstPrompt::from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        raws.into_iter().map(RawFirstPrompt::decode).collect()
    }

    pub fn first_prompt(&self, workspace_id: &str) -> Result<Option<FirstPromptRow>, StoreError> {
        let raw = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {FIRST_PROMPT_COLUMNS} FROM first_prompts WHERE workspace_id = ?1"
                ),
                [workspace_id],
                RawFirstPrompt::from_row,
            )
            .optional()?;
        raw.map(RawFirstPrompt::decode).transpose()
    }

    /// Adds one to `early_attempts` (`early`) or `attempts`, and sets `last_attempt_at_ms`. `None`
    /// when the entry is gone.
    pub fn record_first_prompt_attempt(
        &self,
        workspace_id: &str,
        early: bool,
        at_ms: i64,
    ) -> Result<Option<FirstPromptRow>, StoreError> {
        let sql = format!(
            "UPDATE first_prompts SET
               early_attempts = early_attempts + ?2,
               attempts = attempts + ?3,
               last_attempt_at_ms = ?4
             WHERE workspace_id = ?1
             RETURNING {FIRST_PROMPT_COLUMNS}"
        );
        let raw = self
            .conn()
            .query_row(
                &sql,
                params![workspace_id, early as i64, !early as i64, at_ms],
                RawFirstPrompt::from_row,
            )
            .optional()?;
        raw.map(RawFirstPrompt::decode).transpose()
    }

    /// Marks the entry `failed` with `error`. `None` when the entry is gone.
    pub fn fail_first_prompt(
        &self,
        workspace_id: &str,
        error: &str,
    ) -> Result<Option<FirstPromptRow>, StoreError> {
        let sql = format!(
            "UPDATE first_prompts SET status = 'failed', error = ?2
             WHERE workspace_id = ?1
             RETURNING {FIRST_PROMPT_COLUMNS}"
        );
        let raw = self
            .conn()
            .query_row(&sql, params![workspace_id, error], RawFirstPrompt::from_row)
            .optional()?;
        raw.map(RawFirstPrompt::decode).transpose()
    }

    /// Sets `attachment_ids` to the empty list. `None` when the entry is gone.
    pub fn clear_first_prompt_attachments(
        &self,
        workspace_id: &str,
    ) -> Result<Option<FirstPromptRow>, StoreError> {
        let sql = format!(
            "UPDATE first_prompts SET attachment_ids = '[]'
             WHERE workspace_id = ?1
             RETURNING {FIRST_PROMPT_COLUMNS}"
        );
        let raw = self
            .conn()
            .query_row(&sql, [workspace_id], RawFirstPrompt::from_row)
            .optional()?;
        raw.map(RawFirstPrompt::decode).transpose()
    }

    pub fn remove_first_prompt(&self, workspace_id: &str) -> Result<bool, StoreError> {
        let n = self.conn().execute(
            "DELETE FROM first_prompts WHERE workspace_id = ?1",
            [workspace_id],
        )?;
        Ok(n > 0)
    }

    /// Removes entries created before `created_before_ms`.
    pub fn prune_first_prompts(&self, created_before_ms: i64) -> Result<usize, StoreError> {
        Ok(self.conn().execute(
            "DELETE FROM first_prompts WHERE created_at_ms < ?1",
            [created_before_ms],
        )?)
    }

    /// Sets (`Some`) or clears (`None`) the agent settings kept for a workspace's first prompt.
    pub fn set_first_prompt_agent(
        &self,
        workspace_id: &str,
        agent: Option<&str>,
    ) -> Result<(), StoreError> {
        let conn = self.conn();
        match agent {
            Some(agent) => conn.execute(
                "INSERT INTO first_prompt_agents (workspace_id, agent) VALUES (?1, ?2)
                 ON CONFLICT (workspace_id) DO UPDATE SET agent = excluded.agent",
                [workspace_id, agent],
            )?,
            None => conn.execute(
                "DELETE FROM first_prompt_agents WHERE workspace_id = ?1",
                [workspace_id],
            )?,
        };
        Ok(())
    }

    pub fn first_prompt_agent(&self, workspace_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT agent FROM first_prompt_agents WHERE workspace_id = ?1",
                [workspace_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Links `session_id` as the continuation of `previous_session_id` in one immediate
    /// transaction. The outer error is the database; the inner one is a refused link, with the
    /// text the phone shows. `title` and `created_at` are the previous chat's own; the previous
    /// chat's link, when it has one, takes precedence so a whole conversation keeps one title and
    /// one position among the tabs.
    pub fn join_chats(
        &self,
        session_id: &str,
        workspace_id: &str,
        previous_session_id: &str,
        title: &str,
        created_at: &str,
    ) -> Result<Result<(), String>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let link_of = |id: &str| -> rusqlite::Result<Option<ChatLinkRow>> {
            tx.query_row(
                "SELECT session_id, workspace_id, previous_session_id, title, created_at
                 FROM chat_links WHERE session_id = ?1",
                [id],
                chat_link_from_row,
            )
            .optional()
        };

        if let Some(existing) = link_of(session_id)? {
            if existing.workspace_id == workspace_id
                && existing.previous_session_id == previous_session_id
            {
                return Ok(Ok(()));
            }
            return Ok(Err("This chat already belongs to a conversation".into()));
        }
        let continued: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM chat_links WHERE previous_session_id = ?1)",
            [previous_session_id],
            |row| row.get(0),
        )?;
        if continued {
            return Ok(Err(
                "This chat already continues in another tab. Refresh to open the latest conversation."
                    .into(),
            ));
        }

        let mut visited = std::collections::HashSet::from([session_id.to_owned()]);
        let mut cursor = Some(previous_session_id.to_owned());
        while let Some(id) = cursor {
            if visited.contains(&id) {
                return Ok(Err("Chat history cannot contain a cycle".into()));
            }
            let link = link_of(&id)?;
            if let Some(link) = &link {
                if link.workspace_id != workspace_id {
                    return Ok(Err("Chats must share a workspace".into()));
                }
            }
            visited.insert(id);
            cursor = link.map(|link| link.previous_session_id);
        }

        let (title, created_at) = match link_of(previous_session_id)? {
            Some(previous) => (previous.title, previous.created_at),
            None => (title.to_owned(), created_at.to_owned()),
        };
        tx.execute(
            "INSERT INTO chat_links (session_id, workspace_id, previous_session_id, title, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, workspace_id, previous_session_id, title, created_at],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// The links of a workspace, in the order they were made.
    pub fn chat_links(&self, workspace_id: &str) -> Result<Vec<ChatLinkRow>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT session_id, workspace_id, previous_session_id, title, created_at
             FROM chat_links WHERE workspace_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt.query_map([workspace_id], chat_link_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Inserts, or — for the same endpoint — replaces the keys, the label when the new one is not
    /// empty, and clears `failures` and `last_error`.
    pub fn upsert_device(&self, device: &NewDevice) -> Result<DeviceRow, StoreError> {
        let sql = format!(
            "INSERT INTO push_devices (id, endpoint, p256dh, auth, label, created_at_ms, failures)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
             ON CONFLICT (endpoint) DO UPDATE SET
               p256dh = excluded.p256dh,
               auth = excluded.auth,
               label = CASE WHEN excluded.label <> '' THEN excluded.label ELSE push_devices.label END,
               failures = 0,
               last_error = NULL
             RETURNING {DEVICE_COLUMNS}"
        );
        Ok(self.conn().query_row(
            &sql,
            params![
                device.id,
                device.endpoint,
                device.p256dh,
                device.auth,
                device.label,
                device.created_at_ms,
            ],
            device_from_row,
        )?)
    }

    /// Oldest first (`created_at_ms`, then `id`).
    pub fn devices(&self) -> Result<Vec<DeviceRow>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DEVICE_COLUMNS} FROM push_devices ORDER BY created_at_ms, id"
        ))?;
        let rows = stmt.query_map([], device_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn device(&self, id: &str) -> Result<Option<DeviceRow>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {DEVICE_COLUMNS} FROM push_devices WHERE id = ?1"),
                [id],
                device_from_row,
            )
            .optional()?)
    }

    pub fn remove_device(&self, id: &str) -> Result<bool, StoreError> {
        let n = self
            .conn()
            .execute("DELETE FROM push_devices WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    pub fn remove_device_by_endpoint(&self, endpoint: &str) -> Result<bool, StoreError> {
        let n = self
            .conn()
            .execute("DELETE FROM push_devices WHERE endpoint = ?1", [endpoint])?;
        Ok(n > 0)
    }

    /// `last_ok_at_ms = at_ms`, `failures = 0`, `last_error = NULL`.
    pub fn record_device_ok(&self, id: &str, at_ms: i64) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE push_devices SET last_ok_at_ms = ?2, failures = 0, last_error = NULL
             WHERE id = ?1",
            params![id, at_ms],
        )?;
        Ok(())
    }

    /// `failures + 1` and `last_error`; returns the new count, 0 when the device is gone.
    pub fn record_device_failure(&self, id: &str, error: &str) -> Result<u32, StoreError> {
        let failures = self
            .conn()
            .query_row(
                "UPDATE push_devices SET failures = failures + 1, last_error = ?2
                 WHERE id = ?1 RETURNING failures",
                params![id, error],
                |row| row.get(0),
            )
            .optional()?;
        Ok(failures.unwrap_or(0))
    }
}
