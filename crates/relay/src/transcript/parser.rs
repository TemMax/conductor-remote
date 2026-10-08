//! Turns a stored chat row into the entries the phone renders.
//!
//! Conductor stores each turn's raw agent stream as one JSON frame per row; a prompt the
//! user typed is stored as plain text. A frame of type `user` is never the user: it is tool
//! plumbing that carries tool results. Only a plain-text row becomes a user entry.

use serde_json::Value;

use super::entry::{StoredMessage, StoredOutboxMessage, TranscriptEntry, TranscriptRole};
use super::js;

/// How much of a tool's output travels to the phone, in UTF-16 code units. The first fetch
/// of a chat carries its whole backlog, so this cap is the only size control.
const MAX_OUTPUT_UNITS: usize = 2000;
/// How much of a row that is not a known frame is shown, in UTF-16 code units.
const MAX_RAW_UNITS: usize = 200;

/// The inputs of a tool call that can serve as its detail, in order of preference.
const DETAIL_KEYS: [&str; 7] = [
    "command",
    "file_path",
    "path",
    "pattern",
    "url",
    "skill",
    "prompt",
];

/// The `\uXXXX` escape that starts at `at`, as its code unit.
fn unicode_escape(bytes: &[u8], at: usize) -> Option<u16> {
    let digits = bytes.get(at..at + 6)?;
    if digits[0] != b'\\' || digits[1] != b'u' {
        return None;
    }
    let hex = std::str::from_utf8(&digits[2..]).ok()?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(hex, 16).ok()
}

