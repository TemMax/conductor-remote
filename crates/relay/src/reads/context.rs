//! The context breakdown of a chat: what fills its window.
//!
//! The chat is read in batches, each in its own `read`, so that only one batch of raw rows is
//! in memory at a time and the database lock is released between batches. The last result of
//! every chat is kept, because the phone asks again whenever the chat changes. Requests for the
//! same chat that arrive together share one computation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use rusqlite::types::Value as SqlValue;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::{ReadError, Reads};
use crate::transcript::context::{estimate_text_tokens, ContextAccumulator, ContextCategories};
use crate::transcript::render::{render_transcript, RenderFormat};
use crate::transcript::{parse_message, StoredMessage, TranscriptEntry};

/// How many rows of a chat one `read` fetches.
const CONTEXT_BATCH_ROWS: usize = 500;

/// How many chats keep their last result.
const MAX_REMEMBERED_CHATS: usize = 32;

/// How many chats have a lock of their own for the time they are computed; past this the locks
/// nobody holds are dropped.
const MAX_BUILD_LOCKS: usize = 64;

/// The stored counters of an open chat.
const CHAT_SQL: &str = "\
SELECT context_token_count, context_used_percent
 FROM sessions
 WHERE id = ?1 AND COALESCE(is_hidden, 0) = 0
 LIMIT 1";

/// What tells that the rows of a chat are the rows they were.
const ROWS_STAMP_SQL: &str = "\
SELECT MAX(rowid), COUNT(*) FROM session_messages WHERE session_id = ?1";

/// One batch of the rows of a chat after a cursor. `rowid` is the cursor.
const ROWS_SQL: &str = "\
SELECT rowid, id, role, content, created_at, sent_at, queue_order
 FROM session_messages
 WHERE session_id = ?1 AND rowid > ?2
 ORDER BY rowid ASC
 LIMIT ?3";

/// The TypeScript `ContextBreakdown`. Field order is the order of the keys.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBreakdown {
    /// The total Conductor stored for the last completed turn, rounded, never negative.
    pub total_tokens: i64,
    /// The stored percentage of the window in use; `null` when it is missing or not finite.
    pub used_percent: Option<f64>,
    /// Whether the counted window follows a compaction boundary.
    pub compacted: bool,
    pub categories: ContextCategories,
    pub fork_tokens: ForkTokens,
}

/// Estimated sizes of the transcript a fork attaches, in tokens, per level of detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ForkTokens {
    pub concise: i64,
    pub reasoning: i64,
    pub full: i64,
}

/// The facts a stored result was made from. The result is reused while all of them hold.
#[derive(Clone, Debug, PartialEq)]
struct Stamp {
    token_count: Option<f64>,
    used_percent: Option<f64>,
    max_rowid: Option<i64>,
    row_count: i64,
}

/// A row of the chat: the columns the accumulator and the parser read.
struct ContextRow {
    role: Option<String>,
    message: StoredMessage,
}

impl Reads {
    /// The breakdown of an open chat; `None` for an unknown or a closed (hidden) chat.
    ///
    /// The result of the last call for the chat is returned again while the stored counters
    /// and the chat's `MAX(rowid)` and `COUNT(*)` of rows are what they were then. The rows
    /// are read in batches, each in its own `read`, none sharing a transaction with another.
    pub fn context_breakdown(
        &self,
        session_id: &str,
    ) -> Result<Option<ContextBreakdown>, ReadError> {
        let Some(stamp) = self.context_stamp(session_id)? else {
            return Ok(None);
        };
        let key = (self.db().path().to_path_buf(), session_id.to_owned());
        if let Some(kept) = remembered().get(&key, &stamp) {
            return Ok(Some(kept));
        }
        // One computation per chat at a time: a request that arrives meanwhile waits here and
        // finds the result of the first one, made from the same stamp, once it gets the lock.
        let lock = build_lock(&key);
        let _computing = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(kept) = remembered().get(&key, &stamp) {
            return Ok(Some(kept));
        }
        count_computation(&key);
        let hook = BUILD_HOOK.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(hook) = hook {
            hook(session_id);
        }
        // The stamp was taken before the rows are read: a row that arrives in between makes
        // the next call's stamp differ, so a result is never kept for rows it did not see.
        let breakdown = self.compute_context(session_id, &stamp)?;
        remembered().put(key, stamp, breakdown.clone());
        Ok(Some(breakdown))
    }

