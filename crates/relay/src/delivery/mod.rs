//! Turns the phone's writes into confirmed actions in Conductor's window: delivery receipts,
//! retries, the duplicate memo, stop and new chat.

pub mod agent;
pub mod attach;
pub mod chats;
pub mod create;
pub mod deliver;
pub mod firstprompt;
pub mod merge;
pub mod parked;
pub mod sendonce;
pub mod service;
pub mod split_workspace;
pub mod workspace_ops;

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::agent::AgentPatch;
use crate::contract::Priority;

/// A boxed future that can be awaited from any task.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The `strategy` every write answer names.
pub const STRATEGY: &str = "accessibility";

/// `POST /api/sessions/:sessionId/prompt`, as the route parsed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendRequest {
    pub session_id: String,
    /// Trimmed and not empty.
    pub text: String,
    pub workspace_id: Option<String>,
    /// The phone's bubble id: repeats of one id are answered from the first send's outcome.
    pub client_id: Option<String>,
    /// Queue behind the running answer (Cmd+Return) instead of sending now.
    pub queue: bool,
    /// `x-client-timeout-ms`, when it parsed as a whole number.
    pub client_timeout_ms: Option<u64>,
    pub priority: Priority,
    /// The staged agent settings to apply before typing; never an empty patch.
    pub agent: Option<AgentPatch>,
}

/// Where a split puts the copied chat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SplitDestination {
    /// Another tab of the same workspace.
    #[default]
    Chat,
    /// A new workspace that carries the source's current code.
    Workspace,
}

/// `POST /api/sessions/:id/split`, as the route parsed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitRequest {
    pub destination: SplitDestination,
    pub workspace_id: Option<String>,
    pub prompt: Option<String>,
    /// Default true.
    pub include_thinking: bool,
    /// Default false.
    pub include_tools: bool,
    pub through_rowid: Option<i64>,
    pub only_rowid: Option<i64>,
}

/// `POST /api/workspaces`, as the route parsed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateRequest {
    pub repo: Option<String>,
    pub prompt: Option<String>,
    /// Default true.
    pub send_immediately: bool,
    pub attachment_ids: Vec<String>,
    /// The agent settings of the new workspace's first chat; never an empty patch.
    pub agent: Option<AgentPatch>,
}

/// What a write answers: the HTTP status, the JSON body, and `retry-after` in seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteAnswer {
    pub status: u16,
    pub body: Value,
    pub retry_after_secs: Option<u32>,
}

impl WriteAnswer {
    pub fn json(status: u16, body: Value) -> WriteAnswer {
        WriteAnswer {
            status,
            body,
            retry_after_secs: None,
        }
    }

    /// `{"error": message}`.
    pub fn error(status: u16, message: &str) -> WriteAnswer {
        WriteAnswer::json(status, json!({ "error": message }))
    }
}

/// The writes the HTTP routes call. `service::Writes` is the real one; route tests use a fake.
pub trait WriteService: Send + Sync + 'static {
    /// Whether this process holds the Accessibility grant.
    fn available(&self) -> bool;
    fn send_prompt(&self, request: SendRequest) -> BoxFuture<WriteAnswer>;
    fn stop_turn(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer>;
    fn new_chat(&self, workspace_id: String, priority: Priority) -> BoxFuture<WriteAnswer>;
    /// Every parked prompt as the phone's `ParkedPrompt` JSON (`parked::parked_json`).
    fn parked_prompts(&self) -> Vec<serde_json::Value>;
    /// `DELETE /api/sessions/:id/prompt`: 200 `{"ok":true}` when any entry of the chat was
    /// dropped, else 404 `{"error":"no parked prompt"}`.
    fn dismiss_parked(&self, session_id: String) -> BoxFuture<WriteAnswer>;
    /// `POST /api/sessions/:id/attachments?workspaceId=`: raw bytes, already capped at 25 MiB;
    /// `name` is "" when the header was missing or undecodable (the service answers 400 after its
    /// workspace checks, as the reference does).
    fn upload_attachment(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        name: String,
        bytes: axum::body::Bytes,
    ) -> BoxFuture<WriteAnswer>;
    /// `POST /api/attachments`.
    fn stage_attachment(&self, name: String, bytes: axum::body::Bytes) -> BoxFuture<WriteAnswer>;
    /// `DELETE /api/attachments/:id`.
    fn discard_staged(&self, stage_id: String) -> BoxFuture<WriteAnswer>;
    /// `POST /api/workspaces/:id/merge`.
    fn merge(&self, workspace_id: String) -> BoxFuture<WriteAnswer>;
    /// `POST /api/sessions/:id/restore` with `{workspaceId?}`.
    fn restore_chat(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer>;
    /// `POST /api/sessions/:id/history` with `{workspaceId, previousSessionId}`.
    fn join_history(
        &self,
        session_id: String,
        workspace_id: String,
        previous_session_id: String,
    ) -> BoxFuture<WriteAnswer>;
    /// `POST /api/sessions/:id/split`: a copy of the chat in another tab of the workspace, or
    /// (destination "workspace") in a new workspace.
    fn split_chat(
        &self,
        session_id: String,
        request: SplitRequest,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer>;
    /// `POST /api/workspaces` with the parsed body.
    fn create_workspace(
        &self,
        request: CreateRequest,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer>;
    /// `DELETE /api/workspaces/:id/prompt`.
    fn dismiss_first_prompt(&self, workspace_id: String) -> BoxFuture<WriteAnswer>;
    /// Every pending first prompt as the phone's `FirstPrompt` JSON, each with `workspaceId`.
    fn pending_prompts(&self) -> Vec<serde_json::Value>;
    /// The chat links of a workspace as the phone's `chat_history` object (successor id → link).
    fn chat_history(&self, workspace_id: &str) -> serde_json::Value;
    /// `POST /api/sessions/:id/agent`.
    fn set_agent(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        patch: AgentPatch,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        let _ = (session_id, workspace_id, patch, priority);
        Box::pin(async { WriteAnswer::error(501, "not implemented") })
    }
    /// `GET /api/sessions/:id/models?workspaceId=`.
    fn list_models(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        let _ = (session_id, workspace_id, priority);
        Box::pin(async { WriteAnswer::error(501, "not implemented") })
    }
    /// `DELETE /api/sessions/:id` with `{workspaceId?, closeRunning?}`.
    fn close_chat(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        close_running: bool,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        let _ = (session_id, workspace_id, close_running, priority);
        Box::pin(async { WriteAnswer::error(501, "not implemented") })
    }
    /// `POST /api/workspaces/:id/status` with `{status}`.
    fn set_workspace_status(
        &self,
        workspace_id: String,
        status: String,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        let _ = (workspace_id, status, priority);
        Box::pin(async { WriteAnswer::error(501, "not implemented") })
    }
    /// `POST /api/workspaces/:id/archive` with `{stopAgents?}`.
    fn archive_workspace(
        &self,
        workspace_id: String,
        stop_agents: bool,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        let _ = (workspace_id, stop_agents, priority);
        Box::pin(async { WriteAnswer::error(501, "not implemented") })
    }
    /// `POST /api/workspaces/:id/continue` with `{sessionId?}`.
    fn continue_workspace(
        &self,
        workspace_id: String,
        session_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        let _ = (workspace_id, session_id, priority);
        Box::pin(async { WriteAnswer::error(501, "not implemented") })
    }
}
