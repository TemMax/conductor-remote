//! The push routes: the config, subscribing a phone, unsubscribing it and the test push. Each
//! validates the request and hands it to the `NotifyService`.

use axum::body::Bytes;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use serde_json::{json, Map, Value};

use super::writes::object_of;
use super::{error_body, json};
use crate::contract::AppState;
use crate::notify::Subscription;

const BASE: &str = "/api/push";

/// Whether the request is one of the push routes, whatever its body.
pub fn is_push(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET => path == BASE,
        Method::POST => matches!(
            path,
            "/api/push/subscribe" | "/api/push/unsubscribe" | "/api/push/test"
        ),
        _ => false,
    }
}

/// Answers a push route, or `None` when the request is not one. The token has been checked.
pub async fn route(state: &AppState, method: &Method, path: &str, body: Bytes) -> Option<Response> {
    if !is_push(method, path) {
        return None;
    }
    let Some(notify) = state.notify.clone() else {
        return Some(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "notifications are unavailable",
        ));
    };
    if method == Method::GET {
        return Some(match notify.config() {
            Ok(config) => json(StatusCode::OK, &config),
            Err(error) => internal(&error),
        });
    }
    let object = match object_of(&body) {
        Ok(object) => object,
        Err(message) => return Some(failure(StatusCode::BAD_REQUEST, &message)),
    };
    let response = match path {
        "/api/push/subscribe" => {
            let (subscription, label) = match subscription_of(&object) {
                Ok(parsed) => parsed,
                Err(message) => return Some(failure(StatusCode::BAD_REQUEST, message)),
            };
            match notify.subscribe(subscription, label) {
                Ok((id, devices)) => json(
                    StatusCode::OK,
                    &json!({ "ok": true, "id": id, "devices": devices }),
                ),
                Err(error) => internal(&error),
            }
        }
        "/api/push/unsubscribe" => {
            let Some(endpoint) = text_of(&object, "endpoint") else {
                return Some(failure(StatusCode::BAD_REQUEST, "need the endpoint"));
            };
            match notify.unsubscribe(endpoint) {
                Ok((removed, devices)) => json(
                    StatusCode::OK,
                    &json!({ "ok": removed, "devices": devices }),
                ),
                Err(error) => internal(&error),
            }
        }
        _ => {
            let Some(id) = text_of(&object, "id") else {
                return Some(failure(StatusCode::BAD_REQUEST, "need the device id"));
            };
            match notify.test(id.to_owned()).await {
                Ok(()) => json(StatusCode::OK, &json!({ "ok": true })),
                Err(error) => json(
                    StatusCode::BAD_GATEWAY,
                    &json!({ "ok": false, "error": error }),
                ),
            }
        }
    };
    Some(response)
}

/// The subscription and the optional label, or the message of the 400 to answer.
fn subscription_of(
    object: &Map<String, Value>,
) -> Result<(Subscription, Option<String>), &'static str> {
    const NEED: &str = "need a subscription with endpoint and keys";
    let Some(Value::Object(subscription)) = object.get("subscription") else {
        return Err(NEED);
    };
    let Some(Value::Object(keys)) = subscription.get("keys") else {
        return Err(NEED);
    };
    let (Some(endpoint), Some(p256dh), Some(auth)) = (
        text_of(subscription, "endpoint"),
        text_of(keys, "p256dh"),
        text_of(keys, "auth"),
    ) else {
        return Err(NEED);
    };
    if !is_https(endpoint) {
        return Err("endpoint must be https");
    }
    let label = match object.get("label") {
        Some(Value::String(label)) => Some(label.clone()),
        _ => None,
    };
    Ok((
        Subscription {
            endpoint: endpoint.to_owned(),
            p256dh: p256dh.to_owned(),
            auth: auth.to_owned(),
        },
        label,
    ))
}

/// A field that is a string and not empty.
fn text_of<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    match object.get(field) {
        Some(Value::String(value)) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn is_https(endpoint: &str) -> bool {
    endpoint
        .as_bytes()
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case(b"https://"))
}

fn internal(error: &str) -> Response {
    tracing::error!("push route failed: {error}");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

fn failure(status: StatusCode, message: &str) -> Response {
    json(status, &error_body(message))
}