    /// The stored counters of the chat and the stamp of its rows, read together under one
    /// lock; `None` when the chat is unknown or hidden.
    fn context_stamp(&self, session_id: &str) -> Result<Option<Stamp>, ReadError> {
        Ok(self.db().read("context.chat", |conn| {
            let counters = conn
                .query_row(CHAT_SQL, [session_id], |row| {
                    Ok((row.get::<_, SqlValue>(0)?, row.get::<_, SqlValue>(1)?))
                })
                .optional()?;
            let Some((token_count, used_percent)) = counters else {
                return Ok(None);
            };
            let (max_rowid, row_count) = conn.query_row(ROWS_STAMP_SQL, [session_id], |row| {
                Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?))
            })?;
            Ok(Some(Stamp {
                token_count: number(token_count),
                used_percent: number(used_percent),
                max_rowid,
                row_count,
            }))
        })?)
    }

    fn compute_context(
        &self,
        session_id: &str,
        stamp: &Stamp,
    ) -> Result<ContextBreakdown, ReadError> {
        let total_tokens = total_tokens(stamp.token_count);
        let mut accumulator = ContextAccumulator::new();
        let mut entries: Vec<TranscriptEntry> = Vec::new();
        let mut cursor = 0;
        // Resolved once, after the first batch came back non-empty, outside any `read`.
        let mut worktree: Option<Option<String>> = None;
        loop {
            let batch = self.context_batch(session_id, cursor)?;
            let Some(last) = batch.last() else { break };
            cursor = last.message.rowid;
            let worktree = match &worktree {
                Some(resolved) => resolved,
                None => worktree.insert(self.chat_worktree(session_id)?),
            };
            for row in &batch {
                accumulator.push(row.role.as_deref(), row.message.content.as_deref());
                entries.extend(parse_message(&row.message, worktree.as_deref()));
            }
            let full = batch.len() == CONTEXT_BATCH_ROWS;
            drop(batch);
            if !full {
                break;
            }
        }
        let (categories, compacted) = accumulator.finish(total_tokens);
        Ok(ContextBreakdown {
            total_tokens,
            used_percent: stamp.used_percent.filter(|percent| percent.is_finite()),
            compacted,
            categories,
            fork_tokens: ForkTokens {
                concise: fork_tokens(&entries, false, false),
                reasoning: fork_tokens(&entries, true, false),
                full: fork_tokens(&entries, true, true),
            },
        })
    }

    /// The next batch of rows of the chat after `cursor`, at most `CONTEXT_BATCH_ROWS`. The
    /// database lock is held only while this one batch is fetched.
    fn context_batch(&self, session_id: &str, cursor: i64) -> Result<Vec<ContextRow>, ReadError> {
        Ok(self.db().read("context.rows", |conn| {
            let mut stmt = conn.prepare_cached(ROWS_SQL)?;
            let rows = stmt.query_map(
                rusqlite::params![session_id, cursor, CONTEXT_BATCH_ROWS as i64],
                |row| {
                    Ok(ContextRow {
                        role: row.get(2)?,
                        message: StoredMessage {
                            rowid: row.get(0)?,
                            id: row.get(1)?,
                            content: row.get(3)?,
                            created_at: row.get(4)?,
                            sent_at: row.get(5)?,
                            queue_order: row.get(6)?,
                        },
                    })
                },
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?)
    }
}

/// A stored number; anything else (NULL, text, a blob) is no number.
fn number(value: SqlValue) -> Option<f64> {
    match value {
        SqlValue::Integer(n) => Some(n as f64),
        SqlValue::Real(x) => Some(x),
        _ => None,
    }
}

/// `max(0, round(count or 0))`.
fn total_tokens(count: Option<f64>) -> i64 {
    // The cast saturates, and a NaN, which SQLite cannot store, would become 0.
    count.unwrap_or(0.0).round().max(0.0) as i64
}

/// The estimate for the transcript rendered one way. The text is dropped on return, so at
/// most one rendered transcript is alive at a time.
fn fork_tokens(entries: &[TranscriptEntry], thinking: bool, tools: bool) -> i64 {
    estimate_text_tokens(&render_transcript(
        entries,
        RenderFormat { thinking, tools },
    ))
}

type Key = (PathBuf, String);

struct Kept {
    stamp: Stamp,
    breakdown: ContextBreakdown,
    /// The value of the table's clock when the result was last stored or reused.
    used: u64,
}

/// The last result of up to `MAX_REMEMBERED_CHATS` chats; the one unused longest makes room.
#[derive(Default)]
struct Table {
    clock: u64,
    chats: HashMap<Key, Kept>,
}

impl Table {
    /// The stored result of the chat when it was made from `stamp`.
    fn get(&mut self, key: &Key, stamp: &Stamp) -> Option<ContextBreakdown> {
        self.clock += 1;
        let kept = self
            .chats
            .get_mut(key)
            .filter(|kept| kept.stamp == *stamp)?;
        kept.used = self.clock;
        Some(kept.breakdown.clone())
    }

    fn put(&mut self, key: Key, stamp: Stamp, breakdown: ContextBreakdown) {
        self.clock += 1;
        if !self.chats.contains_key(&key) && self.chats.len() >= MAX_REMEMBERED_CHATS {
            let oldest = self
                .chats
                .iter()
                .min_by_key(|(_, kept)| kept.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.chats.remove(&oldest);
            }
        }
        let used = self.clock;
        self.chats.insert(
            key,
            Kept {
                stamp,
                breakdown,
                used,
            },
        );
    }
}

/// The locks held while a chat is computed, by database path and chat id. A lock is looked up
/// here and then taken outside the table, so the table is never held across a computation.
fn build_lock(key: &Key) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<Key, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !locks.contains_key(key) && locks.len() >= MAX_BUILD_LOCKS {
        // Keep the locks a request holds or waits on, so their computations stay shared.
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    Arc::clone(locks.entry(key.clone()).or_default())
}

#[doc(hidden)]
pub type BuildHook = Arc<dyn Fn(&str) + Send + Sync>;

static BUILD_HOOK: Mutex<Option<BuildHook>> = Mutex::new(None);

/// Sets the function called with the chat id at the start of every computation, while the chat's
/// lock is held; `None` removes it. For tests.
#[doc(hidden)]
pub fn set_context_build_hook(hook: Option<BuildHook>) {
    *BUILD_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = hook;
}

fn computations() -> MutexGuard<'static, HashMap<Key, u64>> {
    static COUNTS: OnceLock<Mutex<HashMap<Key, u64>>> = OnceLock::new();
    COUNTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn count_computation(key: &Key) {
    *computations().entry(key.clone()).or_insert(0) += 1;
}

/// How many times the context of the chat was computed since the process started. For tests.
#[doc(hidden)]
pub fn context_computations(db_path: &Path, session_id: &str) -> u64 {
    computations()
        .get(&(db_path.to_path_buf(), session_id.to_owned()))
        .copied()
        .unwrap_or(0)
}

/// The process-wide table, keyed by database path and chat id. It is never held across a
/// database read or a computation.
fn remembered() -> MutexGuard<'static, Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(max_rowid: i64) -> Stamp {
        Stamp {
            token_count: Some(10.0),
            used_percent: None,
            max_rowid: Some(max_rowid),
            row_count: max_rowid,
        }
    }

    fn breakdown(total: i64) -> ContextBreakdown {
        ContextBreakdown {
            total_tokens: total,
            used_percent: None,
            compacted: false,
            categories: ContextCategories::default(),
            fork_tokens: ForkTokens {
                concise: 1,
                reasoning: 1,
                full: 1,
            },
        }
    }

    fn key(chat: usize) -> Key {
        (PathBuf::from("/ctx/db"), format!("ctx-{chat}"))
    }

    #[test]
    fn a_result_is_reused_for_its_stamp_only() {
        let mut table = Table::default();
        table.put(key(0), stamp(1), breakdown(7));
        assert_eq!(table.get(&key(0), &stamp(1)), Some(breakdown(7)));
        assert_eq!(table.get(&key(0), &stamp(2)), None);
        assert_eq!(table.get(&key(1), &stamp(1)), None);
    }

    #[test]
    fn a_new_result_replaces_the_old_one_of_the_chat() {
        let mut table = Table::default();
        table.put(key(0), stamp(1), breakdown(7));
        table.put(key(0), stamp(2), breakdown(8));
        assert_eq!(table.chats.len(), 1);
        assert_eq!(table.get(&key(0), &stamp(1)), None);
        assert_eq!(table.get(&key(0), &stamp(2)), Some(breakdown(8)));
    }

    #[test]
    fn the_chat_unused_longest_makes_room_for_the_thirty_third() {
        let mut table = Table::default();
        for chat in 0..MAX_REMEMBERED_CHATS {
            table.put(key(chat), stamp(1), breakdown(chat as i64));
        }
        // Chat 0 is the oldest stored, but it is used again; chat 1 is now the oldest.
        assert!(table.get(&key(0), &stamp(1)).is_some());
        table.put(key(99), stamp(1), breakdown(99));
        assert_eq!(table.chats.len(), MAX_REMEMBERED_CHATS);
        assert!(table.get(&key(0), &stamp(1)).is_some());
        assert_eq!(table.get(&key(1), &stamp(1)), None);
        assert!(table.get(&key(99), &stamp(1)).is_some());
    }

    #[test]
    fn the_total_is_rounded_and_never_negative() {
        assert_eq!(total_tokens(None), 0);
        assert_eq!(total_tokens(Some(-5.0)), 0);
        assert_eq!(total_tokens(Some(-0.4)), 0);
        assert_eq!(total_tokens(Some(1234.4)), 1234);
        assert_eq!(total_tokens(Some(1234.5)), 1235);
        assert_eq!(total_tokens(Some(f64::INFINITY)), i64::MAX);
    }
}
