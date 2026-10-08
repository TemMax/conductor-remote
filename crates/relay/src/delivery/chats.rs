//! Chat restore, history links and splits.

use std::ops::Range;
use std::sync::Arc;

use rusqlite::OptionalExtension;
use serde_json::{json, Map, Value};

use super::service::{
    blocking, internal, locate_checkout, open_new_chat, with_session, workspace_target, Inner,
    NO_SESSION_WORKSPACE,
};
use super::split_workspace::{self, ForkSource, Prepared};
use super::{SplitDestination, SplitRequest, WriteAnswer};
use crate::contract::Priority;
use crate::files::attachments::{write_attachment, Written};
use crate::reads::{ReadError, Reads};
use crate::state::store::Store;
use crate::transcript::render::{render_transcript, RenderFormat};
use crate::transcript::{TranscriptEntry, TranscriptRole};
use crate::ui::driver::{Target, UiDriver, UiError};

/// The `strategy` of a restore: the chat is brought back by Conductor's own deep link.
const RESTORE_STRATEGY: &str = "deep-link";
pub(crate) const CHAT_NOT_FOUND: &str = "chat not found";
pub(crate) const NOT_IN_WORKSPACE: &str = "chat is not in that workspace";
const WORKSPACE_NOT_LIVE: &str = "Restore this workspace in Conductor before restoring its tabs.";
const NOT_RESTORED: &str =
    "Conductor has not restored this tab yet. Try again, or update Conductor on your Mac.";
const CHATS_NOT_IN_WORKSPACE: &str = "chats not found in that workspace";
const SOURCE_NOT_FOUND: &str = "source chat not found";
const WORKTREE_UNRESOLVED: &str = "worktree path unresolved";
const SPLIT_CHAT_NOT_FOUND: &str = "chat not found in that workspace";
const THROUGH_NOT_POSITIVE: &str = "throughRowid must be a positive integer";
const ONLY_NOT_POSITIVE: &str = "onlyRowid must be a positive integer";
const BOTH_CUTS: &str = "throughRowid and onlyRowid cannot be combined";
const NOT_IN_CHAT: &str = "that message is not in this chat";
const NOTHING_TO_COPY: &str = "that chat has nothing to copy yet";
/// The product a transcript's header names as its copier.
const PRODUCT: &str = "Conductor Remote";
/// JavaScript's `Number.MAX_SAFE_INTEGER`: the largest rowid the phone sends exactly.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The owner of a chat and what a history link inherits from it, whether open or closed.
pub(crate) struct ChatRow {
    pub(crate) workspace_id: Option<String>,
    title: Option<String>,
    created_at: Option<String>,
}

const CHAT_ROW_SQL: &str = "SELECT workspace_id, title, created_at FROM sessions WHERE id = ?";

/// The row of a chat in any state; `None` when there is no such chat. Blocking.
pub(crate) fn chat_row(reads: &Reads, session_id: &str) -> Result<Option<ChatRow>, ReadError> {
    Ok(reads.db().read("writes.chat_row", |conn| {
        conn.query_row(CHAT_ROW_SQL, [session_id], |row| {
            Ok(ChatRow {
                workspace_id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
            })
        })
        .optional()
    })?)
}

/// Whether the chat is one of its workspace's open chats. Blocking.
fn is_visible(reads: &Reads, workspace_id: &str, session_id: &str) -> Result<bool, ReadError> {
    Ok(reads
        .visible_sessions(workspace_id)?
        .iter()
        .any(|chat| chat.id == session_id))
}

/// 502 `{"ok":false,"strategy":"deep-link","error":…}`.
fn restore_failed(error: &str) -> WriteAnswer {
    WriteAnswer::json(
        502,
        json!({ "ok": false, "strategy": RESTORE_STRATEGY, "error": error }),
    )
}

