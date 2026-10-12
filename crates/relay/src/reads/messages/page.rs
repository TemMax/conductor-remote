//! Bounded transcript windows. Legacy full-history reads remain available for export.

use rusqlite::OptionalExtension;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};

use super::{MessagesResponse, ReadError, Reads, StoredMessage, TranscriptEntry};
use crate::transcript::parse_message;

// A matching call is usually outside the window. Read only its row, then keep only
// the requested tool entry: other prose in the same frame is not page context.
const CONTEXT_CALL_SQL: &str = "SELECT rowid,id,content,created_at,sent_at,queue_order
 FROM session_messages WHERE rowid IN (
   SELECT m.rowid FROM session_messages m,
     json_each(CASE WHEN json_valid(m.content) THEN m.content ELSE '{}' END, '$.message.content') b
   WHERE m.session_id=?1 AND m.rowid<?2 AND instr(m.content,?3)>0
     AND json_extract(CASE WHEN b.type='object' THEN b.value ELSE '{}' END,'$.type')='tool_use'
     AND json_extract(CASE WHEN b.type='object' THEN b.value ELSE '{}' END,'$.id')=?3
   ORDER BY m.rowid ASC LIMIT 1)";

fn stored_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        rowid: r.get(0)?,
        id: r.get(1)?,
        content: r.get(2)?,
        created_at: r.get(3)?,
        sent_at: r.get(4)?,
        queue_order: r.get(5)?,
    })
}

#[derive(Clone, Copy)]
pub enum MessageWindow {
    Latest,
    Before(i64),
    After(i64),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    #[serde(flatten)]
    pub messages: MessagesResponse,
    pub oldest_cursor: i64,
    pub has_older: bool,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_entries: Vec<TranscriptEntry>,
}

impl Reads {
    pub fn get_message_page(
        &self,
        session: &str,
        window: MessageWindow,
        limit: usize,
    ) -> Result<MessagePage, ReadError> {
        let limit = limit.clamp(1, 500);
        let (condition, direction, bound) = match window {
            MessageWindow::Latest => ("?2 IS NULL", "DESC", None),
            MessageWindow::Before(before) => ("rowid < ?2", "DESC", Some(before)),
            MessageWindow::After(after) => ("rowid > ?2", "ASC", Some(after)),
        };
        // Select/sort small index entries before fetching the large content columns.
        let sql = format!(
            "SELECT rowid,id,content,created_at,sent_at,queue_order FROM session_messages
             WHERE rowid IN (SELECT rowid FROM session_messages
                              WHERE session_id=?1 AND {condition}
                              ORDER BY rowid {direction} LIMIT ?3)
             ORDER BY rowid ASC"
        );
        let mut rows = self.db().read("messages.page", |conn| {
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows =
                stmt.query_map(rusqlite::params![session, bound, limit as i64 + 1], |r| {
                    Ok(StoredMessage {
                        rowid: r.get(0)?,
                        id: r.get(1)?,
                        content: r.get(2)?,
                        created_at: r.get(3)?,
                        sent_at: r.get(4)?,
                        queue_order: r.get(5)?,
                    })
                })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        let more = rows.len() > limit;
        let forward = matches!(window, MessageWindow::After(_));
        if more {
            if forward {
                rows.pop();
            } else {
                rows.remove(0);
            }
        }
        let oldest_cursor = rows.first().map_or(0, |r| r.rowid);
        let cursor = rows.last().map_or_else(
            || match window {
                MessageWindow::After(after) => after,
                _ => 0,
            },
            |r| r.rowid,
        );
        let worktree = if rows.is_empty() {
            None
        } else {
            self.chat_worktree(session)?
        };
        let entries: Vec<_> = rows
            .iter()
            .flat_map(|r| parse_message(r, worktree.as_deref()))
            .collect();
        let context_entries =
            self.message_page_context(session, oldest_cursor, &entries, worktree.as_deref())?;
        Ok(MessagePage {
            messages: MessagesResponse {
                entries,
                cursor,
                queued: self.queued_messages(session)?,
                pending_question: self.pending_question(session)?,
            },
            oldest_cursor,
            has_older: !forward && more,
            has_more: forward && more,
            context_entries,
        })
    }

    fn message_page_context(
        &self,
        session: &str,
        oldest: i64,
        entries: &[TranscriptEntry],
        worktree: Option<&str>,
    ) -> Result<Vec<TranscriptEntry>, ReadError> {
        let mut known: HashSet<String> = entries
            .iter()
            .filter(|e| e.tool.is_some())
            .filter_map(|e| e.tool_use_id.clone())
            .collect();
        let mut pending: VecDeque<String> = entries
            .iter()
            .flat_map(|e| {
                e.parent_tool_use_id
                    .iter()
                    .chain(
                        e.tool_use_id
                            .iter()
                            .filter(|_| e.tool.is_none() && e.output.is_some()),
                    )
                    .cloned()
            })
            .collect();
        let mut context = Vec::new();
        while let Some(id) = pending.pop_front() {
            // Includes misses, so cyclic/unknown parent ids are looked up at most once.
            if !known.insert(id.clone()) {
                continue;
            }
            for entry in self.message_context_call(session, oldest, &id, worktree)? {
                if let Some(parent) = &entry.parent_tool_use_id {
                    pending.push_back(parent.clone());
                }
                context.push(entry);
            }
        }
        context.sort_by_key(|e| e.rowid);
        Ok(context)
    }

    fn message_context_call(
        &self,
        session: &str,
        oldest: i64,
        id: &str,
        worktree: Option<&str>,
    ) -> Result<Vec<TranscriptEntry>, ReadError> {
        let key = (session.to_owned(), id.to_owned());
        let cached = self
            .message_call_rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        let calls = |row: &StoredMessage| {
            parse_message(row, worktree)
                .into_iter()
                .filter(|e| e.tool.is_some() && e.tool_use_id.as_deref() == Some(id))
                .collect::<Vec<_>>()
        };
        if let Some((rowid, message_id)) = cached.filter(|(rowid, _)| *rowid < oldest) {
            let row = self.db().read("messages.context.row", |conn| {
                conn.query_row("SELECT rowid,id,content,created_at,sent_at,queue_order FROM session_messages WHERE rowid=?1 AND session_id=?2 AND id=?3",
                    rusqlite::params![rowid, session, message_id], stored_row).optional()
            })?;
            if let Some(row) = row {
                let entries = calls(&row);
                if !entries.is_empty() {
                    return Ok(entries);
                }
            }
        }
        let row = self.db().read("messages.context", |conn| {
            conn.query_row(
                CONTEXT_CALL_SQL,
                rusqlite::params![session, oldest, id],
                stored_row,
            )
            .optional()
        })?;
        let mut cache = self
            .message_call_rows
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(row) = row else {
            cache.remove(&key);
            return Ok(Vec::new());
        };
        if cache.len() >= 256 && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, (row.rowid, row.id.clone()));
        drop(cache);
        Ok(calls(&row))
    }
}
