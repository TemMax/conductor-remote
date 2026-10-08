//! The latest root question's lifecycle. Polling never focuses Conductor.
use super::{ReadError, Reads};
use crate::transcript::questions::{QuestionProvider, QuestionRequest};
use crate::transcript::{parse_message, StoredMessage};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::collections::HashMap;

impl Reads {
    pub fn pending_question(&self, session_id: &str) -> Result<Option<QuestionRequest>, ReadError> {
        Ok(self
            .db()
            .read("questions.pending", |conn| pending(conn, session_id))?)
    }
}

fn root_frame(text: &str) -> Option<Value> {
    let frame: Value = serde_json::from_str(text).ok()?;
    if frame
        .get("parent_tool_use_id")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        return None;
    }
    Some(frame)
}

fn codex_answer(text: &str, question: &QuestionRequest) -> bool {
    let Some(first) = question.questions.first() else {
        return false;
    };
    let Some(mut rest) = text.strip_prefix(&format!("{}\n", first.question)) else {
        return false;
    };
    for q in &question.questions[1..] {
        let Some((answer, next)) = rest.split_once(&format!("\n{}\n", q.question)) else {
            return false;
        };
        if answer.trim().is_empty() {
            return false;
        }
        rest = next;
    }
    !rest.trim().is_empty()
}

fn pending(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<QuestionRequest>> {
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM sessions WHERE id=?",
            [session_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if !matches!(status.as_deref(), Some("working" | "needs_user_input")) {
        return Ok(None);
    }
    let mut candidates = conn.prepare("SELECT rowid,id,content,turn_id FROM session_messages WHERE session_id=?1 AND role='assistant' AND cancelled_at IS NULL AND (queue_order IS NULL OR sent_at IS NOT NULL) AND (instr(content,'codex_async_questions')>0 OR instr(content,'AskUserQuestion')>0) ORDER BY rowid")?;
    let mut rows = candidates.query([session_id])?;
    let mut first_seen: HashMap<(QuestionProvider, String, Option<String>), QuestionRequest> =
        HashMap::new();
    let mut latest: Option<(i64, Option<String>, QuestionRequest)> = None;
    while let Some(row) = rows.next()? {
        let content: String = row.get(2)?;
        let Some(frame) = root_frame(&content) else {
            continue;
        };
        if frame.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let carrier = frame.get("codex_async_questions").is_some()
            || frame
                .pointer("/message/content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|b| {
                        matches!(
                            b.get("name").and_then(Value::as_str),
                            Some("AskUserQuestion" | "mcp__conductor__AskUserQuestion")
                        ) && b.get("type").and_then(Value::as_str) == Some("tool_use")
                    })
                });
        if !carrier {
            continue;
        }
        let rowid: i64 = row.get(0)?;
        let turn: Option<String> = row.get(3)?;
        let question = parse_message(
            &StoredMessage {
                rowid,
                id: row.get(1)?,
                content: Some(content),
                created_at: None,
                sent_at: None,
                queue_order: None,
            },
            None,
        )
        .into_iter()
        .filter_map(|e| e.question)
        .next_back();
        let Some(question) = question else {
            latest = None;
            continue;
        };
        let key = (question.provider, question.id.clone(), turn.clone());
        if let Some(original) = first_seen.get(&key) {
            if original != &question
                && latest.as_ref().is_some_and(|(_, current_turn, current)| {
                    current_turn == &turn
                        && current.id == question.id
                        && current.provider == question.provider
                })
            {
                latest = None;
            }
            continue;
        }
        first_seen.insert(key, question.clone());
        latest = Some((rowid, turn, question));
    }
    let Some((rowid, turn, question)) = latest else {
        return Ok(None);
    };
    let mut stmt=conn.prepare("SELECT content,turn_id,sent_at,queue_order FROM session_messages WHERE session_id=?1 AND rowid>?2 AND cancelled_at IS NULL ORDER BY rowid")?;
    let mut rows = stmt.query(rusqlite::params![session_id, rowid])?;
    while let Some(row) = rows.next()? {
        let content: Option<String> = row.get(0)?;
        let next_turn: Option<String> = row.get(1)?;
        let sent: Option<String> = row.get(2)?;
        let queued: Option<i64> = row.get(3)?;
        if queued.is_some() && sent.is_none() {
            continue;
        }
        let text = content.as_deref().unwrap_or("");
        let parsed = serde_json::from_str::<Value>(text).ok();
        let frame = root_frame(text);
        if parsed.is_some() && frame.is_none() {
            continue;
        }
        if turn.is_some() && next_turn.is_some() && turn != next_turn {
            return Ok(None);
        }
        if frame.as_ref().is_some_and(|f| {
            matches!(
                f.get("type").and_then(Value::as_str),
                Some("result" | "error")
            )
        }) {
            return Ok(None);
        }
        match question.provider {
            QuestionProvider::Claude => {
                if frame
                    .as_ref()
                    .and_then(|f| f.pointer("/message/content"))
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            b.get("type").and_then(Value::as_str) == Some("tool_result")
                                && b.get("tool_use_id").and_then(Value::as_str)
                                    == Some(question.id.as_str())
                        })
                    })
                {
                    return Ok(None);
                }
            }
            QuestionProvider::Codex => {
                // Conductor has no request id on these user rows. Match its native packet
                // prefix and question order, within the same turn, never arbitrary mentions.
                if frame.is_none()
                    && turn.is_some()
                    && turn == next_turn
                    && sent.is_some()
                    && codex_answer(text, &question)
                {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(question))
}
