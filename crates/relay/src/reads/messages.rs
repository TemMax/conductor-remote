//! Transcript message reads.

use rusqlite::OptionalExtension;
use serde::Serialize;

use super::workspaces::resolve_worktree;
use super::{ReadError, Reads};
use crate::transcript::{
    parse_message, parse_outbox_message, StoredMessage, StoredOutboxMessage, TranscriptEntry,
};

/// What the phone polls for: the TypeScript `MessagesResponse`. Field order is the order of
/// the keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessagesResponse {
    /// The entries of the rows after the requested cursor, in row order.
    pub entries: Vec<TranscriptEntry>,
    /// The `rowid` of the last row read, also when it produced no entry; the requested cursor
    /// when no row came back.
    pub cursor: i64,
    /// The whole queue-mode outbox of the chat, whatever the cursor.
    pub queued: Vec<TranscriptEntry>,
    #[serde(rename = "pendingQuestion", skip_serializing_if = "Option::is_none")]
    pub pending_question: Option<crate::transcript::questions::QuestionRequest>,
}

/// How many rows of a chat one `read` fetches. A chat of tens of thousands of rows is read in
/// batches so that only one batch of raw rows is in memory at a time and the database lock is
/// released between batches.
const MESSAGE_BATCH_ROWS: usize = 500;

/// One batch of the durable rows of a chat after a cursor. `rowid` is the cursor.
const MESSAGES_SQL: &str = "\
SELECT rowid, id, content, created_at, sent_at, queue_order
 FROM session_messages
 WHERE session_id = ?1 AND rowid > ?2
 ORDER BY rowid ASC
 LIMIT ?3";

/// Whether this database has the outbox table; older builds and rollbacks do not.
const OUTBOX_PROBE_SQL: &str = "\
SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_messages_outbox' LIMIT 1";

/// The prompts accepted but not yet dispatched, in the order they will be sent: rows without
/// a `queue_order` come last.
const QUEUE_SQL: &str = "\
SELECT message_id, delivery_payload, created_at
 FROM session_messages_outbox
 WHERE session_id = ?1 AND mode = 'queue'
 ORDER BY COALESCE(queue_order, 2147483647), created_at ASC";

/// The workspace and repository of a chat, from which its worktree is resolved.
const WORKTREE_SQL: &str = "\
SELECT w.directory_name, w.branch, r.name, r.root_path
 FROM sessions s
 JOIN workspaces w ON w.id = s.workspace_id
 LEFT JOIN repos r ON r.id = w.repository_id
 WHERE s.id = ?1 LIMIT 1";

/// The columns of the worktree query.
struct WorktreeRow {
    directory_name: Option<String>,
    branch: Option<String>,
    repo_name: Option<String>,
    repo_root: Option<String>,
}

impl Reads {
    /// The entries of the rows after `after`, the new cursor, and the full queue snapshot.
    ///
    /// `after` is a `session_messages` row id and is not checked: every row of the chat with a
    /// greater id is returned. An unknown chat is not an error; it has no rows and an empty
    /// queue. The rows are read in batches, each in its own `read`, and the queue after them;
    /// none of these reads shares a transaction.
    pub fn get_messages(
        &self,
        session_id: &str,
        after: i64,
    ) -> Result<MessagesResponse, ReadError> {
        let mut entries = Vec::new();
        let mut cursor = after;
        // Resolved once, after the first batch came back non-empty, outside any `read`.
        let mut worktree: Option<Option<String>> = None;
        loop {
            let batch = self.read_batch(session_id, cursor)?;
            let Some(last) = batch.last() else { break };
            cursor = last.rowid;
            let worktree = match &worktree {
                Some(resolved) => resolved,
                None => worktree.insert(self.chat_worktree(session_id)?),
            };
            for row in &batch {
                entries.extend(parse_message(row, worktree.as_deref()));
            }
            let full = batch.len() == MESSAGE_BATCH_ROWS;
            drop(batch);
            if !full {
                break;
            }
        }

        let queued = self.queued_messages(session_id)?;
        Ok(MessagesResponse {
            entries,
            cursor,
            queued,
            pending_question: self.pending_question(session_id)?,
        })
    }

    /// The next batch of durable rows of the chat after `cursor`, at most `MESSAGE_BATCH_ROWS`.
    /// The database lock is held only while this one batch is fetched.
    fn read_batch(&self, session_id: &str, cursor: i64) -> Result<Vec<StoredMessage>, ReadError> {
        Ok(self.db().read("messages.rows", |conn| {
            let mut stmt = conn.prepare_cached(MESSAGES_SQL)?;
            let rows = stmt.query_map(
                rusqlite::params![session_id, cursor, MESSAGE_BATCH_ROWS as i64],
                |row| {
                    Ok(StoredMessage {
                        rowid: row.get(0)?,
                        id: row.get(1)?,
                        content: row.get(2)?,
                        created_at: row.get(3)?,
                        sent_at: row.get(4)?,
                        queue_order: row.get(5)?,
                    })
                },
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?)
    }

    /// The directory of the chat's worktree, or `None` when the chat, its workspace or the
    /// directory is not found. The lookup runs in the database; the directory is resolved
    /// after the lock is released.
    pub(super) fn chat_worktree(&self, session_id: &str) -> Result<Option<String>, ReadError> {
        let found = self.db().read("messages.worktree", |conn| {
            conn.query_row(WORKTREE_SQL, [session_id], |row| {
                Ok(WorktreeRow {
                    directory_name: row.get(0)?,
                    branch: row.get(1)?,
                    repo_name: row.get(2)?,
                    repo_root: row.get(3)?,
                })
            })
            .optional()
        })?;
        Ok(found.and_then(|row| {
            resolve_worktree(
                self.workspaces_root(),
                row.repo_name.as_deref(),
                row.directory_name.as_deref(),
                row.branch.as_deref(),
                row.repo_root.as_deref(),
            )
            .map(|path| path.to_string_lossy().into_owned())
        }))
    }

    /// The renderable queue-mode outbox rows of the chat, in sending order. Without the outbox
    /// table the queue is empty: the legacy rows of that era carry their own `queued` flag
    /// inside the durable entries.
    fn queued_messages(&self, session_id: &str) -> Result<Vec<TranscriptEntry>, ReadError> {
        let rows = self.db().read("messages.queue", |conn| {
            let present = conn
                .query_row(OUTBOX_PROBE_SQL, [], |_| Ok(()))
                .optional()?
                .is_some();
            if !present {
                return Ok(Vec::new());
            }
            let mut stmt = conn.prepare(QUEUE_SQL)?;
            let rows = stmt.query_map([session_id], |row| {
                Ok(StoredOutboxMessage {
                    message_id: row.get(0)?,
                    delivery_payload: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.iter().filter_map(parse_outbox_message).collect())
    }
}