/// JavaScript strings may hold an unpaired surrogate, which JSON writes as an escape that
/// `serde_json` refuses. Replaces each such escape (a high surrogate not directly followed
/// by a low one, a low surrogate not directly preceded by a high one) with `\ufffd`, the
/// stand-in `js::clip` uses. An escaped backslash is skipped whole, so `\\ud83d` is text.
pub(super) fn replace_lone_surrogates(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        match unicode_escape(bytes, i) {
            Some(0xD800..=0xDBFF)
                if matches!(unicode_escape(bytes, i + 6), Some(0xDC00..=0xDFFF)) =>
            {
                i += 12;
            }
            Some(0xD800..=0xDFFF) => {
                out.push_str(&text[copied..i]);
                out.push_str("\\ufffd");
                i += 6;
                copied = i;
            }
            Some(_) => i += 6,
            None => i += 2,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// One durable row becomes zero or more entries. `worktree` is the chat's worktree
/// directory, used to shorten paths in tool details.
pub fn parse_message(row: &StoredMessage, worktree: Option<&str>) -> Vec<TranscriptEntry> {
    let content = row.content.as_deref().unwrap_or("");
    let base = TranscriptEntry {
        id: row.id.clone(),
        rowid: row.rowid,
        role: TranscriptRole::System,
        text: String::new(),
        tool: None,
        question: None,
        detail: None,
        tool_use_id: None,
        parent_tool_use_id: None,
        subagent_label: None,
        output: None,
        diff: false,
        images: Vec::new(),
        error: false,
        ts: row.created_at.clone().unwrap_or_default(),
        queued: row.queue_order.is_some() && row.sent_at.is_none(),
    };

    // A plain prompt: the only source of user entries.
    if !content.starts_with('{') {
        if js::trim(content).is_empty() {
            return Vec::new();
        }
        return vec![TranscriptEntry {
            role: TranscriptRole::User,
            text: content.to_owned(),
            ..base
        }];
    }

    let Some(frame) = serde_json::from_str::<Value>(content)
        .ok()
        .or_else(|| serde_json::from_str::<Value>(&replace_lone_surrogates(content)).ok())
    else {
        return vec![TranscriptEntry {
            text: js::clip(content, MAX_RAW_UNITS),
            ..base
        }];
    };
    let frame_type = js::property(&frame, "type").and_then(Value::as_str);
    let base = TranscriptEntry {
        parent_tool_use_id: js::non_blank(js::property(&frame, "parent_tool_use_id"))
            .map(str::to_owned),
        ..base
    };

    if frame_type == Some("assistant") {
        if let Some(question) = super::questions::codex_request(&frame) {
            return vec![TranscriptEntry {
                id: format!("question:codex:{}", question.id),
                role: TranscriptRole::Assistant,
                text: question
                    .questions
                    .iter()
                    .map(|q| q.question.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                question: Some(question),
                ..base
            }];
        }
    }

    // Bookkeeping frames: hooks, init, token accounting, the end of a turn.
    if matches!(frame_type, Some("system" | "result")) {
        return Vec::new();
    }

    // How a stopped turn ends: an error frame whose own wording is shown as it is.
    if frame_type == Some("error") {
        if let Some(said) = js::non_blank(js::property(&frame, "content")) {
            return vec![TranscriptEntry {
                text: js::clip(said, MAX_RAW_UNITS),
                ..base
            }];
        }
    }

    let blocks = js::property(&frame, "message")
        .and_then(|message| js::property(message, "content"))
        .and_then(Value::as_array);
    let Some(blocks) = blocks else {
        if matches!(frame_type, Some("user" | "assistant")) {
            return Vec::new();
        }
        // An unknown frame shape: a raw dump keeps a change in Conductor's format visible.
        return vec![TranscriptEntry {
            text: js::clip(content, MAX_RAW_UNITS),
            ..base
        }];
    };

    let mut row_entries = RowEntries {
        row_id: &row.id,
        base,
        entries: Vec::new(),
        pending: Vec::new(),
    };
    // Images are numbered per row, across every result the row holds.
    let mut image_index = 0_usize;

    for block in blocks {
        let block_type = js::property(block, "type").and_then(Value::as_str);
        match block_type {
            Some("text") => {
                // Text inside a `user` frame is injected context, not something anyone said.
                if let Some(text) = js::property(block, "text").and_then(Value::as_str) {
                    if frame_type != Some("user") {
                        row_entries.pending.push(text);
                    }
                }
            }
            Some("thinking") => {
                row_entries.flush();
                let text = js::non_blank(js::property(block, "thinking"))
                    .or_else(|| js::non_blank(js::property(block, "text")));
                if let Some(text) = text {
                    row_entries.push(|entry| {
                        entry.role = TranscriptRole::Thinking;
                        entry.text = text.to_owned();
                    });
                }
            }
            Some("tool_use") => {
                let Some(name) = js::property(block, "name").and_then(Value::as_str) else {
                    continue;
                };
                row_entries.flush();
                let input = js::property(block, "input");
                let (text, detail) = summarize_tool_use(name, input, worktree);
                row_entries.push(|entry| {
                    entry.role = TranscriptRole::Tool;
                    entry.tool = Some(name.to_owned());
                    entry.tool_use_id = js::non_blank(js::property(block, "id")).map(str::to_owned);
                    entry.subagent_label = subagent_label(name, input);
                    entry.text = text;
                    entry.detail = detail;
                    entry.question = super::questions::claude_request(block);
                });
            }
            Some("tool_result") => {
                // A result sits in a later row than the call it answers, so it travels as an
                // entry of its own that names the call. Only a failure repeats its output as
                // `text`: for a success that would send the largest thing here twice.
                row_entries.flush();
                let read = result_output(js::property(block, "content"), worktree);
                let images: Vec<String> = (0..read.images)
                    .map(|_| {
                        let reference = format!("{}.{image_index}", row.rowid);
                        image_index += 1;
                        reference
                    })
                    .collect();
                let failed = js::is_truthy(js::property(block, "is_error"));
                let output = if read.text.is_empty() && failed {
                    "(tool error)".to_owned()
                } else {
                    read.text
                };
                if output.is_empty() && images.is_empty() {
                    continue;
                }
                row_entries.push(|entry| {
                    entry.role = TranscriptRole::Tool;
                    entry.text = if failed {
                        output.clone()
                    } else {
                        String::new()
                    };
                    entry.output = Some(output);
                    entry.tool_use_id =
                        js::non_blank(js::property(block, "tool_use_id")).map(str::to_owned);
                    entry.diff = read.diff;
                    entry.images = images;
                    entry.error = failed;
                });
            }
            _ => {}
        }
    }
    row_entries.flush();
    row_entries.entries
}

/// One row of the queue (outbox) table becomes an entry, or nothing when it cannot be rendered.
///
/// A queue row has no transcript row yet, so its `rowid` is 0; its id stays the same when
/// Conductor dispatches it and the durable row appears.
pub fn parse_outbox_message(row: &StoredOutboxMessage) -> Option<TranscriptEntry> {
    let payload: Value = serde_json::from_str(row.delivery_payload.as_deref()?).ok()?;
    let text = js::non_blank(js::property(&payload, "message"))?;
    Some(TranscriptEntry {
        id: row.message_id.clone(),
        rowid: 0,
        role: TranscriptRole::User,
        text: text.to_owned(),
        tool: None,
        question: None,
        detail: None,
        tool_use_id: None,
        parent_tool_use_id: None,
        subagent_label: None,
        output: None,
        diff: false,
        images: Vec::new(),
        error: false,
        ts: row.created_at.clone().unwrap_or_default(),
        queued: true,
    })
}

/// The entries of one frame while its blocks are read.
struct RowEntries<'a> {
    row_id: &'a str,
    /// What every entry of the frame shares.
    base: TranscriptEntry,
    entries: Vec<TranscriptEntry>,
    /// Assistant prose not yet turned into an entry.
    pending: Vec<&'a str>,
}

impl RowEntries<'_> {
    /// Adds an entry. Its id counts the entries already added for the row, not the blocks read.
    fn push(&mut self, fill: impl FnOnce(&mut TranscriptEntry)) {
        let mut entry = self.base.clone();
        entry.id = format!("{}:{}", self.row_id, self.entries.len());
        fill(&mut entry);
        self.entries.push(entry);
    }

    /// Turns the prose collected so far into one assistant entry.
    fn flush(&mut self) {
        let joined = self.pending.join("\n");
        self.pending.clear();
        let text = js::trim(&joined);
        if !text.is_empty() {
            self.push(|entry| {
                entry.role = TranscriptRole::Assistant;
                entry.text = text.to_owned();
            });
        }
    }
}

/// Makes a tool detail relative to the worktree: an absolute path wastes a phone's line.
fn strip_worktree(s: &str, worktree: Option<&str>) -> String {
    let Some(worktree) = worktree.filter(|worktree| !worktree.is_empty()) else {
        return s.to_owned();
    };
    // Conductor starts a command with `cd <worktree>`, joined by `&&` or a newline.
    let rest = match s.strip_prefix("cd ").and_then(|s| s.strip_prefix(worktree)) {
        Some(after) => {
            let after = js::trim_start(after);
            js::trim_start(after.strip_prefix("&&").unwrap_or(after))
        }
        None => s,
    };
    rest.replace(&format!("{worktree}/"), "")
        .replace(worktree, ".")
}

/// Whether JavaScript's `typeof` calls the value an object, which a tool input must be.
fn is_object_like(value: &Value) -> bool {
    matches!(value, Value::Object(_) | Value::Array(_))
}

/// The title and the detail of a tool call: its description, else the tool name, and its
/// primary input. A tool without a recognisable primary input gets the title alone.
fn summarize_tool_use(
    name: &str,
    input: Option<&Value>,
    worktree: Option<&str>,
) -> (String, Option<String>) {
    let Some(input) = input.filter(|input| is_object_like(input)) else {
        return (name.to_owned(), None);
    };
    let text = js::non_blank(js::property(input, "description")).unwrap_or(name);
    let detail = DETAIL_KEYS
        .iter()
        .find_map(|key| js::non_blank(js::property(input, key)))
        .filter(|detail| *detail != text)
        .map(|detail| strip_worktree(detail, worktree));
    (text.to_owned(), detail)
}

/// A label for a tool call that spawns a subagent, in either shape Conductor writes.
fn subagent_label(name: &str, input: Option<&Value>) -> Option<String> {
    let input = input.filter(|input| is_object_like(input))?;
    let field = |key: &str| js::non_blank(js::property(input, key));

    if name == "Agent" || name == "Task" {
        let label = field("description")
            .or_else(|| field("subagent_type"))
            .unwrap_or("Subagent");
        return Some(label.to_owned());
    }
    if !is_spawn_agent(name) {
        return None;
    }
    let raw = field("agent_path")
        .and_then(|path| path.split('/').rfind(|segment| !segment.is_empty()))
        .or_else(|| field("task_name"))
        .or_else(|| field("agent_nickname"));
    let Some(raw) = raw else {
        return Some("Subagent".to_owned());
    };
    let words = collapse_separators(raw);
    let words = js::trim(&words);
    if words.is_empty() {
        return Some("Subagent".to_owned());
    }
    Some(js::uppercase_first(words))
}

/// Whether a tool name ends in `spawn_agent` or `spawnagent`, in any ASCII case, at the
/// start of the name or after one of `_`, `.`, `:`.
fn is_spawn_agent(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["spawn_agent", "spawnagent"].iter().any(|suffix| {
        lower
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with(['_', '.', ':']))
    })
}