/// What the restore job saw on the UI thread.
enum Restored {
    /// Another restore brought the chat back while this one waited for the UI thread.
    AlreadyOpen,
    Visible,
    NotVisible,
    Failed(UiError),
}

/// `POST /api/sessions/:id/restore`.
pub(crate) async fn restore_chat(
    inner: Arc<Inner>,
    session_id: String,
    workspace_id: Option<String>,
    priority: Priority,
) -> WriteAnswer {
    let owner = {
        let session_id = session_id.clone();
        blocking(&inner.reads, "restore.owner", move |reads| {
            Ok(chat_row(reads, &session_id)?.and_then(|row| row.workspace_id))
        })
        .await
    };
    let owner = match owner {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, CHAT_NOT_FOUND),
        Ok(Some(owner)) => owner,
    };
    if workspace_id.as_deref().is_some_and(|given| given != owner) {
        return WriteAnswer::error(409, NOT_IN_WORKSPACE);
    }

    let located = {
        let session_id = session_id.clone();
        blocking(&inner.reads, "restore.workspace", move |reads| {
            let Some(workspace) = reads.write_workspace(Some(&owner), None)? else {
                return Ok(None);
            };
            let visible = is_visible(reads, &workspace.id, &session_id)?;
            Ok(Some((workspace, visible)))
        })
        .await
    };
    let workspace = match located {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(409, WORKSPACE_NOT_LIVE),
        Ok(Some((workspace, true))) => {
            let body = json!({ "ok": true, "strategy": RESTORE_STRATEGY, "alreadyOpen": true });
            return with_session(&inner.reads, &workspace.id, &session_id, body).await;
        }
        Ok(Some((workspace, false))) => workspace,
    };

    let target = Target {
        session_id: Some(session_id.clone()),
        ..workspace_target(&workspace)
    };
    let reads = Arc::clone(&inner.reads);
    let timings = inner.timings;
    let chat = session_id.clone();
    // The link and the looks share one job, so no other command navigates away while Conductor
    // un-hides the chat, and two restores of one chat open the link once.
    let job = move |driver: &mut dyn UiDriver| -> Restored {
        let visible = || match is_visible(&reads, &target.workspace_id, &chat) {
            Ok(visible) => visible,
            // A later look may still see the chat, so a failed one is only logged.
            Err(error) => {
                tracing::error!(%error, "the open chats during a restore could not be read");
                false
            }
        };
        if visible() {
            return Restored::AlreadyOpen;
        }
        if let Err(error) = driver.open_link(&target.deep_link()) {
            return Restored::Failed(error);
        }
        for _ in 0..timings.restore_checks {
            std::thread::sleep(timings.restore_poll);
            if visible() {
                return Restored::Visible;
            }
        }
        Restored::NotVisible
    };

    let body = match inner.ui.run(priority, job).await {
        Err(error) => return restore_failed(&error.to_string()),
        Ok(Restored::Failed(error)) => return restore_failed(&error.to_string()),
        Ok(Restored::NotVisible) => return restore_failed(NOT_RESTORED),
        Ok(Restored::AlreadyOpen) => {
            json!({ "ok": true, "strategy": RESTORE_STRATEGY, "alreadyOpen": true })
        }
        Ok(Restored::Visible) => json!({ "ok": true, "strategy": RESTORE_STRATEGY }),
    };
    with_session(&inner.reads, &workspace.id, &session_id, body).await
}

/// The relay's own store, once the writes are configured.
fn store_of(inner: &Inner) -> Option<Arc<Store>> {
    inner.deps.as_ref().map(|deps| Arc::clone(&deps.store))
}

