//! The write routes: send a prompt, stop a turn, start a new chat, merge, restore, join history,
//! split, create a workspace, dismiss a first prompt and set a chat's agent options
//! (`POST /api/sessions/:id/agent`), plus the workspace actions (`DELETE /api/sessions/:id` closes a
//! chat; `POST /api/workspaces/:id/status`, `/archive` and `/continue`), the model list of a chat
//! (`GET /api/sessions/:id/models`) and the preference routes. Each validates the request and
//! hands it to the `WriteService` (or the `PrefsService`); the service's status, body and
//! `retry-after` are the answer.

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use serde_json::{json, Map, Value};

use super::respond::match_param;
use super::{error_body, json};
use crate::agent::AgentPatch;
use crate::contract::{AppState, Priority};
use crate::delivery::{CreateRequest, SendRequest, SplitDestination, SplitRequest, WriteAnswer};

const CLIENT_HEADER: &str = "x-relay-client";
const TIMEOUT_HEADER: &str = "x-client-timeout-ms";
const PREFS: &str = "/api/prefs";
const WORKSPACES: &str = "/api/workspaces";
/// The largest row id the phone can hold exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The `POST` write routes, by their path.
enum Target {
    Prompt(String),
    Stop(String),
    NewChat(String),
    Merge(String),
    Restore(String),
    History(String),
    Split(String),
    Agent(String),
    Status(String),
    Archive(String),
    Continue(String),
    CreateWorkspace,
}

impl Target {
    fn parse(path: &str) -> Option<Target> {
        if path == WORKSPACES {
            return Some(Target::CreateWorkspace);
        }
        match_param(path, "/api/sessions/", "/prompt")
            .map(Target::Prompt)
            .or_else(|| match_param(path, "/api/sessions/", "/stop").map(Target::Stop))
            .or_else(|| match_param(path, "/api/workspaces/", "/sessions").map(Target::NewChat))
            .or_else(|| match_param(path, "/api/workspaces/", "/merge").map(Target::Merge))
            .or_else(|| match_param(path, "/api/sessions/", "/restore").map(Target::Restore))
            .or_else(|| match_param(path, "/api/sessions/", "/history").map(Target::History))
            .or_else(|| match_param(path, "/api/sessions/", "/split").map(Target::Split))
            .or_else(|| match_param(path, "/api/sessions/", "/agent").map(Target::Agent))
            .or_else(|| match_param(path, "/api/workspaces/", "/status").map(Target::Status))
            .or_else(|| match_param(path, "/api/workspaces/", "/archive").map(Target::Archive))
            .or_else(|| match_param(path, "/api/workspaces/", "/continue").map(Target::Continue))
    }
}

/// The two dismiss routes, by their path.
enum Dismiss {
    /// A parked prompt of a chat.
    Session(String),
    /// The first-prompt queue of a workspace.
    Workspace(String),
}

impl Dismiss {
    fn parse(path: &str) -> Option<Dismiss> {
        match_param(path, "/api/sessions/", "/prompt")
            .map(Dismiss::Session)
            .or_else(|| match_param(path, "/api/workspaces/", "/prompt").map(Dismiss::Workspace))
    }
}

/// The session of a `DELETE /api/sessions/:id` path: one non-empty segment, no `/`.
fn close_session(path: &str) -> Option<String> {
    match_param(path, "/api/sessions/", "")
}

/// Whether the request is a `POST` on one of the write paths, a `DELETE` on one of the dismiss
/// paths or on a session path, a `GET` on the models path, or a `GET` or `PATCH` on the preferences path.
pub fn is_write(method: &Method, path: &str) -> bool {
    match *method {
        Method::POST => Target::parse(path).is_some(),
        Method::DELETE => Dismiss::parse(path).is_some() || close_session(path).is_some(),
        Method::GET => path == PREFS || models_session(path).is_some(),
        Method::PATCH => path == PREFS,
        _ => false,
    }
}

/// The session of a `GET /api/sessions/:id/models` path.
fn models_session(path: &str) -> Option<String> {
    match_param(path, "/api/sessions/", "/models")
}

