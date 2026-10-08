//! The read routes: Conductor's workspaces, chats and messages over HTTP.

use std::sync::Arc;

use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use serde::Serialize;
use serde_json::Value;

use super::respond::{bytes_response, json_bytes_response, json_response, match_param};
use crate::contract::{AppState, ConductorStatus, ErrorResponse, StateResponse};
use crate::db::DbError;
use crate::delivery::WriteService;
use crate::files::{file_preview, local_image, ImageError, PreviewError};
use crate::reads::extras::commands::{Commands, SystemCommands};
use crate::reads::review::diff::{
    list_source_files, workspace_diff, workspace_file_diff, WorkspaceFiles,
};
use crate::reads::sessions::SessionRow;
use crate::reads::snapshot::Key;
use crate::reads::workspaces::{RepoRow, SearchWorkspace};
use crate::reads::{ReadError, Reads};
use crate::search::results::SearchParams;
use crate::usage::tools::{ToolRange, ToolUsageError};

const API_STATE: &str = "/api/state";
const API_REPOS: &str = "/api/repos";
const WORKSPACES: &str = "/api/workspaces/";
const SESSIONS: &str = "/api/sessions/";
const REPOS: &str = "/api/repos/";
const TOOL_IMAGES: &str = "/api/tool-images/";
const API_MODELS: &str = "/api/models";
const API_MODEL_DEFAULTS: &str = "/api/models/defaults";
const FILES: &str = "/api/files/";
const LOCAL_IMAGES: &str = "/api/local-images/";
const API_SEARCH: &str = "/api/search";
const API_USAGE: &str = "/api/usage";
const API_TOOL_USAGE: &str = "/api/usage/tools";

const TOOL_RANGE_REFUSAL: &str = "Choose 24h, 7d, or 30d for tool usage.";
const TOOL_TIMEOUT_MESSAGE: &str = "Tool usage took too long to read. Try a shorter range.";
/// The limit of a search whose `limit` is missing, not a number, not finite or zero.
const DEFAULT_SEARCH_LIMIT: f64 = 12.0;
const MAX_SEARCH_LIMIT: f64 = 50.0;

const ICON_CACHE: &str = "public, max-age=300";
const TOOL_IMAGE_CACHE: &str = "private, max-age=86400, immutable";
const LOCAL_IMAGE_CACHE: &str = "no-store";

/// The header a phone sends to say which device it is.
const DEVICE_HEADER: &str = "x-relay-device";

/// What a request asks for.
enum Target {
    State,
    Repos,
    Workspace(String),
    Sessions(String),
    ClosedSessions(String),
    Messages {
        session: String,
        after: i64,
    },
    RepoIcon(String),
    ToolImage(String),
    Context(String),
    Models,
    ModelDefaults,
    Diff(String),
    FileDiff {
        id: String,
        /// The decoded `path` parameter; `None` when it is missing, empty or not decodable.
        path: Option<String>,
    },
    Files(String),
    FilePreview(String),
    LocalImage(String),
    Search(SearchParams),
    PlanUsage {
        force: bool,
    },
    ToolUsage {
        /// `None` for a `range` that is none of `24h`, `7d` and `30d`.
        range: Option<ToolRange>,
        force: bool,
    },
}