/// `POST /api/sessions/:id/history`.
pub(crate) async fn join_history(
    inner: Arc<Inner>,
    session_id: String,
    workspace_id: String,
    previous_session_id: String,
) -> WriteAnswer {
    let Some(store) = store_of(&inner) else {
        tracing::error!("a history join reached writes that were never configured");
        return internal();
    };
    let rows = {
        let session_id = session_id.clone();
        let previous_session_id = previous_session_id.clone();
        blocking(&inner.reads, "history.chats", move |reads| {
            Ok((
                chat_row(reads, &session_id)?,
                chat_row(reads, &previous_session_id)?,
            ))
        })
        .await
    };
    let (current, previous) = match rows {
        Err(answer) => return answer,
        Ok(rows) => rows,
    };
    let owned = |row: &Option<ChatRow>| {
        row.as_ref()
            .and_then(|row| row.workspace_id.as_deref())
            .is_some_and(|owner| owner == workspace_id)
    };
    if !owned(&previous) || !owned(&current) {
        return WriteAnswer::error(404, CHATS_NOT_IN_WORKSPACE);
    }
    let Some(previous) = previous else {
        return WriteAnswer::error(404, SOURCE_NOT_FOUND);
    };

    let title = previous.title.unwrap_or_default();
    let created_at = previous.created_at.unwrap_or_default();
    let joined = tokio::task::spawn_blocking(move || {
        store.join_chats(
            &session_id,
            &workspace_id,
            &previous_session_id,
            &title,
            &created_at,
        )
    })
    .await;
    match joined {
        Ok(Ok(Ok(()))) => WriteAnswer::json(200, json!({ "ok": true })),
        Ok(Ok(Err(refused))) => WriteAnswer::error(409, &refused),
        Ok(Err(error)) => {
            tracing::error!(%error, "a history join could not be stored");
            internal()
        }
        Err(error) => {
            tracing::error!(%error, "a history join did not finish");
            internal()
        }
    }
}

/// How a split cuts the transcript.
enum Cut {
    Whole,
    Through(i64),
    Only(i64),
}

/// The positions of the entries a cut keeps; `None` when no entry carries its rowid. The
/// entries of one row cross together: one row yields the reasoning, the prose and the tool calls
/// of one message.
fn cut_range(entries: &[TranscriptEntry], cut: &Cut) -> Option<Range<usize>> {
    match *cut {
        Cut::Whole => Some(0..entries.len()),
        // `transcriptThrough`: every entry up to that row, the row included.
        Cut::Through(rowid) => {
            entries.iter().position(|entry| entry.rowid == rowid)?;
            let end = entries
                .iter()
                .rposition(|entry| entry.rowid <= rowid)
                .map_or(0, |last| last + 1);
            Some(0..end)
        }
        // `transcriptMessage`: that row's entries and nothing around them.
        Cut::Only(rowid) => {
            let first = entries.iter().position(|entry| entry.rowid == rowid)?;
            let last = entries.iter().rposition(|entry| entry.rowid == rowid)?;
            Some(first..last + 1)
        }
    }
}

/// A tool entry with no tool name (absent or empty) and an output: the result answering a call.
/// The rule of the transcript render, which keeps its own private.
fn is_tool_result(entry: &TranscriptEntry) -> bool {
    entry.role == TranscriptRole::Tool
        && entry.tool.as_deref().is_none_or(str::is_empty)
        && entry.output.is_some()
}

/// A successful result: the render leaves it behind without admitting to it, so it counts in no
/// bucket.
fn is_quiet_result(entry: &TranscriptEntry) -> bool {
    is_tool_result(entry) && !entry.error
}

/// What a cut keeps and what it leaves out.
#[derive(Debug, Default)]
pub(super) struct Counts {
    kept: usize,
    thinking: usize,
    tools: usize,
    earlier: usize,
    later: usize,
}