/// Whether the route reads a request body: the `POST`s, the close-chat `DELETE` and the
/// preferences `PATCH`. A merge, the dismiss `DELETE`s and a `GET` carry nothing worth reading.
pub fn reads_body(method: &Method, path: &str) -> bool {
    match *method {
        Method::POST => !matches!(Target::parse(path), Some(Target::Merge(_))),
        Method::DELETE => close_session(path).is_some(),
        Method::PATCH => path == PREFS,
        _ => false,
    }
}

/// Answers a write route, or `None` when the request is not one. The token has been checked.
pub async fn route(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Option<Response> {
    if method == Method::DELETE {
        if let Some(response) = dismiss(state, uri.path()).await {
            return Some(response);
        }
        return close(state, uri.path(), headers, &body).await;
    }
    if uri.path() == PREFS {
        return match *method {
            Method::GET => Some(prefs_get(state)),
            Method::PATCH => Some(prefs_patch(state, &body)),
            _ => None,
        };
    }
    if method == Method::GET {
        let session_id = models_session(uri.path())?;
        let Some(writes) = state.writes.clone() else {
            return Some(failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "writes are unavailable",
            ));
        };
        let workspace_id = query_workspace(uri.query());
        return Some(respond(
            writes
                .list_models(session_id, workspace_id, priority_of(headers))
                .await,
        ));
    }
    if method != Method::POST {
        return None;
    }
    let target = Target::parse(uri.path())?;
    let Some(writes) = state.writes.clone() else {
        return Some(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "writes are unavailable",
        ));
    };
    let priority = priority_of(headers);

    let answer = match target {
        Target::Prompt(session_id) => {
            let request = match prompt_request(session_id, headers, &body, priority) {
                Ok(request) => request,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes.send_prompt(request).await
        }
        Target::Stop(session_id) => {
            let workspace_id = match stop_workspace(&body) {
                Ok(workspace_id) => workspace_id,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes.stop_turn(session_id, workspace_id, priority).await
        }
        Target::NewChat(workspace_id) => writes.new_chat(workspace_id, priority).await,
        Target::Merge(workspace_id) => writes.merge(workspace_id).await,
        Target::Restore(session_id) => {
            let workspace_id = match stop_workspace(&body) {
                Ok(workspace_id) => workspace_id,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes
                .restore_chat(session_id, workspace_id, priority)
                .await
        }
        Target::History(session_id) => {
            let (workspace_id, previous_session_id) = match history_fields(&body) {
                Ok(fields) => fields,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes
                .join_history(session_id, workspace_id, previous_session_id)
                .await
        }
        Target::Split(session_id) => {
            let request = match split_request(&body) {
                Ok(request) => request,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes.split_chat(session_id, request, priority).await
        }
        Target::Agent(session_id) => {
            let (workspace_id, patch) = match agent_fields(&body) {
                Ok(fields) => fields,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes
                .set_agent(session_id, workspace_id, patch, priority)
                .await
        }
        Target::Status(workspace_id) => {
            let status = match status_field(&body) {
                Ok(status) => status,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes
                .set_workspace_status(workspace_id, status, priority)
                .await
        }
        Target::Archive(workspace_id) => {
            let stop_agents = match flag_field(&body, "stopAgents") {
                Ok(stop_agents) => stop_agents,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes
                .archive_workspace(workspace_id, stop_agents, priority)
                .await
        }
        Target::Continue(workspace_id) => {
            let session_id = match string_field(&body, "sessionId") {
                Ok(session_id) => session_id,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes
                .continue_workspace(workspace_id, session_id, priority)
                .await
        }
        Target::CreateWorkspace => {
            let request = match create_request(&body) {
                Ok(request) => request,
                Err(refusal) => return Some(refusal.into_response()),
            };
            writes.create_workspace(request, priority).await
        }
    };
    Some(respond(answer))
}

/// `DELETE` on a prompt path: the service drops a chat's parked prompt or a workspace's first
/// prompt.
async fn dismiss(state: &AppState, path: &str) -> Option<Response> {
    let target = Dismiss::parse(path)?;
    let Some(writes) = state.writes.clone() else {
        return Some(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "writes are unavailable",
        ));
    };
    Some(respond(match target {
        Dismiss::Workspace(workspace_id) => writes.dismiss_first_prompt(workspace_id).await,
        Dismiss::Session(session_id) => writes.dismiss_parked(session_id).await,
    }))
}

/// `DELETE /api/sessions/:id`: the service closes the chat. The body is optional:
/// `{workspaceId?, closeRunning?}`.
async fn close(state: &AppState, path: &str, headers: &HeaderMap, body: &[u8]) -> Option<Response> {
    let session_id = close_session(path)?;
    let Some(writes) = state.writes.clone() else {
        return Some(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "writes are unavailable",
        ));
    };
    let (workspace_id, close_running) = match close_fields(body) {
        Ok(fields) => fields,
        Err(refusal) => return Some(refusal.into_response()),
    };
    Some(respond(
        writes
            .close_chat(
                session_id,
                workspace_id,
                close_running,
                priority_of(headers),
            )
            .await,
    ))
}

/// `{workspaceId?, closeRunning?}`; an empty body counts as `{}`.
fn close_fields(body: &[u8]) -> Result<(Option<String>, bool), Refusal> {
    let object = optional_object(body).map_err(bad_request)?;
    let workspace_id = optional_string(&object, "workspaceId").map_err(bad_request)?;
    let close_running = optional_bool(&object, "closeRunning").map_err(bad_request)?;
    Ok((workspace_id, close_running))
}

/// `{status}`; the service checks the value.
fn status_field(body: &[u8]) -> Result<String, Refusal> {
    let object = object_of(body).map_err(bad_request)?;
    match object.get("status") {
        Some(Value::String(status)) => Ok(status.trim().to_owned()),
        _ => Err(bad_request(
            "status must be one of backlog, in-progress, in-review, done, canceled".to_owned(),
        )),
    }
}

/// One optional boolean of a body that may be empty.
fn flag_field(body: &[u8], field: &str) -> Result<bool, Refusal> {
    let object = optional_object(body).map_err(bad_request)?;
    optional_bool(&object, field).map_err(bad_request)
}

/// One optional string of a body that may be empty.
fn string_field(body: &[u8], field: &str) -> Result<Option<String>, Refusal> {
    let object = optional_object(body).map_err(bad_request)?;
    optional_string(&object, field).map_err(bad_request)
}

/// `GET /api/prefs`: `{"prefs": <document>}`.
fn prefs_get(state: &AppState) -> Response {
    match state.services.prefs.clone() {
        Some(prefs) => json(StatusCode::OK, &json!({ "prefs": prefs.get() })),
        None => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "preferences are unavailable",
        ),
    }
}

/// `PATCH /api/prefs`: an empty body counts as `{}`; the service owns the rest of the checks.
fn prefs_patch(state: &AppState, body: &[u8]) -> Response {
    let Some(prefs) = state.services.prefs.clone() else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "preferences are unavailable",
        );
    };
    let patch = if body.is_empty() {
        Value::Object(Map::new())
    } else {
        match serde_json::from_slice::<Value>(body) {
            Ok(value) => value,
            Err(_) => return failure(StatusCode::BAD_REQUEST, "request body must be valid JSON"),
        }
    };
    match prefs.patch(patch) {
        Ok(document) => json(StatusCode::OK, &json!({ "prefs": document })),
        Err(message) => failure(StatusCode::BAD_REQUEST, &message),
    }
}

/// The `workspaceId` query parameter, percent-decoded and trimmed; absent or empty is `None`.
fn query_workspace(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "workspaceId")
        .map(|(_, value)| {
            String::from_utf8_lossy(&super::percent_decode(value, true))
                .trim()
                .to_owned()
        })
        .filter(|value| !value.is_empty())
}

/// `{workspaceId?, model?, effort?, plan?, fast?}`; at least one agent field.
fn agent_fields(body: &[u8]) -> Result<(Option<String>, AgentPatch), Refusal> {
    let object = object_of(body).map_err(bad_request)?;
    let workspace_id = optional_string(&object, "workspaceId").map_err(bad_request)?;
    let patch = AgentPatch::from_object(&object).map_err(bad_request)?;
    if patch.is_empty() {
        return Err(bad_request("nothing to change".to_owned()));
    }
    Ok((workspace_id, patch))
}

pub(super) fn priority_of(headers: &HeaderMap) -> Priority {
    match headers.get(CLIENT_HEADER).map(HeaderValue::as_bytes) {
        Some(b"mcp") => Priority::Background,
        _ => Priority::Interactive,
    }
}

fn prompt_request(
    session_id: String,
    headers: &HeaderMap,
    body: &[u8],
    priority: Priority,
) -> Result<SendRequest, Refusal> {
    let object = object_of(body).map_err(bad_request)?;
    let fields = prompt_fields(&object).map_err(bad_request)?;
    if fields.auto {
        return Err(Refusal::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Auto is unavailable.",
        ));
    }
    Ok(SendRequest {
        session_id,
        text: fields.text,
        workspace_id: fields.workspace_id,
        client_id: fields.client_id,
        queue: fields.queue,
        client_timeout_ms: headers
            .get(TIMEOUT_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok()),
        priority,
        agent: fields.agent,
    })
}

struct PromptFields {
    text: String,
    workspace_id: Option<String>,
    client_id: Option<String>,
    queue: bool,
    auto: bool,
    agent: Option<AgentPatch>,
}

/// Checks the fields in the order `text`, `workspaceId`, `clientId`, `queue`, `auto`, `agent`.
fn prompt_fields(object: &Map<String, Value>) -> Result<PromptFields, String> {
    let text = match object.get("text") {
        Some(Value::String(text)) => text.trim(),
        _ => return Err("text: prompt must be a string".to_owned()),
    };
    if text.is_empty() {
        return Err("text: empty prompt".to_owned());
    }
    let workspace_id = optional_string(object, "workspaceId")?;
    let client_id = optional_string(object, "clientId")?;
    let queue = optional_bool(object, "queue")?;
    let auto = optional_bool(object, "auto")?;
    let agent = match object.get("agent") {
        None | Some(Value::Null) => None,
        Some(Value::Object(agent)) => {
            let patch = AgentPatch::from_object(agent).map_err(|e| format!("agent.{e}"))?;
            (!patch.is_empty()).then_some(patch)
        }
        Some(_) => return Err("agent: must be an object".to_owned()),
    };
    Ok(PromptFields {
        text: text.to_owned(),
        workspace_id,
        client_id,
        queue,
        auto,
        agent,
    })
}

/// The stop body is optional; when there is one it is an object whose `workspaceId` may name the
/// workspace.
fn stop_workspace(body: &[u8]) -> Result<Option<String>, Refusal> {
    if body.is_empty() {
        return Ok(None);
    }
    let object = object_of(body).map_err(bad_request)?;
    optional_string(&object, "workspaceId").map_err(bad_request)
}

/// `{workspaceId, previousSessionId}`, both strings.
fn history_fields(body: &[u8]) -> Result<(String, String), Refusal> {
    let object = object_of(body).map_err(bad_request)?;
    match (object.get("workspaceId"), object.get("previousSessionId")) {
        (Some(Value::String(workspace_id)), Some(Value::String(previous))) => {
            Ok((workspace_id.clone(), previous.clone()))
        }
        _ => Err(bad_request(
            "workspaceId and previousSessionId are required".to_owned(),
        )),
    }
}

/// Checks `destination` (missing, null or "chat" is a chat; "workspace" is a workspace), then the
/// row ids, as the reference does.
fn split_request(body: &[u8]) -> Result<SplitRequest, Refusal> {
    let object = object_of(body).map_err(bad_request)?;
    let destination = match object.get("destination") {
        None | Some(Value::Null) => SplitDestination::Chat,
        Some(Value::String(destination)) if destination == "chat" => SplitDestination::Chat,
        Some(Value::String(destination)) if destination == "workspace" => {
            SplitDestination::Workspace
        }
        Some(_) => {
            return Err(bad_request(
                "destination must be chat or workspace".to_owned(),
            ))
        }
    };
    let through_rowid = rowid(&object, "throughRowid").map_err(bad_request)?;
    let only_rowid = rowid(&object, "onlyRowid").map_err(bad_request)?;
    if through_rowid.is_some() && only_rowid.is_some() {
        return Err(bad_request(
            "throughRowid and onlyRowid cannot be combined".to_owned(),
        ));
    }
    Ok(SplitRequest {
        destination,
        workspace_id: optional_string(&object, "workspaceId").map_err(bad_request)?,
        prompt: optional_string(&object, "prompt").map_err(bad_request)?,
        // Thinking is kept unless the phone says `false`; tools only when it says `true`.
        include_thinking: object.get("includeThinking") != Some(&Value::Bool(false)),
        include_tools: object.get("includeTools") == Some(&Value::Bool(true)),
        through_rowid,
        only_rowid,
    })
}

/// A positive safe integer or null.
fn rowid(object: &Map<String, Value>, field: &str) -> Result<Option<i64>, String> {
    let invalid = || format!("{field} must be a positive integer");
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            let value = number.as_i64().or_else(|| {
                number
                    .as_f64()
                    .filter(|f| f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64)
                    .map(|f| f as i64)
            });
            match value {
                Some(value) if (1..=MAX_SAFE_INTEGER).contains(&value) => Ok(Some(value)),
                _ => Err(invalid()),
            }
        }
        Some(_) => Err(invalid()),
    }
}