impl Target {
    /// The kind of route, for the failure log. Never an id.
    fn kind(&self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Repos => "repos",
            Self::Workspace(_) => "workspace",
            Self::Sessions(_) => "sessions",
            Self::ClosedSessions(_) => "closed sessions",
            Self::Messages { .. } => "messages",
            Self::RepoIcon(_) => "repo icon",
            Self::ToolImage(_) => "tool image",
            Self::Context(_) => "context",
            Self::Models => "models",
            Self::ModelDefaults => "model defaults",
            Self::Diff(_) => "diff",
            Self::FileDiff { .. } => "file diff",
            Self::Files(_) => "files",
            Self::FilePreview(_) => "file preview",
            Self::LocalImage(_) => "local image",
            Self::Search(_) => "search",
            Self::PlanUsage { .. } => "plan usage",
            Self::ToolUsage { .. } => "tool usage",
        }
    }

    /// `None` for a path that is not a read route.
    fn parse(path: &str, query: Option<&str>) -> Option<Self> {
        match path {
            API_STATE => return Some(Self::State),
            API_REPOS => return Some(Self::Repos),
            API_MODELS => return Some(Self::Models),
            API_MODEL_DEFAULTS => return Some(Self::ModelDefaults),
            API_SEARCH => return Some(Self::Search(search_params(query))),
            API_USAGE => {
                return Some(Self::PlanUsage {
                    force: refresh(query),
                })
            }
            API_TOOL_USAGE => {
                return Some(Self::ToolUsage {
                    range: tool_range(query),
                    force: refresh(query),
                })
            }
            _ => {}
        }
        if let Some(name) = match_param(path, REPOS, "/icon") {
            return Some(Self::RepoIcon(name));
        }
        if let Some(reference) = match_param(path, TOOL_IMAGES, "") {
            return Some(Self::ToolImage(reference));
        }
        if let Some(id) = match_param(path, SESSIONS, "/context") {
            return Some(Self::Context(id));
        }
        if let Some(reference) = match_param(path, FILES, "") {
            return Some(Self::FilePreview(reference));
        }
        if let Some(image) = match_param(path, LOCAL_IMAGES, "") {
            return Some(Self::LocalImage(image));
        }
        if let Some(id) = match_param(path, WORKSPACES, "/diff/file") {
            return Some(Self::FileDiff {
                id,
                path: form_value(query, "path").filter(|path| !path.is_empty()),
            });
        }
        if let Some(id) = match_param(path, WORKSPACES, "/diff") {
            return Some(Self::Diff(id));
        }
        if let Some(id) = match_param(path, WORKSPACES, "/files") {
            return Some(Self::Files(id));
        }
        if let Some(id) = match_param(path, WORKSPACES, "/sessions/closed") {
            return Some(Self::ClosedSessions(id));
        }
        if let Some(id) = match_param(path, WORKSPACES, "/sessions") {
            return Some(Self::Sessions(id));
        }
        if let Some(id) = match_param(path, WORKSPACES, "") {
            return Some(Self::Workspace(id));
        }
        match_param(path, SESSIONS, "/messages").map(|session| Self::Messages {
            session,
            after: after(query),
        })
    }
}

/// The `after` query parameter as a 64-bit integer; anything else, or none, is 0.
fn after(query: Option<&str>) -> i64 {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("after="))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// The raw values of every parameter called `key` in a query string, in order.
fn raw_values<'a>(query: Option<&'a str>, key: &'a str) -> impl Iterator<Item = &'a str> {
    query.into_iter().flat_map(move |q| {
        q.split('&').filter_map(move |pair| {
            pair.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
        })
    })
}

