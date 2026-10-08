//! The attachment routes: upload to a chat, stage for a new workspace, and discard a staged
//! file. The two uploads read their own body, capped at 25 MiB, and hand the bytes to the
//! `WriteService` without copying them.

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;

use super::respond::match_param;
use super::writes::respond;
use super::{error_body, is_length_limit, json, percent_decode};
use crate::contract::AppState;

const NAME_HEADER: &str = "x-attachment-name";
const STAGE: &str = "/api/attachments";
/// The largest attachment an upload accepts.
const MAX_ATTACHMENT: usize = 25 << 20;
const TOO_LARGE: &str = "attachments are limited to 25 MB";

/// The attachment routes, by method and path.
pub enum Target {
    /// `POST /api/sessions/:id/attachments?workspaceId=`.
    Upload(String),
    /// `POST /api/attachments`.
    Stage,
    /// `DELETE /api/attachments/:id`.
    Discard(String),
}

impl Target {
    /// The route this request is, whatever its body.
    pub fn parse(method: &Method, path: &str) -> Option<Target> {
        match *method {
            Method::POST if path == STAGE => Some(Target::Stage),
            Method::POST => match_param(path, "/api/sessions/", "/attachments").map(Target::Upload),
            Method::DELETE => match_param(path, "/api/attachments/", "").map(Target::Discard),
            _ => None,
        }
    }
}

/// Answers an attachment route. The token has been checked; the body has not been read.
pub async fn route(
    state: &AppState,
    target: Target,
    uri: &Uri,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let Some(writes) = state.writes.clone() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "writes are unavailable");
    };
    if let Target::Discard(stage_id) = target {
        return respond(writes.discard_staged(stage_id).await);
    }
    let name = attachment_name(headers);
    let bytes = match read_attachment(headers, body).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    respond(match target {
        Target::Upload(session_id) => {
            writes
                .upload_attachment(session_id, workspace_id(uri.query()), name, bytes)
                .await
        }
        Target::Stage | Target::Discard(_) => writes.stage_attachment(name, bytes).await,
    })
}

/// The `x-attachment-name` header, percent-decoded as UTF-8; `""` when it is missing or cannot
/// be decoded (the service answers 400 after its own workspace checks).
fn attachment_name(headers: &HeaderMap) -> String {
    headers
        .get(NAME_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| String::from_utf8(percent_decode(value, false)).ok())
        .unwrap_or_default()
}

/// The `workspaceId` query parameter, when it is there and not empty.
fn workspace_id(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "workspaceId")
        .map(|(_, value)| String::from_utf8_lossy(&percent_decode(value, true)).into_owned())
        .filter(|value| !value.is_empty())
}

/// Reads the body, refusing with 413 when `content-length` already says it is over the cap and
/// stopping at the cap when it does not say. At most 25 MiB is ever held.
async fn read_attachment(headers: &HeaderMap, body: Body) -> Result<Bytes, Response> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_ATTACHMENT as u64) {
        return Err(failure(StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE));
    }
    match axum::body::to_bytes(body, MAX_ATTACHMENT).await {
        Ok(bytes) => Ok(bytes),
        Err(error) if is_length_limit(&error) => {
            Err(failure(StatusCode::PAYLOAD_TOO_LARGE, TOO_LARGE))
        }
        Err(_) => Err(failure(
            StatusCode::BAD_REQUEST,
            "could not read the request body",
        )),
    }
}

fn failure(status: StatusCode, message: &str) -> Response {
    json(status, &error_body(message))
}