/// The create body. The agent fields (`model`, `effort`, `plan`, `fast`) are applied; `send` is
/// accepted and ignored; Auto is not available.
fn create_request(body: &[u8]) -> Result<CreateRequest, Refusal> {
    let object = object_of(body).map_err(bad_request)?;
    let repo = optional_string(&object, "repo").map_err(bad_request)?;
    let prompt = optional_string(&object, "prompt").map_err(bad_request)?;
    let send_immediately = match object.get("sendImmediately") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(bad_request("sendImmediately: must be a boolean".to_owned())),
    };
    let attachment_ids = match object.get("attachmentIds") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(attachment_ids_invalid)?,
        Some(_) => return Err(attachment_ids_invalid()),
    };
    let agent = AgentPatch::from_object(&object).map_err(bad_request)?;
    if optional_bool(&object, "auto").map_err(bad_request)? {
        return Err(Refusal::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Auto is unavailable.",
        ));
    }
    Ok(CreateRequest {
        repo,
        prompt,
        send_immediately,
        attachment_ids,
        agent: (!agent.is_empty()).then_some(agent),
    })
}

fn attachment_ids_invalid() -> Refusal {
    bad_request("attachmentIds: must be an array of strings".to_owned())
}

/// An empty body counts as `{}`; otherwise a JSON object.
fn optional_object(body: &[u8]) -> Result<Map<String, Value>, String> {
    if body.is_empty() {
        Ok(Map::new())
    } else {
        object_of(body)
    }
}