fn count(entries: &[TranscriptEntry], range: &Range<usize>, format: RenderFormat) -> Counts {
    let outside =
        |slice: &[TranscriptEntry]| slice.iter().filter(|entry| !is_quiet_result(entry)).count();
    let mut counts = Counts {
        earlier: outside(&entries[..range.start]),
        later: outside(&entries[range.end..]),
        ..Counts::default()
    };
    for entry in &entries[range.clone()] {
        if is_quiet_result(entry) {
            continue;
        }
        if entry.role == TranscriptRole::Thinking && !format.thinking {
            counts.thinking += 1;
        } else if entry.role == TranscriptRole::Tool && !format.tools {
            counts.tools += 1;
        } else {
            counts.kept += 1;
        }
    }
    counts
}

/// The lines above the transcript: its title, where it came from, what it carries and, when it
/// is not the whole chat, where it stops.
fn transcript_header(
    title: &str,
    place: &str,
    session_id: &str,
    format: RenderFormat,
    cut: &Cut,
    later: usize,
) -> String {
    let mut lines = vec![
        format!("# Transcript of {title}"),
        String::new(),
        place.to_owned(),
        format!(
            "Copied from the Conductor chat `{session_id}` by {PRODUCT}. thinking {}, tool calls {}.",
            if format.thinking { "included" } else { "omitted" },
            if format.tools { "included" } else { "omitted" },
        ),
    ];
    match cut {
        Cut::Only(_) => lines.push(
            "The copy contains only the selected source message; all earlier and later messages \
             are omitted."
                .to_owned(),
        ),
        _ if later > 0 => lines.push(format!(
            "The copy stops partway through: {later} later {} not in it.",
            if later == 1 {
                "entry is"
            } else {
                "entries are"
            }
        )),
        _ => {}
    }
    lines.push(String::new());
    lines.push(String::new());
    lines.join("\n")
}

/// `Forked from <token>`, a blank line, then the prompt trimmed; with no prompt the last line is
/// left empty for the user's own words.
pub(super) fn attachment_prompt(token: &str, prompt: Option<&str>) -> String {
    let context = prompt.map(str::trim).unwrap_or_default();
    format!("Forked from {token}\n\n{context}")
}

/// The `attachment` of a split's answer: the written transcript and what its cut kept and left out.
pub(super) fn attachment_json(written: &Written, counts: &Counts) -> Value {
    json!({
        "name": written.name,
        "path": written.path,
        "bytes": written.bytes,
        "kept": counts.kept,
        "elided": {
            "thinking": counts.thinking,
            "tools": counts.tools,
            "earlier": counts.earlier,
            "later": counts.later,
        },
    })
}

