//! How a chat's context divides into initial context, chat, thinking and tools.
//!
//! Conductor stores the provider's exact total but not how it divides. The stored frames
//! still mark the parts that matter — visible prose, reasoning, tool traffic — so those
//! three are estimated from their UTF-8 size, and the rest of the total is the initial
//! context: prompts, tool definitions, attachments and compaction summaries.

use serde_json::{Map, Value};

use super::js::{self, Replacement};

const BYTES_PER_TOKEN: i64 = 4;
const BINARY: &str = "[binary data]";

/// How a chat's context divides, in tokens. The four always sum to the total they were fitted to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ContextCategories {
    pub initial: i64,
    pub chat: i64,
    pub thinking: i64,
    pub tools: i64,
}

/// Byte sums of the three measured categories.
#[derive(Clone, Copy, Debug, Default)]
struct Bytes {
    chat: i64,
    thinking: i64,
    tools: i64,
}

impl Bytes {
    fn add(&mut self, other: Bytes) {
        self.chat += other.chat;
        self.thinking += other.thinking;
        self.tools += other.tools;
    }
}

/// Reads a chat's rows once, in row order, in constant memory.
///
/// The counted window is the rows after the last root compaction boundary up to the
/// completed cut, the last root `result` frame (or the last row when there is none). One
/// forward pass finds it with two sums: the bytes since the latest boundary, reset at each
/// boundary, and a snapshot of that sum taken at each root `result`. At the end the latest
/// snapshot is the answer when there is one; otherwise the cut is the last row and the
/// running sum is.
#[derive(Debug, Default)]
pub struct ContextAccumulator {
    since_boundary: Bytes,
    boundary_seen: bool,
    completed: Option<(Bytes, bool)>,
}

impl ContextAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// `role` and `content` are the columns of one `session_messages` row.
    pub fn push(&mut self, role: Option<&str>, content: Option<&str>) {
        let content = content.unwrap_or("");
        let frame = parse_frame(content);
        let root_type = frame
            .as_ref()
            .filter(|frame| is_root(frame))
            .and_then(frame_type);
        if root_type == Some("system")
            && frame
                .as_ref()
                .and_then(|frame| js::property(frame, "subtype"))
                .and_then(Value::as_str)
                == Some("compact_boundary")
        {
            self.since_boundary = Bytes::default();
            self.boundary_seen = true;
            return;
        }
        self.since_boundary
            .add(row_bytes(role, content, frame.as_ref()));
        if root_type == Some("result") {
            self.completed = Some((self.since_boundary, self.boundary_seen));
        }
    }

    /// The categories fitted to `total_tokens`, and whether the counted window follows a compaction.
    pub fn finish(self, total_tokens: i64) -> (ContextCategories, bool) {
        let (bytes, compacted) = self
            .completed
            .unwrap_or((self.since_boundary, self.boundary_seen));
        (fit(bytes, total_tokens.max(0)), compacted)
    }
}

/// `ceil(utf8 bytes / 4)`; 0 for an empty string.
pub fn estimate_text_tokens(text: &str) -> i64 {
    tokens(byte_len(text))
}

fn tokens(bytes: i64) -> i64 {
    (bytes + BYTES_PER_TOKEN - 1) / BYTES_PER_TOKEN
}

fn byte_len(text: &str) -> i64 {
    i64::try_from(text.len()).unwrap_or(i64::MAX)
}

/// The row as a frame, or `None` when it is not one. A human prompt may be JSON too, so a
/// frame has to be a message frame with typed content blocks or one of the bookkeeping types.
pub(crate) fn parse_frame(content: &str) -> Option<Value> {
    if !content.starts_with('{') {
        return None;
    }
    let frame = js::parse(content)?;
    let recognised = match frame_type(&frame) {
        Some("assistant" | "user") => message_blocks(&frame).is_some(),
        Some("system" | "result" | "error") => true,
        _ => false,
    };
    (frame.is_object() && recognised).then_some(frame)
}

pub(crate) fn frame_type(frame: &Value) -> Option<&str> {
    js::property(frame, "type").and_then(Value::as_str)
}

pub(crate) fn message_blocks(frame: &Value) -> Option<&Vec<Value>> {
    js::property(frame, "message")
        .and_then(|message| js::property(message, "content"))
        .and_then(Value::as_array)
}

/// A frame the chat's own agent wrote, not one copied in from a subagent.
pub(crate) fn is_root(frame: &Value) -> bool {
    !js::is_truthy(js::property(frame, "parent_tool_use_id"))
}

/// What one row adds to the measured categories.
fn row_bytes(role: Option<&str>, content: &str, frame: Option<&Value>) -> Bytes {
    let mut bytes = Bytes::default();
    let Some(frame) = frame else {
        // A plain prompt. A malformed frame is no evidence of a category.
        if role == Some("user") || !content.starts_with('{') {
            bytes.chat += byte_len(content);
        }
        return bytes;
    };
    // A subagent's frames are mirrored into its parent for display; the parent's model
    // only saw its own call and the summary that came back.
    if !is_root(frame) {
        return bytes;
    }
    let Some(blocks) = message_blocks(frame) else {
        return bytes;
    };
    let user_frame = frame_type(frame) == Some("user");
    for block in blocks.iter().filter(|block| block.is_object()) {
        let kind = js::property(block, "type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind == "tool_use" || kind == "tool_result" {
            bytes.tools += block_bytes(block);
        } else if kind.contains("thinking") || kind.contains("reasoning") {
            bytes.thinking += block_bytes(block);
        } else if (kind == "text" || kind == "output_text") && !user_frame {
            bytes.chat += block_bytes(block);
        }
    }
    bytes
}