pub(super) fn object_of(body: &[u8]) -> Result<Map<String, Value>, String> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err("request body must be a JSON object".to_owned()),
        Err(_) => Err("request body must be valid JSON".to_owned()),
    }
}

/// A string or null: trimmed, and empty is `None`.
fn optional_string(object: &Map<String, Value>, field: &str) -> Result<Option<String>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            Ok((!value.is_empty()).then(|| value.to_owned()))
        }
        Some(_) => Err(format!("{field}: must be a string")),
    }
}

/// A boolean or null; absent and null are `false`.
fn optional_bool(object: &Map<String, Value>, field: &str) -> Result<bool, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("{field}: must be a boolean")),
    }
}

/// A request the route answers itself, without calling the service.
struct Refusal {
    status: StatusCode,
    message: String,
}

impl Refusal {
    fn new(status: StatusCode, message: &str) -> Refusal {
        Refusal {
            status,
            message: message.to_owned(),
        }
    }

    fn into_response(self) -> Response {
        failure(self.status, &self.message)
    }
}

fn bad_request(message: String) -> Refusal {
    Refusal {
        status: StatusCode::BAD_REQUEST,
        message,
    }
}

fn failure(status: StatusCode, message: &str) -> Response {
    json(status, &error_body(message))
}

pub(super) fn respond(answer: WriteAnswer) -> Response {
    let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = json(status, &answer.body);
    if let Some(seconds) = answer.retry_after_secs {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}
