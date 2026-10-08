//! The reads the writes need: delivery receipts, the live workspace of a write and the open
//! chats of a workspace.

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension};

use super::{ReadError, Reads};

/// Where a chat's transcript stood before a send: its newest row, and the outbox items queued then.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliveryCursor {
    pub rowid: i64,
    pub outbox_ids: std::collections::BTreeSet<String>,
}

/// Proof that Conductor took a prompt: an outbox item or a transcript row.
/// JSON: `{"kind":"outbox","id":…}` or `{"kind":"message","id":…,"rowid":…,"turnId":…}`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Receipt {
    Outbox {
        id: String,
    },
    Message {
        id: String,
        rowid: i64,
        #[serde(rename = "turnId")]
        turn_id: Option<String>,
    },
}

/// The facts of a live workspace a write needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteWorkspace {
    pub id: String,
    pub branch: Option<String>,
    pub repo_name: Option<String>,
    pub workspace_name: Option<String>,
    pub directory_name: Option<String>,
}

/// An open chat, in tab order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleSession {
    pub id: String,
    pub title: Option<String>,
    pub status: Option<String>,
}

/// Whether this database has the outbox table; older builds and rollbacks do not.
const OUTBOX_PROBE_SQL: &str = "\
SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_messages_outbox' LIMIT 1";

const CURSOR_ROWID_SQL: &str = "\
SELECT COALESCE(MAX(rowid), 0) FROM session_messages WHERE session_id = ?1";

const CURSOR_OUTBOX_SQL: &str = "\
SELECT message_id FROM session_messages_outbox WHERE session_id = ?1";

/// The user rows of a chat after the cursor, oldest first.
const DURABLE_SQL: &str = "\
SELECT rowid, id, content, turn_id
 FROM session_messages
 WHERE session_id = ?1 AND role = 'user' AND rowid > ?2
 ORDER BY rowid ASC";

/// The same without the `turn_id` column, which older builds lack.
const DURABLE_NO_TURN_SQL: &str = "\
SELECT rowid, id, content, NULL AS turn_id
 FROM session_messages
 WHERE session_id = ?1 AND role = 'user' AND rowid > ?2
 ORDER BY rowid ASC";

const OUTBOX_SQL: &str = "\
SELECT message_id, delivery_payload
 FROM session_messages_outbox
 WHERE session_id = ?1
 ORDER BY COALESCE(queue_order, 2147483647), created_at ASC";

/// The live workspace with this id.
const WORKSPACE_BY_ID_SQL: &str = "\
SELECT w.id, w.branch, r.name, w.workspace_name, w.directory_name
 FROM workspaces w
 LEFT JOIN repos r ON r.id = w.repository_id
 WHERE w.id = ?1 AND w.state IN ('ready', 'setting_up')
 LIMIT 1";

/// The live workspace of a chat.
const WORKSPACE_BY_SESSION_SQL: &str = "\
SELECT w.id, w.branch, r.name, w.workspace_name, w.directory_name
 FROM sessions s
 JOIN workspaces w ON w.id = s.workspace_id
 LEFT JOIN repos r ON r.id = w.repository_id
 WHERE s.id = ?1 AND w.state IN ('ready', 'setting_up')
 LIMIT 1";

/// The open chats of a workspace, in the order of `list_sessions`: the tab order.
const VISIBLE_SESSIONS_SQL: &str = "\
SELECT id, title, status
 FROM sessions
 WHERE workspace_id = ?1 AND COALESCE(is_hidden, 0) = 0
 ORDER BY created_at ASC";

/// A durable user row after the cursor.
struct DurableRow {
    rowid: i64,
    id: String,
    content: Option<String>,
    turn_id: Option<String>,
}

fn outbox_present(conn: &Connection) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(OUTBOX_PROBE_SQL, [], |_| Ok(()))
        .optional()?
        .is_some())
}

fn is_missing_column(error: &rusqlite::Error) -> bool {
    error.to_string().contains("no such column")
}

fn durable_rows(
    conn: &Connection,
    session_id: &str,
    after: i64,
) -> rusqlite::Result<Vec<DurableRow>> {
    let run = |sql: &str| -> rusqlite::Result<Vec<DurableRow>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params![session_id, after], |row| {
            Ok(DurableRow {
                rowid: row.get(0)?,
                id: row.get(1)?,
                content: row.get(2)?,
                turn_id: row.get(3)?,
            })
        })?;
        rows.collect()
    };
    match run(DURABLE_SQL) {
        Err(error) if is_missing_column(&error) => run(DURABLE_NO_TURN_SQL),
        other => other,
    }
}