/// `POST /api/sessions/:id/split`.
pub(crate) async fn split_chat(
    inner: Arc<Inner>,
    session_id: String,
    request: SplitRequest,
    priority: Priority,
) -> WriteAnswer {
    let located = {
        let session_id = session_id.clone();
        let workspace_id = request.workspace_id.clone();
        blocking(&inner.reads, "split.workspace", move |reads| {
            let Some(workspace) =
                reads.write_workspace(workspace_id.as_deref(), Some(&session_id))?
            else {
                return Ok(None);
            };
            let checkout = locate_checkout(reads, &workspace.id)?;
            let source = reads
                .visible_sessions(&workspace.id)?
                .into_iter()
                .find(|chat| chat.id == session_id);
            Ok(Some((workspace, checkout, source)))
        })
        .await
    };
    let (workspace, checkout, source) = match located {
        Err(answer) => return answer,
        Ok(None) => return WriteAnswer::error(404, NO_SESSION_WORKSPACE),
        Ok(Some(found)) => found,
    };
    let (repo_root, repo_name, worktree) = match checkout {
        Some(checkout) => (checkout.repo_root, checkout.repo_name, checkout.worktree),
        None => (None, None, None),
    };
    let Some(worktree) = worktree else {
        return WriteAnswer::error(409, WORKTREE_UNRESOLVED);
    };
    let Some(source) = source else {
        return WriteAnswer::error(404, SPLIT_CHAT_NOT_FOUND);
    };

    let positive =
        |rowid: Option<i64>| rowid.is_none_or(|rowid| (1..=MAX_SAFE_INTEGER).contains(&rowid));
    if !positive(request.through_rowid) {
        return WriteAnswer::error(400, THROUGH_NOT_POSITIVE);
    }
    if !positive(request.only_rowid) {
        return WriteAnswer::error(400, ONLY_NOT_POSITIVE);
    }
    let cut = match (request.through_rowid, request.only_rowid) {
        (Some(_), Some(_)) => return WriteAnswer::error(400, BOTH_CUTS),
        (Some(through), None) => Cut::Through(through),
        (None, Some(only)) => Cut::Only(only),
        (None, None) => Cut::Whole,
    };

    let entries = {
        let session_id = session_id.clone();
        match blocking(&inner.reads, "split.messages", move |reads| {
            Ok(reads.get_messages(&session_id, 0)?.entries)
        })
        .await
        {
            Ok(entries) => entries,
            Err(answer) => return answer,
        }
    };
    let Some(range) = cut_range(&entries, &cut) else {
        return WriteAnswer::error(409, NOT_IN_CHAT);
    };
    let format = RenderFormat {
        thinking: request.include_thinking,
        tools: request.include_tools,
    };
    let counts = count(&entries, &range, format);
    if counts.kept == 0 {
        return WriteAnswer::error(409, NOTHING_TO_COPY);
    }

    let title = source
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("chat")
        .to_owned();
    let place = [workspace.repo_name.as_deref(), workspace.branch.as_deref()]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let transcript = transcript_header(&title, &place, &session_id, format, &cut, counts.later)
        + &render_transcript(&entries[range], format);

    let name = format!("Transcript of {title}.md");
    if request.destination == SplitDestination::Workspace {
        let source = ForkSource {
            worktree,
            repo_name,
            repo_root,
            directory_name: workspace.directory_name,
            branch: workspace.branch,
        };
        let prepared = Prepared {
            name,
            transcript,
            counts,
            prompt: request.prompt,
        };
        return split_workspace::into_workspace(&inner, source, prepared, priority).await;
    }
    let written = match tokio::task::spawn_blocking(move || {
        write_attachment(&worktree, &name, transcript.as_bytes(), false)
    })
    .await
    {
        Ok(Ok(written)) => written,
        Ok(Err(error)) => {
            tracing::error!(%error, "a split's transcript could not be written");
            return internal();
        }
        Err(error) => {
            tracing::error!(%error, "a split's transcript write did not finish");
            return internal();
        }
    };
    let attachment = attachment_json(&written, &counts);

    match open_new_chat(&inner, &workspace, priority).await {
        Ok(new_id) => WriteAnswer::json(
            200,
            json!({
                "ok": true,
                "destination": "chat",
                "sessionId": new_id,
                "workspaceId": workspace.id,
                "text": attachment_prompt(&written.token, request.prompt.as_deref()),
                "attachment": attachment,
            }),
        ),
        Err(mut answer) if answer.status == 502 => {
            if let Some(body) = answer.body.as_object_mut() {
                body.insert("destination".to_owned(), json!("chat"));
                body.insert("attachment".to_owned(), attachment);
            }
            answer
        }
        Err(answer) => answer,
    }
}

/// The chat links of a workspace as the phone's `chat_history` object (successor id → link).
pub(crate) fn chat_history(inner: &Arc<Inner>, workspace_id: &str) -> Value {
    let Some(store) = store_of(inner) else {
        return json!({});
    };
    match store.chat_links(workspace_id) {
        Ok(links) => Value::Object(
            links
                .into_iter()
                .map(|link| {
                    let entry = json!({
                        "previousSessionId": link.previous_session_id,
                        "title": link.title,
                        "createdAt": link.created_at,
                    });
                    (link.session_id, entry)
                })
                .collect::<Map<String, Value>>(),
        ),
        Err(error) => {
            tracing::error!(%error, "the chat links of a workspace could not be read");
            json!({})
        }
    }
}
