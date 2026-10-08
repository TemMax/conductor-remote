//! The dev-server routes: one workspace's state, start and stop. Each validates the request and
//! hands it to the `DevServerService`, whose status and body pass through.

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use serde_json::Value;

use super::respond::match_param;
use super::writes::{object_of, priority_of, respond};
use super::{error_body, json};
use crate::contract::AppState;

const WORKSPACES: &str = "/api/workspaces/";
const DEV_SERVER: &str = "/dev-server";

/// The workspace of a `/api/workspaces/:id/dev-server` path.
fn workspace_of(path: &str) -> Option<String> {
    match_param(path, WORKSPACES, DEV_SERVER)
}

/// Whether the request is one of the dev-server routes, whatever its body.
pub fn is_dev(method: &Method, path: &str) -> bool {
    matches!(*method, Method::GET | Method::POST | Method::DELETE) && workspace_of(path).is_some()
}

/// Whether the route reads a request body: the `POST`.
pub fn reads_body(method: &Method) -> bool {
    method == Method::POST
}

/// Answers a dev-server route, or `None` when the request is not one. The token has been checked
/// and `body` is empty unless `reads_body` said otherwise.
pub async fn route(
    state: &AppState,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Option<Response> {
    if !is_dev(method, path) {
        return None;
    }
    let workspace_id = workspace_of(path)?;
    let Some(dev) = state.services.dev.clone() else {
        return Some(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the dev server is unavailable",
        ));
    };
    let priority = priority_of(headers);
    let answer = match *method {
        Method::GET => dev.state(workspace_id).await,
        Method::DELETE => dev.stop(workspace_id, priority).await,
        _ => match run_config_id_of(&body) {
            Ok(run_config_id) => dev.start(workspace_id, run_config_id, priority).await,
            Err(message) => return Some(failure(StatusCode::BAD_REQUEST, &message)),
        },
    };
    Some(respond(answer))
}

/// The `runConfigId` of a start body. An empty body is `{}`; absent and `null` are `None`; a
/// string is trimmed and must not be empty. The error is the message of the 400 to answer.
fn run_config_id_of(body: &[u8]) -> Result<Option<String>, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let object = object_of(body)?;
    match object.get("runConfigId") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.trim().to_owned())),
        Some(_) => Err("runConfigId must be a non-empty string".to_owned()),
    }
}

fn failure(status: StatusCode, message: &str) -> Response {
    json(status, &error_body(message))
}
