//! HTTP: the token gate, the API routes and the embedded web app.

mod dev;
mod files;
mod host;
mod push;
mod reads;
pub mod respond;
mod writes;

use std::borrow::Cow;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use rust_embed::RustEmbed;
use serde::Serialize;

use crate::contract::{AppState, Asset, Assets, ErrorResponse, LaunchResponse};

const INDEX: &str = "index.html";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

/// Every route the relay serves.
pub fn router(state: AppState) -> Router {
    Router::new().fallback(handle).with_state(state)
}

/// The web app built into the binary.
pub fn embedded_assets() -> Arc<dyn Assets> {
    Arc::new(EmbeddedAssets)
}

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct WebDist;

struct EmbeddedAssets;

impl Assets for EmbeddedAssets {
    fn get(&self, path: &str) -> Option<Asset> {
        let file = WebDist::get(path)?;
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let content_type = if mime.type_() == mime_guess::mime::TEXT {
            format!("{mime}; charset=utf-8")
        } else {
            mime.to_string()
        };
        Some(Asset {
            bytes: file.data,
            content_type,
        })
    }
}

async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path();
    if path == "/api" || path.starts_with("/api/") {
        api(state, request).await
    } else {
        web(&state, request)
    }
}

// ---------------------------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------------------------

/// The largest request body a write route reads.
const MAX_BODY: usize = 1 << 20;
/// The largest request body a push route reads.
const MAX_PUSH_BODY: usize = 64 << 10;
/// The largest request body a host route reads.
const MAX_HOST_BODY: usize = 64 << 10;

async fn api(state: AppState, request: Request) -> Response {
    if !authorized(&state, &request) {
        return json(StatusCode::UNAUTHORIZED, &error_body("unauthorized"));
    }
    // The supervisor's own status poll must not make the relay look busy.
    if !(request.method() == Method::GET && request.uri().path() == "/api/host/status") {
        if let Some(host) = &state.services.host {
            host.note_request();
        }
    }
    let (parts, body) = request.into_parts();
    if let Some(response) = reads::route(&state, &parts.method, &parts.uri, &parts.headers).await {
        return response;
    }
    // The attachment routes read their own body, with their own cap.
    if let Some(target) = files::Target::parse(&parts.method, parts.uri.path()) {
        return files::route(&state, target, &parts.uri, &parts.headers, body).await;
    }
    if writes::is_write(&parts.method, parts.uri.path()) {
        // Only some routes carry a body worth reading.
        let bytes = if writes::reads_body(&parts.method, parts.uri.path()) {
            match read_body(body, MAX_BODY).await {
                Ok(bytes) => bytes,
                Err(response) => return response,
            }
        } else {
            Bytes::new()
        };
        if let Some(response) =
            writes::route(&state, &parts.method, &parts.uri, &parts.headers, bytes).await
        {
            return response;
        }
    } else if push::is_push(&parts.method, parts.uri.path()) {
        // Only the POSTs have a body; `GET /api/push` reads none.
        let bytes = if parts.method == Method::POST {
            match read_body(body, MAX_PUSH_BODY).await {
                Ok(bytes) => bytes,
                Err(response) => return response,
            }
        } else {
            Bytes::new()
        };
        if let Some(response) = push::route(&state, &parts.method, parts.uri.path(), bytes).await {
            return response;
        }
    } else if host::is_host(&parts.method, parts.uri.path()) {
        // Only the POSTs have a body.
        let bytes = if host::reads_body(&parts.method) {
            match read_body(body, MAX_HOST_BODY).await {
                Ok(bytes) => bytes,
                Err(response) => return response,
            }
        } else {
            Bytes::new()
        };
        if let Some(response) = host::route(&state, &parts.method, &parts.uri, bytes).await {
            return response;
        }
    } else if dev::is_dev(&parts.method, parts.uri.path()) {
        // Only the POST has a body.
        let bytes = if dev::reads_body(&parts.method) {
            match read_body(body, MAX_HOST_BODY).await {
                Ok(bytes) => bytes,
                Err(response) => return response,
            }
        } else {
            Bytes::new()
        };
        if let Some(response) = dev::route(
            &state,
            &parts.method,
            parts.uri.path(),
            &parts.headers,
            bytes,
        )
        .await
        {
            return response;
        }
    }
    match (&parts.method, parts.uri.path()) {
        (&Method::POST, "/api/conductor/launch") => launch(state).await,
        _ => json(StatusCode::NOT_FOUND, &error_body("not found")),
    }
}

