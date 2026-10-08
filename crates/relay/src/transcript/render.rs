//! A chat as markdown, in Conductor's own transcript layout: the text a fork attaches.
//!
//! An `##` heading per role, prose verbatim under it, and a marker where entries were
//! left out. The heading comes before the marker, so a run of hidden tool calls between a
//! prompt and its answer prints as `## Assistant`, the marker, then the reply.

use super::js;
use super::{TranscriptEntry, TranscriptRole};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderFormat {
    pub thinking: bool,
    pub tools: bool,
}

/// The transcript as the text a fork attaches.
pub fn render_transcript(entries: &[TranscriptEntry], format: RenderFormat) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut pending = Elided::default();
    let mut heading: Option<TranscriptRole> = None;

    for entry in entries {
        // A successful result is the output of the call printed above it: left behind, and
        // not an elision of a call. A failure still prints; it is often why the answer
        // changed course.
        if is_tool_result(entry) && !entry.error {
            continue;
        }
        if entry.role == TranscriptRole::Thinking && !format.thinking {
            pending.thinking += 1;
            continue;
        }
        if entry.role == TranscriptRole::Tool && !format.tools {
            pending.tools += 1;
            continue;
        }
        if heading != Some(entry.role) {
            lines.push(format!("## {}", heading_of(entry.role)));
            heading = Some(entry.role);
        }
        pending.flush(&mut lines);
        lines.push(if entry.role == TranscriptRole::Tool {
            tool_line(entry)
        } else {
            entry.text.clone()
        });
    }
    // Whatever was dropped after the last kept entry is still admitted to.
    pending.flush(&mut lines);

    // Tool lines are a list, so consecutive ones share a paragraph; everything else is
    // separated by a blank line.
    let mut text = String::new();
    for (index, line) in lines.iter().enumerate() {
        text.push_str(line);
        let next_is_item = lines
            .get(index + 1)
            .is_some_and(|next| next.starts_with("- "));
        text.push_str(if line.starts_with("- ") && next_is_item {
            "\n"
        } else {
            "\n\n"
        });
    }
    format!("{}\n", js::trim(&text))
}

/// Entries hidden since the last line written.
#[derive(Default)]
struct Elided {
    thinking: usize,
    tools: usize,
}

impl Elided {
    fn flush(&mut self, lines: &mut Vec<String>) {
        let mut parts = Vec::new();
        if self.tools > 0 {
            parts.push(plural(self.tools, "tool call"));
        }
        if self.thinking > 0 {
            parts.push(plural(self.thinking, "thinking block"));
        }
        *self = Self::default();
        if !parts.is_empty() {
            lines.push(format!("[{} elided]", parts.join(", ")));
        }
    }
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

fn heading_of(role: TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User => "User",
        TranscriptRole::Assistant => "Assistant",
        TranscriptRole::Thinking => "Thinking",
        TranscriptRole::Tool => "Tools",
        TranscriptRole::System => "System",
    }
}

/// A tool entry with no tool name (absent or empty) and an output: the result answering a call.
fn is_tool_result(entry: &TranscriptEntry) -> bool {
    entry.role == TranscriptRole::Tool
        && entry.tool.as_deref().is_none_or(str::is_empty)
        && entry.output.is_some()
}

/// One list line per tool entry: what it did, then what it did it to.
fn tool_line(entry: &TranscriptEntry) -> String {
    if entry.error {
        return format!("- [error] {}", entry.text);
    }
    let tool = entry.tool.as_deref().unwrap_or("tool");
    match entry.detail.as_deref().filter(|detail| !detail.is_empty()) {
        Some(detail) => format!("- [{tool}] {} — `{detail}`", entry.text),
        None => format!("- [{tool}] {}", entry.text),
    }
}