/// A query value decoded as a form value: `+` is a space, then percent-decoding. `None` when an
/// escape is invalid or the bytes are not UTF-8.
fn decode_form(value: &str) -> Option<String> {
    let bytes = value.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex_digit(*bytes.get(i + 1)?)?;
            let low = hex_digit(*bytes.get(i + 2)?)?;
            out.push(high << 4 | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The first parameter called `key` in a query string, decoded as a form value. `None` when
/// there is none, or when it is not decodable.
fn form_value(query: Option<&str>, key: &str) -> Option<String> {
    raw_values(query, key).next().and_then(decode_form)
}

/// Every parameter called `key`, decoded as form values, in order; one that is not decodable is
/// left out.
fn form_values(query: Option<&str>, key: &str) -> Vec<String> {
    raw_values(query, key).filter_map(decode_form).collect()
}

/// `refresh=1` asks for a fresh read; any other value, or none, does not.
fn refresh(query: Option<&str>) -> bool {
    form_value(query, "refresh").as_deref() == Some("1")
}

/// What `GET /api/search` asks for. An undecodable `q` is empty and an undecodable `repo` is
/// dropped; `repo` may repeat, and empty and repeated values count once.
fn search_params(query: Option<&str>) -> SearchParams {
    let mut repos: Vec<String> = Vec::new();
    for repo in form_values(query, "repo") {
        if !repo.is_empty() && !repos.contains(&repo) {
            repos.push(repo);
        }
    }
    SearchParams {
        q: form_value(query, "q").unwrap_or_default(),
        repos,
        archived: form_value(query, "archived").as_deref() != Some("0"),
        limit: search_limit(form_value(query, "limit").as_deref()),
    }
}

/// The result limit: the value as a number, 12 when it is missing, not a number, not finite or
/// zero, then at least 1 and at most 50, rounded down.
fn search_limit(value: Option<&str>) -> usize {
    let number = value
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|number| number.is_finite() && *number != 0.0)
        .unwrap_or(DEFAULT_SEARCH_LIMIT);
    number.clamp(1.0, MAX_SEARCH_LIMIT).floor() as usize
}

/// The `range` of the tool usage route: `24h` when it is missing, `None` when it is not one of
/// the three or not decodable.
fn tool_range(query: Option<&str>) -> Option<ToolRange> {
    match raw_values(query, "range").next() {
        None => ToolRange::parse(None).ok(),
        Some(raw) => ToolRange::parse(Some(&decode_form(raw)?)).ok(),
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Answers a read route, or `None` when the request is not one. The token has been checked.
///
/// Takes the parts of the request rather than the request: its body is not `Sync`, so a
/// reference to it cannot live across the await.
pub async fn route(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<Response> {
    if method != Method::GET {
        return None;
    }
    let target = Target::parse(uri.path(), uri.query())?;
    note_viewing(state, &target, headers);
    let running = state.conductor.status().is_running();
    let reads = state.reads.clone().filter(|_| running);

    let response = match (target, reads) {
        (Target::State, None) => json_response(
            headers,
            method,
            StatusCode::OK,
            &StateResponse::skeleton(state.conductor.status()),
        ),
        (_, None) => json_response(
            headers,
            method,
            StatusCode::SERVICE_UNAVAILABLE,
            &error_body("Conductor is not running"),
        ),
        (target, Some(reads)) => {
            let route = target.kind();
            let parked = match (&target, &state.writes) {
                (Target::State, Some(writes)) => writes.parked_prompts(),
                _ => Vec::new(),
            };
            let writes = state.writes.clone();
            match answer(
                target,
                reads,
                parked,
                writes,
                headers.clone(),
                method.clone(),
            )
            .await
            {
                Ok(response) => response,
                Err(failure) => {
                    tracing::error!(
                        route,
                        cause = %failure.describe(),
                        "Conductor read failed"
                    );
                    json_response(
                        headers,
                        method,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &error_body("internal error"),
                    )
                }
            }
        }
    };
    Some(response)
}

/// Tells the notifier which chat a device is showing. A messages request that carries a
/// non-empty `x-relay-device` counts, whatever it is answered with, a 304 included.
fn note_viewing(state: &AppState, target: &Target, headers: &HeaderMap) {
    let (Target::Messages { session, .. }, Some(notify)) = (target, &state.notify) else {
        return;
    };
    let device = headers
        .get(DEVICE_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|device| !device.is_empty());
    if let Some(device) = device {
        notify.note_viewing(device, session);
    }
}

/// What a read produced: a serialised JSON body, or the bytes of a file or an image.
enum Answer {
    Json(StatusCode, Vec<u8>),
    Bytes {
        content_type: String,
        cache_control: &'static str,
        body: Vec<u8>,
    },
}

impl Answer {
    /// Builds the response; run it on the blocking pool, as the ETag and the gzip are costly.
    fn into_response(self, headers: &HeaderMap, method: &Method) -> Response {
        match self {
            Self::Json(status, body) => json_bytes_response(headers, method, status, body),
            Self::Bytes {
                content_type,
                cache_control,
                body,
            } => bytes_response(StatusCode::OK, &content_type, cache_control, body),
        }
    }
}

/// Why a read did not produce a body. Its description never carries a row.
enum Failure {
    Read(ReadError),
    Encode,
    Task,
    /// Only the kind of the error: the message of an I/O error names a path.
    Io(std::io::ErrorKind),
    /// A tool usage scan that failed for a reason other than its time-out. Its message is not
    /// logged: it may carry what the scan read.
    ToolUsage,
}

impl Failure {
    fn describe(&self) -> String {
        match self {
            Self::Read(ReadError::Db(DbError::Open { source, .. })) => {
                format!("cannot open the database ({})", describe_sqlite(source))
            }
            Self::Read(ReadError::Db(DbError::Query(source))) => {
                format!("query failed ({})", describe_sqlite(source))
            }
            Self::Encode => "the answer could not be serialised".to_owned(),
            Self::Task => "the read task panicked or was cancelled".to_owned(),
            Self::Io(kind) => format!("I/O error ({kind})"),
            Self::ToolUsage => "the tool usage scan failed".to_owned(),
        }
    }
}

/// The SQLite code of an error and what names the failure: the message SQLite gave for a
/// failed statement (`SqliteFailure`, or `SqlInputError`, which is what a statement that does
/// not compile, such as one naming a missing column, comes back as), the column of an
/// `InvalidColumnType`. These name schema objects (a column, a table), never a row value; the
/// statement text of `SqlInputError` is left out all the same. Any other variant is described
/// by its code alone.
fn describe_sqlite(source: &rusqlite::Error) -> String {
    let code = |code: Option<rusqlite::ffi::ErrorCode>| {
        code.map_or_else(|| "none".to_owned(), |c| format!("{c:?}"))
    };
    match source {
        rusqlite::Error::SqliteFailure(error, Some(message)) => {
            format!("SQLite code {}: {message}", code(Some(error.code)))
        }
        rusqlite::Error::SqlInputError { error, msg, .. } => {
            format!("SQLite code {}: {msg}", code(Some(error.code)))
        }
        rusqlite::Error::InvalidColumnType(index, name, kind) => {
            format!(
                "SQLite code {}: column {index} {name:?} has type {kind}",
                code(source.sqlite_error_code())
            )
        }
        _ => format!("SQLite code {}", code(source.sqlite_error_code())),
    }
}

impl From<ReadError> for Failure {
    fn from(error: ReadError) -> Self {
        Self::Read(error)
    }
}

impl From<DbError> for Failure {
    fn from(error: DbError) -> Self {
        Self::Read(error.into())
    }
}

impl From<serde_json::Error> for Failure {
    fn from(_: serde_json::Error) -> Self {
        Self::Encode
    }
}

#[derive(Serialize)]
struct Repos {
    repos: Vec<RepoRow>,
}

#[derive(Serialize)]
struct Workspace {
    workspace: SearchWorkspace,
}

#[derive(Serialize)]
struct Sessions<T> {
    sessions: Vec<T>,
}

/// Runs the reads of one route on the blocking pool and finishes its response there too: the
/// ETag's SHA-1 and the gzip of a large body must not run on the async thread. `parked` is the
/// parked prompts for the state route; every other route ignores it. `writes` is asked, on the
/// blocking pool, for the pending first prompts (state route) and the chat links (sessions route).
async fn answer(
    target: Target,
    reads: Arc<Reads>,
    parked: Vec<Value>,
    writes: Option<Arc<dyn WriteService>>,
    headers: HeaderMap,
    method: Method,
) -> Result<Response, Failure> {
    tokio::task::spawn_blocking(move || {
        read(target, &reads, &parked, writes.as_deref())
            .map(|answer| answer.into_response(&headers, &method))
    })
    .await
    .unwrap_or(Err(Failure::Task))
}

fn read(
    target: Target,
    reads: &Reads,
    parked: &[Value],
    writes: Option<&dyn WriteService>,
) -> Result<Answer, Failure> {
    match target {
        Target::State => reads
            .snapshot()
            .get_or_build(reads.db(), Key::State, || state_body(reads))
            .and_then(|body| with_parked(body, parked))
            .and_then(|body| {
                let pending = writes.map(|writes| writes.pending_prompts());
                with_pending(body, pending.as_deref().unwrap_or_default())
            })
            .map(|body| Answer::Json(StatusCode::OK, body)),
        Target::Repos => ok(&Repos {
            repos: reads.list_repos()?,
        }),
        Target::Workspace(id) => match reads.get_any_workspace(&id)? {
            Some(workspace) => ok(&Workspace { workspace }),
            None => workspace_not_found(),
        },
        Target::Sessions(id) => reads
            .snapshot()
            .get_or_build(reads.db(), Key::Sessions(id.clone()), || {
                Ok(serde_json::to_vec(&Sessions::<SessionRow> {
                    sessions: reads.list_sessions(&id)?,
                })?)
            })
            .and_then(|body| {
                let history = writes.map(|writes| writes.chat_history(&id));
                with_chat_history(body, history)
            })
            .map(|body| Answer::Json(StatusCode::OK, body)),
        Target::ClosedSessions(id) => {
            if reads.get_any_workspace(&id)?.is_none() {
                return workspace_not_found();
            }
            ok(&Sessions {
                sessions: reads.list_closed_sessions(&id)?,
            })
        }
        Target::Messages { session, after } => ok(&reads.get_messages(&session, after)?),
        Target::RepoIcon(name) => match reads.repo_icon(&name)? {
            Some(icon) => Ok(Answer::Bytes {
                content_type: icon.content_type.to_owned(),
                cache_control: ICON_CACHE,
                body: icon.bytes,
            }),
            None => not_found("no icon"),
        },
        Target::ToolImage(reference) => match reads.tool_image(&reference)? {
            Some(image) => Ok(Answer::Bytes {
                content_type: image.media_type,
                cache_control: TOOL_IMAGE_CACHE,
                body: image.bytes,
            }),
            None => not_found("image not found"),
        },
        Target::Context(id) => match reads.context_breakdown(&id)? {
            Some(breakdown) => ok(&breakdown),
            None => not_found("chat not found"),
        },
        Target::Diff(id) => {
            let Some(live) = reads.live_target(&id)? else {
                return workspace_not_found();
            };
            let Some(worktree) = live.worktree else {
                return worktree_unresolved();
            };
            ok(&workspace_diff(
                review_commands(reads),
                &worktree,
                &live.base_branch,
            ))
        }
        Target::FileDiff { id, path } => {
            let Some(live) = reads.live_target(&id)? else {
                return workspace_not_found();
            };
            let Some(worktree) = live.worktree else {
                return worktree_unresolved();
            };
            let Some(path) = path else {
                return error_answer(StatusCode::BAD_REQUEST, "file path is required");
            };
            match workspace_file_diff(review_commands(reads), &worktree, &live.base_branch, &path) {
                Some(diff) => ok(&diff),
                None => not_found("changed file not found"),
            }
        }
        Target::Files(id) => {
            let Some(live) = reads.live_target(&id)? else {
                return workspace_not_found();
            };
            match live.worktree {
                Some(worktree) => ok(&list_source_files(review_commands(reads), &worktree)),
                None => ok(&WorkspaceFiles {
                    files: Vec::new(),
                    truncated: false,
                }),
            }
        }
        Target::FilePreview(reference) => {
            let Some((roots, mode)) = reads.preview() else {
                return not_found("source file not found");
            };
            match file_preview(&reference, roots, mode) {
                Ok(preview) => ok(&preview),
                Err(PreviewError::NotFound) => not_found("source file not found"),
                Err(PreviewError::Forbidden(message)) => {
                    error_answer(StatusCode::FORBIDDEN, message)
                }
                Err(PreviewError::TooLarge) => error_answer(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "source file is too large to preview",
                ),
                Err(PreviewError::NotText) => error_answer(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "source file is not text",
                ),
            }
        }
        Target::LocalImage(path) => {
            let Some((roots, mode)) = reads.preview() else {
                return not_found("image not found");
            };
            match local_image(&path, roots, mode) {
                Ok(image) => Ok(Answer::Bytes {
                    content_type: image.content_type.to_owned(),
                    cache_control: LOCAL_IMAGE_CACHE,
                    body: image.bytes,
                }),
                Err(ImageError::NotFound) => not_found("image not found"),
                Err(ImageError::TooLarge) => {
                    error_answer(StatusCode::PAYLOAD_TOO_LARGE, "image is too large")
                }
            }
        }
        Target::Search(params) => ok(&reads.search(reads.search_index().map(|i| &**i), &params)?),
        Target::PlanUsage { force } => match reads.plan_usage() {
            Some(plan) => ok(&plan.read(force)),
            None => usage_unavailable(),
        },
        Target::ToolUsage { range, force } => {
            let Some(range) = range else {
                return error_answer(StatusCode::BAD_REQUEST, TOOL_RANGE_REFUSAL);
            };
            let Some(tools) = reads.tool_usage() else {
                return usage_unavailable();
            };
            match tools.read(range, force) {
                Ok(snapshot) => ok(&snapshot),
                Err(ToolUsageError::TimedOut) => {
                    error_answer(StatusCode::GATEWAY_TIMEOUT, TOOL_TIMEOUT_MESSAGE)
                }
                Err(ToolUsageError::Read(_)) => Err(Failure::ToolUsage),
            }
        }
        Target::Models => ok(&reads.model_catalog()),
        Target::ModelDefaults => ok(&reads.model_defaults().map_err(|e| Failure::Io(e.kind()))?),
    }
}

/// The full `/api/state` body for a running Conductor.
fn state_body(reads: &Reads) -> Result<Vec<u8>, Failure> {
    let workspaces = reads
        .list_workspaces()?
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    let mut response = StateResponse::skeleton(ConductorStatus::Running);
    response.workspaces = workspaces;
    Ok(serde_json::to_vec(&response)?)
}

/// The `/api/state` body with `parked_prompts` added to each workspace that has entries. With no
/// entries the body is returned as it is, unparsed. The cached body is never changed: the
/// entries are added to a copy, so the next answer starts from the cache again.
fn with_parked(body: Vec<u8>, parked: &[Value]) -> Result<Vec<u8>, Failure> {
    if parked.is_empty() {
        return Ok(body);
    }
    let mut state: Value = serde_json::from_slice(&body)?;
    if let Some(workspaces) = state.get_mut("workspaces").and_then(Value::as_array_mut) {
        for workspace in workspaces {
            let Some(id) = workspace.get("id").and_then(Value::as_str) else {
                continue;
            };
            let entries: Vec<Value> = parked
                .iter()
                .filter(|entry| entry.get("workspaceId").and_then(Value::as_str) == Some(id))
                .cloned()
                .collect();
            if !entries.is_empty() {
                workspace["parked_prompts"] = Value::Array(entries);
            }
        }
    }
    Ok(serde_json::to_vec(&state)?)
}

/// The `/api/state` body with `pending_prompt` added to each workspace that has an entry (the
/// first one, in the order given). With no entries the body is returned as it is, unparsed. Like
/// `with_parked`, it works on a copy of the cached body.
fn with_pending(body: Vec<u8>, pending: &[Value]) -> Result<Vec<u8>, Failure> {
    if pending.is_empty() {
        return Ok(body);
    }
    let mut state: Value = serde_json::from_slice(&body)?;
    if let Some(workspaces) = state.get_mut("workspaces").and_then(Value::as_array_mut) {
        for workspace in workspaces {
            let Some(id) = workspace.get("id").and_then(Value::as_str) else {
                continue;
            };
            let entry = pending
                .iter()
                .find(|entry| entry.get("workspaceId").and_then(Value::as_str) == Some(id))
                .cloned();
            if let Some(entry) = entry {
                workspace["pending_prompt"] = entry;
            }
        }
    }
    Ok(serde_json::to_vec(&state)?)
}

/// The sessions body with `chat_history` added when the object has entries. An empty object, or
/// none, leaves the body as it is, unparsed. The cached body is never changed.
fn with_chat_history(body: Vec<u8>, history: Option<Value>) -> Result<Vec<u8>, Failure> {
    let Some(history) =
        history.filter(|history| history.as_object().is_some_and(|o| !o.is_empty()))
    else {
        return Ok(body);
    };
    let mut sessions: Value = serde_json::from_slice(&body)?;
    if let Some(object) = sessions.as_object_mut() {
        object.insert("chat_history".to_owned(), history);
    }
    Ok(serde_json::to_vec(&sessions)?)
}

fn ok(body: &impl Serialize) -> Result<Answer, Failure> {
    Ok(Answer::Json(StatusCode::OK, serde_json::to_vec(body)?))
}

fn error_answer(status: StatusCode, message: &str) -> Result<Answer, Failure> {
    Ok(Answer::Json(
        status,
        serde_json::to_vec(&error_body(message))?,
    ))
}

fn not_found(message: &str) -> Result<Answer, Failure> {
    error_answer(StatusCode::NOT_FOUND, message)
}

fn usage_unavailable() -> Result<Answer, Failure> {
    error_answer(StatusCode::SERVICE_UNAVAILABLE, "usage is not available")
}

fn worktree_unresolved() -> Result<Answer, Failure> {
    error_answer(StatusCode::CONFLICT, "worktree path unresolved")
}

/// The runner of the git work of a review route: the extras' when there are extras, else the
/// system's.
fn review_commands(reads: &Reads) -> &dyn Commands {
    match reads.extras() {
        Some(extras) => extras.shared().commands.as_ref(),
        None => &SystemCommands,
    }
}

fn workspace_not_found() -> Result<Answer, Failure> {
    not_found("workspace not found")
}

fn error_body(message: &str) -> ErrorResponse {
    ErrorResponse {
        error: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::types::Type;
    use rusqlite::Connection;

    use super::*;

    fn query_failure(source: rusqlite::Error) -> String {
        Failure::Read(ReadError::Db(DbError::Query(source))).describe()
    }

    #[test]
    fn a_statement_error_carries_the_message_sqlite_gave() {
        let conn = Connection::open_in_memory().unwrap();
        let error = conn
            .prepare("SELECT no_such_column FROM sqlite_master")
            .unwrap_err();
        let text = query_failure(error);
        assert!(
            text.starts_with("query failed (SQLite code Unknown: "),
            "{text}"
        );
        assert!(text.contains("no such column: no_such_column"), "{text}");
    }

    #[test]
    fn a_failed_step_carries_the_message_sqlite_gave() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".to_owned()),
        );
        assert_eq!(
            query_failure(error),
            "query failed (SQLite code DatabaseCorrupt: database disk image is malformed)"
        );
    }

    #[test]
    fn an_invalid_column_type_carries_its_column_index_name_and_type() {
        let error = rusqlite::Error::InvalidColumnType(3, "queue_order".to_owned(), Type::Blob);
        let text = query_failure(error);
        assert_eq!(
            text,
            "query failed (SQLite code none: column 3 \"queue_order\" has type Blob)"
        );
    }

    #[test]
    fn another_variant_carries_only_the_code() {
        let error = rusqlite::Error::InvalidParameterName("a-secret-name".to_owned());
        assert_eq!(query_failure(error), "query failed (SQLite code none)");
    }

    #[test]
    fn a_failed_open_is_described_the_same_way() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some("unable to open database file".to_owned()),
        );
        let text = Failure::Read(ReadError::Db(DbError::Open {
            path: "/nowhere".into(),
            source: error,
        }))
        .describe();
        assert_eq!(
            text,
            "cannot open the database (SQLite code CannotOpen: unable to open database file)"
        );
    }

    #[test]
    fn an_io_failure_is_described_by_its_kind_only() {
        let failure = Failure::Io(std::io::ErrorKind::PermissionDenied);
        assert_eq!(failure.describe(), "I/O error (permission denied)");
    }

    #[test]
    fn the_route_kind_never_carries_an_id() {
        let kinds = [
            Target::State.kind(),
            Target::Repos.kind(),
            Target::Workspace("w-1".into()).kind(),
            Target::Sessions("w-1".into()).kind(),
            Target::ClosedSessions("w-1".into()).kind(),
            Target::Messages {
                session: "s-1".into(),
                after: 0,
            }
            .kind(),
            Target::RepoIcon("r-1".into()).kind(),
            Target::ToolImage("1.0".into()).kind(),
            Target::Context("s-1".into()).kind(),
            Target::Models.kind(),
            Target::ModelDefaults.kind(),
            Target::Diff("w-1".into()).kind(),
            Target::FileDiff {
                id: "w-1".into(),
                path: None,
            }
            .kind(),
            Target::Files("w-1".into()).kind(),
            Target::FilePreview("/a.rs".into()).kind(),
            Target::LocalImage("/a.png".into()).kind(),
            Target::Search(search_params(None)).kind(),
            Target::PlanUsage { force: false }.kind(),
            Target::ToolUsage {
                range: None,
                force: false,
            }
            .kind(),
        ];
        assert_eq!(
            kinds,
            [
                "state",
                "repos",
                "workspace",
                "sessions",
                "closed sessions",
                "messages",
                "repo icon",
                "tool image",
                "context",
                "models",
                "model defaults",
                "diff",
                "file diff",
                "files",
                "file preview",
                "local image",
                "search",
                "plan usage",
                "tool usage"
            ]
        );
    }
}
