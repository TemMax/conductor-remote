//! The rows the parser reads and the entries it returns.

use serde::Serialize;

/// The columns of a stored message the parser reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
    pub rowid: i64,
    pub id: String,
    pub content: Option<String>,
    pub created_at: Option<String>,
    pub sent_at: Option<String>,
    pub queue_order: Option<i64>,
}

/// The columns of a queue (outbox) row the parser reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredOutboxMessage {
    pub message_id: String,
    /// JSON; only its `message` field is read.
    pub delivery_payload: Option<String>,
    pub created_at: Option<String>,
}

/// How the phone displays an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptRole {
    User,
    Assistant,
    Tool,
    Thinking,
    System,
}

/// One renderable piece of a chat. Its JSON is the `TranscriptEntry` of the phone app:
/// optional fields are left out when unset, never written as `null` or `false`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEntry {
    pub id: String,
    /// The row the entry came from; 0 for a queue row, which has no transcript row yet.
    pub rowid: i64,
    pub role: TranscriptRole,
    /// Human-readable text. For a tool call: its description, else the tool name.
    pub text: String,
    /// Tool name; set on a tool call and absent on a tool result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The primary input of a tool call (command, path, pattern and the like), unclipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The id that pairs a tool call with the result answering it, which sits in a later row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// The agent tool call whose subagent wrote this frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// Human label of a tool call that spawned a subagent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_label: Option<String>,
    /// A tool result's output, clipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// True when `output` is a unified diff.
    #[serde(skip_serializing_if = "is_false")]
    pub diff: bool,
    /// The images a result carried, as `<rowid>.<n>` references.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// True on a failed tool result.
    #[serde(skip_serializing_if = "is_false")]
    pub error: bool,
    /// `created_at` as stored.
    pub ts: String,
    /// True while the message waits in the queue.
    pub queued: bool,
}

/// Takes a reference because that is how serde's `skip_serializing_if` calls it.
fn is_false(value: &bool) -> bool {
    !*value
}
