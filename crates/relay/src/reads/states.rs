//! The cross-workspace reads the notifier watches: the state of every open chat, the last thing
//! an agent said in a chat, and the title of a workspace.

use rusqlite::Row;

use super::{ReadError, Reads};
use crate::transcript::{parse_message, StoredMessage, TranscriptRole};

/// How many of a chat's newest rows `last_assistant_text` looks through.
const TAIL_ROWS: i64 = 20;

/// Every non-hidden chat of every `ready` workspace, in one statement. `turn_started_at` is the
/// expression of `OPEN_SESSIONS_SQL`. The tabs of a workspace are counted once per workspace by
/// the joined aggregate, not once per chat.
const SESSION_STATES_SQL: &str = "\
SELECT s.id, s.workspace_id, s.status, s.title, s.last_user_message_at,
       w.workspace_name, w.pr_title, w.branch, w.directory_name,
       r.name AS repo_name,
       COALESCE(t.tabs, 0) AS tab_count,
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
JOIN workspaces w ON w.id = s.workspace_id
LEFT JOIN repos r ON r.id = w.repository_id
LEFT JOIN (SELECT workspace_id, COUNT(*) AS tabs
             FROM sessions
            WHERE COALESCE(is_hidden, 0) = 0
            GROUP BY workspace_id) t ON t.workspace_id = w.id
WHERE w.state = 'ready' AND COALESCE(s.is_hidden, 0) = 0";

/// The newest rows of a chat, newest first. The inner query picks the `rowid`s from an index
/// alone, so the sort never carries the text of the chat's older rows (chats reach tens of
/// megabytes); only the rows kept are read from the table.
const TAIL_SQL: &str = "\
SELECT rowid, id, content, created_at, sent_at, queue_order
 FROM session_messages
 WHERE rowid IN (SELECT rowid FROM session_messages
                  WHERE session_id = ?1
                  ORDER BY rowid DESC
                  LIMIT ?2)
 ORDER BY rowid DESC";

/// One open chat of a ready workspace, as the notifier watches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStateRow {
    pub session_id: String,
    pub workspace_id: String,
    pub status: Option<String>,
    pub turn_started_at: Option<String>,
    pub last_user_message_at: Option<String>,
    pub workspace_title: String,
    pub repo_name: Option<String>,
    /// The chat's title, only when its workspace has more than one open chat.
    pub session_title: Option<String>,
}

impl Reads {
    /// Every non-hidden chat of every workspace whose state is `ready`.
    pub fn session_states(&self) -> Result<Vec<SessionStateRow>, ReadError> {
        Ok(self.db().read("session_states", |conn| {
            let mut stmt = conn.prepare_cached(SESSION_STATES_SQL)?;
            let rows = stmt
                .query_map([], session_state)?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })?)
    }

    /// The text of the last non-empty assistant entry among the chat's newest 20 rows, trimmed;
    /// parsed exactly as the transcript route parses rows.
    pub fn last_assistant_text(&self, session_id: &str) -> Result<Option<String>, ReadError> {
        let rows = self.db().read("last_assistant_text", |conn| {
            let mut stmt = conn.prepare_cached(TAIL_SQL)?;
            let rows = stmt
                .query_map(rusqlite::params![session_id, TAIL_ROWS], |row| {
                    Ok(StoredMessage {
                        rowid: row.get(0)?,
                        id: row.get(1)?,
                        content: row.get(2)?,
                        created_at: row.get(3)?,
                        sent_at: row.get(4)?,
                        queue_order: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })?;
        // Newest row first; inside a row the last entry is the newest.
        for row in &rows {
            let entries = parse_message(row, None);
            for entry in entries.iter().rev() {
                if entry.role != TranscriptRole::Assistant {
                    continue;
                }
                let text = js_trim(&entry.text);
                if !text.is_empty() {
                    return Ok(Some(text.to_owned()));
                }
            }
        }
        Ok(None)
    }
}

fn session_state(row: &Row<'_>) -> rusqlite::Result<SessionStateRow> {
    let workspace_id: String = row.get("workspace_id")?;
    let workspace_name: Option<String> = row.get("workspace_name")?;
    let pr_title: Option<String> = row.get("pr_title")?;
    let branch: Option<String> = row.get("branch")?;
    let directory_name: Option<String> = row.get("directory_name")?;
    let tab_count: i64 = row.get("tab_count")?;
    let title: Option<String> = row.get("title")?;
    Ok(SessionStateRow {
        session_id: row.get("id")?,
        status: row.get("status")?,
        turn_started_at: row.get("turn_started_at")?,
        last_user_message_at: row.get("last_user_message_at")?,
        workspace_title: workspace_title(
            workspace_name.as_deref(),
            pr_title.as_deref(),
            branch.as_deref(),
            directory_name.as_deref(),
            &workspace_id,
        ),
        repo_name: row.get("repo_name")?,
        // A single-tab workspace's chat title is the workspace again: name it only when it
        // tells the chats apart.
        session_title: if tab_count > 1 { title } else { None },
        workspace_id,
    })
}

/// `workspace_name`, else `pr_title`, else the humanised branch, else `directory_name`, else the
/// first 8 characters of the id; empty strings count as missing.
pub fn workspace_title(
    workspace_name: Option<&str>,
    pr_title: Option<&str>,
    branch: Option<&str>,
    directory_name: Option<&str>,
    workspace_id: &str,
) -> String {
    let present = |value: Option<&str>| value.filter(|v| !v.is_empty()).map(str::to_owned);
    present(workspace_name)
        .or_else(|| present(pr_title))
        .or_else(|| Some(humanize_branch(branch)).filter(|v| !v.is_empty()))
        .or_else(|| present(directory_name))
        .unwrap_or_else(|| workspace_id.chars().take(8).collect())
}

/// The words of a branch name: what follows the first `/`, with `-` and `_` read as spaces,
/// trimmed, the first letter in capitals.
fn humanize_branch(branch: Option<&str>) -> String {
    let branch = branch.unwrap_or("");
    let slug = branch.split_once('/').map_or(branch, |(_, rest)| rest);
    let words = slug.replace(['-', '_'], " ");
    let words = js_trim(&words);
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `s.trim()` of JavaScript: Unicode white space, without U+0085 and with U+FEFF.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}