/// Reads the request body up to `limit` bytes, or answers 413 or 400.
async fn read_body(body: Body, limit: usize) -> Result<Bytes, Response> {
    match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => Ok(bytes),
        Err(error) if is_length_limit(&error) => Err(json(
            StatusCode::PAYLOAD_TOO_LARGE,
            &error_body("request body is too large"),
        )),
        Err(_) => Err(json(
            StatusCode::BAD_REQUEST,
            &error_body("could not read the request body"),
        )),
    }
}

/// Whether reading the body stopped at the length limit. The limit's error type belongs to a
/// crate this one does not depend on, so it is recognised by its message.
fn is_length_limit(error: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if current.to_string() == "length limit exceeded" {
            return true;
        }
        source = current.source();
    }
    false
}

async fn launch(state: AppState) -> Response {
    let conductor = state.conductor.clone();
    let outcome =
        tokio::task::spawn_blocking(move || conductor.launch().map_err(|e| e.to_string()))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
    match outcome {
        Ok(()) => json(
            StatusCode::OK,
            &LaunchResponse {
                ok: true,
                error: None,
            },
        ),
        Err(message) => json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &LaunchResponse {
                ok: false,
                error: Some(message),
            },
        ),
    }
}

fn error_body(message: &str) -> ErrorResponse {
    ErrorResponse {
        error: message.to_owned(),
    }
}

fn json<T: Serialize>(status: StatusCode, body: &T) -> Response {
    let bytes = serde_json::to_vec(body).expect("API bodies serialise");
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The token is the `Authorization: Bearer` value, or, when that header is absent, the `token`
/// query parameter. Either is compared with `Token::matches` and nothing else.
fn authorized(state: &AppState, request: &Request) -> bool {
    match bearer(request) {
        Some(candidate) => state.token.matches(candidate),
        None => query_token(request.uri().query()).is_some_and(|c| state.token.matches(&c)),
    }
}

fn bearer(request: &Request) -> Option<&str> {
    let value = request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

fn query_token(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
        .find(|(key, _)| *key == "token")
        .map(|(_, value)| String::from_utf8_lossy(&percent_decode(value, true)).into_owned())
}

fn percent_decode(input: &str, plus_is_space: bool) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok());
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' if plus_is_space => out.push(b' '),
            other => out.push(other),
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Web app
// ---------------------------------------------------------------------------------------------

fn web(state: &AppState, request: Request) -> Response {
    let head = match *request.method() {
        Method::GET => false,
        Method::HEAD => true,
        _ => return plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    };

    let decoded = percent_decode(request.uri().path(), false);
    let decoded = String::from_utf8_lossy(&decoded);
    if decoded.split(['/', '\\']).any(|segment| segment == "..") {
        return plain(StatusCode::FORBIDDEN, "forbidden");
    }

    let wanted = decoded.trim_start_matches('/');
    let wanted = if wanted.is_empty() { INDEX } else { wanted };
    let (served, asset) = match state.assets.get(wanted) {
        Some(asset) => (wanted, asset),
        None if wanted.starts_with("assets/") => {
            return plain(StatusCode::NOT_FOUND, "not found");
        }
        None => match state.assets.get(INDEX) {
            Some(asset) => (INDEX, asset),
            None => return plain(StatusCode::NOT_FOUND, "not found"),
        },
    };

    let cache = if served.starts_with("assets/") {
        IMMUTABLE
    } else {
        NO_CACHE
    };
    asset_response(asset, cache, head)
}

fn asset_response(asset: Asset, cache: &'static str, head: bool) -> Response {
    let length = asset.bytes.len();
    let body = match (head, asset.bytes) {
        (true, _) => Body::empty(),
        (false, Cow::Borrowed(bytes)) => Body::from(bytes),
        (false, Cow::Owned(bytes)) => Body::from(bytes),
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&asset.content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    response
}

fn plain(status: StatusCode, text: &'static str) -> Response {
    let mut response = Response::new(Body::from(text));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE));
    response
}