/// The UTF-8 length of the block's JSON text, without opaque signatures and with binary
/// payloads replaced by a short marker: a provider does not count those as text, and one
/// base64 screenshot would otherwise read as hundreds of thousands of tokens.
pub(crate) fn block_bytes(block: &Value) -> i64 {
    js::stringify_with(block, &mut visible).map_or(0, |json| byte_len(&json))
}

/// The replacer `block_bytes` stringifies with.
fn visible(holder: Option<&Map<String, Value>>, key: &str, value: &Value) -> Replacement {
    if key == "signature" || key == "encrypted_content" {
        return Replacement::Omit;
    }
    let Some(text) = value.as_str() else {
        return Replacement::Keep;
    };
    let field = |name: &str| holder.and_then(|holder| holder.get(name));
    if starts_with_data_uri(text) {
        return binary();
    }
    if key == "data"
        && field("type")
            .map(js::to_js_string)
            .is_some_and(|kind| matches!(kind.as_str(), "base64" | "image" | "audio"))
    {
        return binary();
    }
    if key == "blob" && field("mimeType").is_some_and(Value::is_string) {
        return binary();
    }
    // A tool result often holds a serialized JSON result whose images are still binary.
    // The re-serialized text replaces the original only when something was stripped.
    if key == "content"
        && field("type").and_then(Value::as_str) == Some("tool_result")
        && (names_opaque_key(text) || contains_data_uri(text))
    {
        if let Some(parsed) = js::parse(text) {
            let mut changed = false;
            let cleaned = js::stringify_with(&parsed, &mut |holder, key, value| {
                let replacement = visible(holder, key, value);
                match &replacement {
                    Replacement::Keep => {}
                    Replacement::Omit => changed = true,
                    Replacement::With(other) => changed |= other != value,
                }
                replacement
            });
            if changed {
                return match cleaned {
                    Some(cleaned) => Replacement::With(Value::String(cleaned)),
                    None => Replacement::Omit,
                };
            }
        }
    }
    Replacement::Keep
}

fn binary() -> Replacement {
    Replacement::With(Value::String(BINARY.to_owned()))
}

/// `/^data:[^;,]+;base64,/i`.
fn starts_with_data_uri(text: &str) -> bool {
    let Some(rest) = strip_prefix_ignore_case(text, "data:") else {
        return false;
    };
    match rest.find([';', ',']) {
        Some(end) if end > 0 => strip_prefix_ignore_case(&rest[end..], ";base64,").is_some(),
        _ => false,
    }
}

/// `/data:[^;,]+;base64,/i` anywhere in the text.
fn contains_data_uri(text: &str) -> bool {
    text.char_indices()
        .any(|(at, c)| c.eq_ignore_ascii_case(&'d') && starts_with_data_uri(&text[at..]))
}

fn strip_prefix_ignore_case<'t>(text: &'t str, prefix: &str) -> Option<&'t str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// `/"(?:data|blob|signature|encrypted_content)"\s*:/`.
fn names_opaque_key(text: &str) -> bool {
    [
        "\"data\"",
        "\"blob\"",
        "\"signature\"",
        "\"encrypted_content\"",
    ]
    .iter()
    .any(|quoted| {
        text.match_indices(quoted)
            .any(|(at, _)| js::trim_start(&text[at + quoted.len()..]).starts_with(':'))
    })
}

/// The measured bytes as tokens, fitted to the total. When the estimate fits, the rest of
/// the total is the initial context. When the byte heuristic overshoots, the three are
/// scaled down to the total in proportion, floored, and the rounding remainder is handed
/// out one token at a time by largest fraction; the initial context is then 0.
fn fit(bytes: Bytes, total: i64) -> ContextCategories {
    let estimated = [
        tokens(bytes.chat),
        tokens(bytes.thinking),
        tokens(bytes.tools),
    ];
    let visible: i64 = estimated.iter().sum();
    if visible <= total {
        return ContextCategories {
            initial: total - visible,
            chat: estimated[0],
            thinking: estimated[1],
            tools: estimated[2],
        };
    }
    if total == 0 {
        return ContextCategories::default();
    }

    // The arithmetic is JavaScript's: doubles, floored, then the remainder one at a time.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let mut portions: Vec<(usize, i64, f64)> = {
        let scale = total as f64 / visible as f64;
        estimated
            .iter()
            .enumerate()
            .map(|(index, &estimate)| {
                let exact = estimate as f64 * scale;
                (index, exact.floor() as i64, exact - exact.floor())
            })
            .collect()
    };
    let mut left = total - portions.iter().map(|(_, value, _)| value).sum::<i64>();
    // A stable sort, as JavaScript's is, so equal fractions keep chat, thinking, tools order.
    portions.sort_by(|a, b| b.2.total_cmp(&a.2));
    for portion in &mut portions {
        if left <= 0 {
            break;
        }
        left -= 1;
        portion.1 += 1;
    }
    portions.sort_by_key(|(index, _, _)| *index);
    ContextCategories {
        initial: 0,
        chat: portions[0].1,
        thinking: portions[1].1,
        tools: portions[2].1,
    }
}