/// Replaces every run of `-` and `_` with one space.
fn collapse_separators(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        let separator = c == '-' || c == '_';
        if separator && !in_run {
            out.push(' ');
        } else if !separator {
            out.push(c);
        }
        in_run = separator;
    }
    out
}

/// What one tool result says.
struct ResultOutput {
    text: String,
    /// `text` is a unified diff.
    diff: bool,
    /// How many image blocks the result carried.
    images: usize,
}

/// The text of a result: error tags removed, trimmed, clipped.
fn plain(text: &str) -> String {
    js::clip(js::trim(&without_error_tags(text)), MAX_OUTPUT_UNITS)
}

/// Removes every `<tool_use_error>` and `</tool_use_error>` tag in one pass, so that a tag
/// formed by a removal is not itself removed.
fn without_error_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find('<') {
        out.push_str(&rest[..position]);
        let tail = &rest[position..];
        let after_tag = tail
            .strip_prefix("<tool_use_error>")
            .or_else(|| tail.strip_prefix("</tool_use_error>"));
        rest = match after_tag {
            Some(after) => after,
            None => {
                out.push('<');
                &tail[1..]
            }
        };
    }
    out.push_str(rest);
    out
}

/// Reads a tool result in whatever shape the tool answered: a string, a list of blocks,
/// Conductor's edit result, or something unknown, which falls back to its own JSON rather
/// than to silence.
fn result_output(content: Option<&Value>, worktree: Option<&str>) -> ResultOutput {
    let (text, diff, images) = match content {
        Some(Value::String(text)) => (plain(text), false, 0),
        Some(Value::Array(blocks)) => {
            let mut said = String::new();
            let mut tools: Vec<&str> = Vec::new();
            let mut images = 0;
            for block in blocks {
                if !is_object_like(block) {
                    continue;
                }
                if js::property(block, "type").and_then(Value::as_str) == Some("image") {
                    images += 1;
                } else if let Some(tool) = js::non_blank(js::property(block, "tool_name")) {
                    tools.push(tool);
                } else if let Some(text) = js::property(block, "text").and_then(Value::as_str) {
                    said.push_str(text);
                }
            }
            // A list of tool references is the whole answer of a tool search: name them.
            if !tools.is_empty() {
                let noun = if tools.len() == 1 { "tool" } else { "tools" };
                said.push_str(&format!("{} {noun}: {}", tools.len(), tools.join(", ")));
            }
            (plain(&said), false, images)
        }
        Some(content @ Value::Object(_)) => {
            match js::non_blank(js::property(content, "diffString")) {
                // Conductor's edit result: the status names the file, above the hunks.
                Some(patch) => {
                    let head = js::non_blank(js::property(content, "status"))
                        .map(|status| format!("{}\n", strip_worktree(status, worktree)))
                        .unwrap_or_default();
                    (plain(&format!("{head}{patch}")), true, 0)
                }
                None => (plain(&js::stringify(content)), false, 0),
            }
        }
        _ => (String::new(), false, 0),
    };
    ResultOutput { text, diff, images }
}