fn durable_receipt(
    conn: &Connection,
    session_id: &str,
    target: &str,
    cursor: &DeliveryCursor,
) -> rusqlite::Result<Option<Receipt>> {
    let rows = durable_rows(conn, session_id, cursor.rowid)?;
    Ok(rows
        .into_iter()
        .find(|row| {
            row.content.as_deref().map(str::trim) == Some(target)
                && !cursor.outbox_ids.contains(&row.id)
        })
        .map(|row| Receipt::Message {
            id: row.id,
            rowid: row.rowid,
            turn_id: row.turn_id,
        }))
}

fn outbox_receipt(
    conn: &Connection,
    session_id: &str,
    target: &str,
    cursor: &DeliveryCursor,
) -> rusqlite::Result<Option<Receipt>> {
    let mut stmt = conn.prepare(OUTBOX_SQL)?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (id, payload) = row?;
        if cursor.outbox_ids.contains(&id) {
            continue;
        }
        let message = payload
            .and_then(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok())
            .and_then(|value| {
                value
                    .get("message")?
                    .as_str()
                    .map(|text| text.trim().to_owned())
            });
        if message.as_deref() == Some(target) {
            return Ok(Some(Receipt::Outbox { id }));
        }
    }
    Ok(None)
}

fn workspace_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WriteWorkspace> {
    Ok(WriteWorkspace {
        id: row.get(0)?,
        branch: row.get(1)?,
        repo_name: row.get(2)?,
        workspace_name: row.get(3)?,
        directory_name: row.get(4)?,
    })
}

impl Reads {
    /// Where the transcript of `session_id` stands now: its newest row and the outbox items
    /// queued at this moment. An unknown chat has rowid 0 and no items.
    pub fn delivery_cursor(&self, session_id: &str) -> Result<DeliveryCursor, ReadError> {
        Ok(self.db().read("receipts.cursor", |conn| {
            let rowid = conn.query_row(CURSOR_ROWID_SQL, [session_id], |row| row.get(0))?;
            let mut outbox_ids = BTreeSet::new();
            if outbox_present(conn)? {
                let mut stmt = conn.prepare(CURSOR_OUTBOX_SQL)?;
                let ids = stmt.query_map([session_id], |row| row.get::<_, String>(0))?;
                for id in ids {
                    outbox_ids.insert(id?);
                }
            }
            Ok(DeliveryCursor { rowid, outbox_ids })
        })?)
    }

    /// The proof that Conductor took `text` since `cursor`, if there is one: a durable user row
    /// first, then a new outbox item, then the durable rows once more, because an item promoted
    /// to a row between the two reads keeps its id. Matching is on the raw content, trimmed.
    pub fn delivery_receipt_since(
        &self,
        session_id: &str,
        text: &str,
        cursor: &DeliveryCursor,
    ) -> Result<Option<Receipt>, ReadError> {
        let target = text.trim();
        let first = self.db().read("receipts.rows", |conn| {
            durable_receipt(conn, session_id, target, cursor)
        })?;
        if first.is_some() {
            return Ok(first);
        }
        let queued = self.db().read("receipts.outbox", |conn| {
            if !outbox_present(conn)? {
                return Ok(None);
            }
            outbox_receipt(conn, session_id, target, cursor)
        })?;
        if queued.is_some() {
            return Ok(queued);
        }
        Ok(self.db().read("receipts.rows_again", |conn| {
            durable_receipt(conn, session_id, target, cursor)
        })?)
    }

    /// The live workspace a write targets: the one named by `workspace_id`, else the one of
    /// `session_id`; `None` when neither is ready or setting up.
    pub fn write_workspace(
        &self,
        workspace_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Option<WriteWorkspace>, ReadError> {
        let (label, sql, key) = match (workspace_id, session_id) {
            (Some(id), _) => ("receipts.workspace", WORKSPACE_BY_ID_SQL, id),
            (None, Some(id)) => ("receipts.session_workspace", WORKSPACE_BY_SESSION_SQL, id),
            (None, None) => return Ok(None),
        };
        Ok(self.db().read(label, |conn| {
            conn.query_row(sql, [key], workspace_row).optional()
        })?)
    }

    /// The open chats of a workspace, in tab order.
    pub fn visible_sessions(&self, workspace_id: &str) -> Result<Vec<VisibleSession>, ReadError> {
        Ok(self.db().read("receipts.visible_sessions", |conn| {
            let mut stmt = conn.prepare(VISIBLE_SESSIONS_SQL)?;
            let rows = stmt.query_map([workspace_id], |row| {
                Ok(VisibleSession {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    status: row.get(2)?,
                })
            })?;
            rows.collect()
        })?)
    }
}
