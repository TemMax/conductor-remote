//! The host routes: the relay's log, the settings the phone reads, keep-awake and the Conductor
//! restart. Each validates the request and hands it to the `HostService`, whose status and body
//! pass through.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use serde_json::{Map, Value};

use super::writes::respond;
use super::{error_body, json, percent_decode};
use crate::contract::AppState;
use crate::delivery::WriteAnswer;
use crate::host::HostService;

const LOGS: &str = "/api/logs";
const SETTINGS: &str = "/api/settings";
const STATUS: &str = "/api/host/status";
const NOSLEEP: &str = "/api/nosleep";
const RESTART: &str = "/api/conductor/restart";

/// The log lines asked for when `limit` is missing.
const DEFAULT_LIMIT: usize = 300;
/// The most log lines one request can ask for.
const MAX_LIMIT: usize = 2000;

/// Whether the request is one of the host routes, whatever its parameters or body.
pub fn is_host(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET => matches!(path, LOGS | SETTINGS | STATUS | NOSLEEP),
        Method::POST => matches!(path, NOSLEEP | RESTART),
        Method::DELETE => path == NOSLEEP,
        _ => false,
    }
}

/// Whether the route reads a request body: the two `POST`s.
pub fn reads_body(method: &Method) -> bool {
    method == Method::POST
}

/// Answers a host route, or `None` when the request is not one. The token has been checked and
/// `body` is empty unless `reads_body` said otherwise.
pub async fn route(
    state: &AppState,
    method: &Method,
    uri: &axum::http::Uri,
    body: Bytes,
) -> Option<Response> {
    let path = uri.path();
    if !is_host(method, path) {
        return None;
    }
    let Some(host) = state.services.host.clone() else {
        return Some(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "host services are unavailable",
        ));
    };
    let answer = match (method, path) {
        (&Method::GET, LOGS) => {
            let file = form_value(uri.query(), "file").filter(|file| !file.is_empty());
            let Some(limit) = limit_of(uri.query()) else {
                return Some(failure(StatusCode::BAD_REQUEST, "limit must be a number"));
            };
            blocking(host, move |host| host.logs(file, Some(limit))).await
        }
        (&Method::GET, SETTINGS) => blocking(host, |host| host.settings()).await,
        (&Method::GET, STATUS) => blocking(host, |host| host.status()).await,
        (&Method::GET, NOSLEEP) => blocking(host, |host| host.nosleep()).await,
        (&Method::DELETE, NOSLEEP) => blocking(host, |host| host.disarm_nosleep()).await,
        (&Method::POST, NOSLEEP) => {
            let object = match object_of(&body) {
                Ok(object) => object,
                Err(message) => return Some(failure(StatusCode::BAD_REQUEST, message)),
            };
            let Some(seconds) = positive_integer(object.get("seconds")) else {
                return Some(failure(
                    StatusCode::BAD_REQUEST,
                    "seconds must be a positive integer",
                ));
            };
            blocking(host, move |host| host.arm_nosleep(seconds)).await
        }
        _ => {
            let object = match object_of(&body) {
                Ok(object) => object,
                Err(message) => return Some(failure(StatusCode::BAD_REQUEST, message)),
            };
            // Only an explicit `true` stops the agents.
            let stop_agents = object.get("stopAgents") == Some(&Value::Bool(true));
            host.restart_conductor(stop_agents).await
        }
    };
    Some(respond(answer))
}

/// Runs a service call that may touch the disk or spawn a process off the async workers.
async fn blocking<F>(host: Arc<dyn HostService>, call: F) -> WriteAnswer
where
    F: FnOnce(&dyn HostService) -> WriteAnswer + Send + 'static,
{
    match tokio::task::spawn_blocking(move || call(host.as_ref())).await {
        Ok(answer) => answer,
        Err(error) => {
            tracing::error!("host route failed: {error}");
            WriteAnswer::error(500, "internal error")
        }
    }
}

/// The body as a JSON object. An empty body is `{}`; anything that is not JSON, or not an
/// object, is the message of the 400 to answer.
fn object_of(body: &[u8]) -> Result<Map<String, Value>, &'static str> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err("request body must be a JSON object"),
        Err(_) => Err("request body must be valid JSON"),
    }
}

/// A JSON integer above zero.
fn positive_integer(value: Option<&Value>) -> Option<u64> {
    value?.as_u64().filter(|seconds| *seconds > 0)
}

/// The `limit` of the logs route, clamped to `1..=MAX_LIMIT`: `DEFAULT_LIMIT` when it is missing
/// or empty, `None` when it is not an integer.
fn limit_of(query: Option<&str>) -> Option<usize> {
    let raw = match form_value(query, "limit") {
        // Present, but not decodable.
        None if raw_value(query, "limit").is_some() => return None,
        None => return Some(DEFAULT_LIMIT),
        Some(raw) => raw,
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Some(DEFAULT_LIMIT);
    }
    let (negative, digits) = match raw.as_bytes().first() {
        Some(b'-') => (true, &raw[1..]),
        Some(b'+') => (false, &raw[1..]),
        _ => (false, raw),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if negative {
        return Some(1);
    }
    // More digits than a `u128` holds is far above the cap.
    let number = digits.parse::<u128>().unwrap_or(u128::MAX);
    Some(number.clamp(1, MAX_LIMIT as u128) as usize)
}

/// The raw value of the first parameter called `key`.
fn raw_value<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        pair.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

/// The first parameter called `key`, decoded as a form value. `None` when there is none, or
/// when it is not decodable.
fn form_value(query: Option<&str>, key: &str) -> Option<String> {
    let raw = raw_value(query, key)?;
    String::from_utf8(percent_decode(raw, true)).ok()
}

fn failure(status: StatusCode, message: &str) -> Response {
    json(status, &error_body(message))
}
