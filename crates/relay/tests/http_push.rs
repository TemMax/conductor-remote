//! The push routes over a fake `NotifyService`: the token gate, the parsed fields each service
//! method receives, every request the routes refuse themselves, and the service's answer.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Token};
use conductor_remote::delivery::BoxFuture;
use conductor_remote::http::router;
use conductor_remote::notify::{DeviceInfo, NotifyService, PushConfig, Subscription};
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "secret-token";

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Subscribe(Subscription, Option<String>),
    Unsubscribe(String),
    Test(String),
}

struct FakeNotify {
    calls: Mutex<Vec<Call>>,
    fail: Mutex<Option<String>>,
}

impl FakeNotify {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
        })
    }

    /// Every service method answers `Err(message)` from now on.
    fn failing(&self, message: &str) {
        *self.fail.lock().unwrap() = Some(message.to_owned());
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn outcome<T>(&self, call: Call, ok: T) -> Result<T, String> {
        self.calls.lock().unwrap().push(call);
        match self.fail.lock().unwrap().clone() {
            Some(message) => Err(message),
            None => Ok(ok),
        }
    }
}

fn device() -> DeviceInfo {
    DeviceInfo {
        id: "d1".to_owned(),
        label: "phone".to_owned(),
        created_at: 10,
        last_ok_at: None,
        last_error: None,
        failures: 0,
    }
}

impl NotifyService for FakeNotify {
    fn config(&self) -> Result<PushConfig, String> {
        if let Some(message) = self.fail.lock().unwrap().clone() {
            return Err(message);
        }
        Ok(PushConfig {
            enabled: true,
            public_key: "pk".to_owned(),
            devices: vec![device()],
        })
    }

    fn subscribe(
        &self,
        subscription: Subscription,
        label: Option<String>,
    ) -> Result<(String, Vec<DeviceInfo>), String> {
        self.outcome(
            Call::Subscribe(subscription, label),
            ("d1".to_owned(), vec![device()]),
        )
    }

    fn unsubscribe(&self, endpoint: &str) -> Result<(bool, Vec<DeviceInfo>), String> {
        self.outcome(Call::Unsubscribe(endpoint.to_owned()), (true, Vec::new()))
    }

    fn test(&self, device_id: String) -> BoxFuture<Result<(), String>> {
        let result = self.outcome(Call::Test(device_id), ());
        Box::pin(async move { result })
    }

    fn note_viewing(&self, _device_id: &str, _session_id: &str) {}
}

fn app_with(notify: Option<Arc<FakeNotify>>) -> Router {
    router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
        assets: Arc::new(MemoryAssets::default()),
        reads: None,
        writes: None,
        notify: notify.map(|notify| notify as Arc<dyn NotifyService>),
        services: Default::default(),
    })
}

fn app() -> (Router, Arc<FakeNotify>) {
    let notify = FakeNotify::new();
    (app_with(Some(notify.clone())), notify)
}

fn request(method: Method, uri: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::from(body))
        .unwrap()
}

fn post(uri: &str, body: &str) -> Request<Body> {
    request(Method::POST, uri, body.as_bytes().to_vec())
}

async fn text(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn answer(app: Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    let status = response.status();
    (status, text(response).await)
}

fn subscription(endpoint: &str) -> Value {
    json!({ "endpoint": endpoint, "keys": { "p256dh": "key", "auth": "secret" } })
}

/// Posts `body` and asserts the 400 with `message` and that the service was not called.
async fn assert_refused(uri: &str, body: &str, message: &str) {
    let (app, notify) = app();
    let (status, text) = answer(app, post(uri, body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} {body}");
    assert_eq!(text, format!(r#"{{"error":"{message}"}}"#), "{uri} {body}");
    assert!(notify.calls().is_empty(), "the service was called");
}

// ---------------------------------------------------------------------------------------------
// The token gate and the missing service
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_push_route_needs_the_token() {
    for (method, uri) in [
        (Method::GET, "/api/push"),
        (Method::POST, "/api/push/subscribe"),
        (Method::POST, "/api/push/unsubscribe"),
        (Method::POST, "/api/push/test"),
    ] {
        let (app, notify) = app();
        let request = Request::builder()
            .method(method.clone())
            .uri(uri)
            .body(Body::from("{}"))
            .unwrap();
        let (status, text) = answer(app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(text, r#"{"error":"unauthorized"}"#);
        assert!(notify.calls().is_empty());
    }
}

#[tokio::test]
async fn without_a_notifier_every_push_route_is_unavailable() {
    let app = app_with(None);
    for (method, uri) in [
        (Method::GET, "/api/push"),
        (Method::POST, "/api/push/subscribe"),
        (Method::POST, "/api/push/unsubscribe"),
        (Method::POST, "/api/push/test"),
    ] {
        let (status, text) =
            answer(app.clone(), request(method.clone(), uri, b"{}".to_vec())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {uri}");
        assert_eq!(text, r#"{"error":"notifications are unavailable"}"#);
    }
}

// ---------------------------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_config_is_the_push_config_json() {
    let (app, _) = app();
    let (status, text) = answer(app, request(Method::GET, "/api/push", Vec::new())).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body,
        json!({
            "enabled": true,
            "publicKey": "pk",
            "devices": [{
                "id": "d1",
                "label": "phone",
                "createdAt": 10,
                "lastOkAt": null,
                "lastError": null,
                "failures": 0
            }]
        })
    );
}

#[tokio::test]
async fn a_config_error_is_an_internal_error() {
    let (app, notify) = app();
    notify.failing("the database is locked");
    let (status, text) = answer(app, request(Method::GET, "/api/push", Vec::new())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(text, r#"{"error":"internal error"}"#);
}

// ---------------------------------------------------------------------------------------------
// Subscribe
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn subscribe_answers_the_id_and_the_devices() {
    let (app, notify) = app();
    let body = json!({ "subscription": subscription("https://push.example/abc") });
    let (status, text) = answer(app, post("/api/push/subscribe", &body.to_string())).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["id"], json!("d1"));
    assert_eq!(body["devices"][0]["id"], json!("d1"));
    assert_eq!(
        notify.calls(),
        vec![Call::Subscribe(
            Subscription {
                endpoint: "https://push.example/abc".to_owned(),
                p256dh: "key".to_owned(),
                auth: "secret".to_owned(),
            },
            None
        )]
    );
}

#[tokio::test]
async fn the_subscribe_label_is_passed_through() {
    let (app, notify) = app();
    let body =
        json!({ "subscription": subscription("https://push.example/abc"), "label": "Pixel" });
    let (status, _) = answer(app, post("/api/push/subscribe", &body.to_string())).await;
    assert_eq!(status, StatusCode::OK);
    let calls = notify.calls();
    let [Call::Subscribe(_, label)] = calls.as_slice() else {
        panic!("expected one subscribe call");
    };
    assert_eq!(label.as_deref(), Some("Pixel"));
}

#[tokio::test]
async fn the_https_scheme_is_matched_without_case() {
    let (app, notify) = app();
    let body = json!({ "subscription": subscription("HTTPS://push.example/abc") });
    let (status, _) = answer(app, post("/api/push/subscribe", &body.to_string())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(notify.calls().len(), 1);
}

#[tokio::test]
async fn subscribe_needs_endpoint_and_keys() {
    let message = "need a subscription with endpoint and keys";
    let cases = [
        json!({}),
        json!({ "subscription": null }),
        json!({ "subscription": "https://push.example/abc" }),
        json!({ "subscription": { "keys": { "p256dh": "k", "auth": "a" } } }),
        json!({ "subscription": { "endpoint": "https://p.example/a" } }),
        json!({ "subscription": { "endpoint": "https://p.example/a", "keys": "k" } }),
        json!({ "subscription": { "endpoint": "https://p.example/a", "keys": { "auth": "a" } } }),
        json!({ "subscription": { "endpoint": "https://p.example/a", "keys": { "p256dh": "k" } } }),
        json!({ "subscription": { "endpoint": 5, "keys": { "p256dh": "k", "auth": "a" } } }),
        json!({ "subscription": { "endpoint": "https://p.example/a", "keys": { "p256dh": 1, "auth": "a" } } }),
        json!({ "subscription": { "endpoint": "https://p.example/a", "keys": { "p256dh": "k", "auth": [] } } }),
    ];
    for body in cases {
        assert_refused("/api/push/subscribe", &body.to_string(), message).await;
    }
}

#[tokio::test]
async fn subscribe_endpoint_must_be_https() {
    for endpoint in [
        "http://push.example/abc",
        "ftp://x",
        "push.example",
        "https:/",
    ] {
        let body = json!({ "subscription": subscription(endpoint) });
        assert_refused(
            "/api/push/subscribe",
            &body.to_string(),
            "endpoint must be https",
        )
        .await;
    }
}

#[tokio::test]
async fn a_subscribe_error_is_an_internal_error() {
    let (app, notify) = app();
    notify.failing("disk full");
    let body = json!({ "subscription": subscription("https://push.example/abc") });
    let (status, text) = answer(app, post("/api/push/subscribe", &body.to_string())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(text, r#"{"error":"internal error"}"#);
}

// ---------------------------------------------------------------------------------------------
// Unsubscribe
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn unsubscribe_answers_whether_a_device_was_removed() {
    let (app, notify) = app();
    let (status, text) = answer(
        app,
        post(
            "/api/push/unsubscribe",
            r#"{"endpoint":"https://push.example/abc"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"ok":true,"devices":[]}"#);
    assert_eq!(
        notify.calls(),
        vec![Call::Unsubscribe("https://push.example/abc".to_owned())]
    );
}

#[tokio::test]
async fn unsubscribe_needs_the_endpoint() {
    for body in [r#"{}"#, r#"{"endpoint":5}"#, r#"{"endpoint":""}"#] {
        assert_refused("/api/push/unsubscribe", body, "need the endpoint").await;
    }
}

#[tokio::test]
async fn an_unsubscribe_error_is_an_internal_error() {
    let (app, notify) = app();
    notify.failing("disk full");
    let (status, text) = answer(app, post("/api/push/unsubscribe", r#"{"endpoint":"e"}"#)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(text, r#"{"error":"internal error"}"#);
}

// ---------------------------------------------------------------------------------------------
// Test push
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_test_push_answers_ok() {
    let (app, notify) = app();
    let (status, text) = answer(app, post("/api/push/test", r#"{"id":"d1"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"ok":true}"#);
    assert_eq!(notify.calls(), vec![Call::Test("d1".to_owned())]);
}

#[tokio::test]
async fn a_failed_test_push_is_a_bad_gateway() {
    let (app, notify) = app();
    notify.failing("the push service said 410");
    let (status, text) = answer(app, post("/api/push/test", r#"{"id":"d1"}"#)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(text, r#"{"ok":false,"error":"the push service said 410"}"#);
    assert_eq!(notify.calls().len(), 1);
}

#[tokio::test]
async fn the_test_push_needs_the_device_id() {
    for body in [r#"{}"#, r#"{"id":7}"#, r#"{"id":""}"#] {
        assert_refused("/api/push/test", body, "need the device id").await;
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_body_must_be_a_json_object() {
    for uri in [
        "/api/push/subscribe",
        "/api/push/unsubscribe",
        "/api/push/test",
    ] {
        assert_refused(uri, "{nope", "request body must be valid JSON").await;
        assert_refused(uri, "", "request body must be valid JSON").await;
        assert_refused(uri, "[]", "request body must be a JSON object").await;
        assert_refused(uri, r#""text""#, "request body must be a JSON object").await;
    }
}

#[tokio::test]
async fn a_body_over_64_kib_is_too_large() {
    let (app, notify) = app();
    let mut body = br#"{"id":""#.to_vec();
    body.resize((64 << 10) + 1, b'a');
    let (status, text) = answer(app, request(Method::POST, "/api/push/test", body)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(text, r#"{"error":"request body is too large"}"#);
    assert!(notify.calls().is_empty());
}

#[tokio::test]
async fn a_body_of_exactly_64_kib_is_read() {
    let (app, notify) = app();
    let prefix = br#"{"id":"d1","pad":""#;
    let suffix = br#""}"#;
    let mut body = prefix.to_vec();
    body.resize((64 << 10) - suffix.len(), b'a');
    body.extend_from_slice(suffix);
    assert_eq!(body.len(), 64 << 10);
    let (status, _) = answer(app, request(Method::POST, "/api/push/test", body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(notify.calls().len(), 1);
}

#[tokio::test]
async fn other_methods_on_the_push_paths_are_not_found() {
    for (method, uri) in [
        (Method::POST, "/api/push"),
        (Method::GET, "/api/push/subscribe"),
        (Method::DELETE, "/api/push/test"),
        (Method::PUT, "/api/push/unsubscribe"),
        (Method::POST, "/api/push/other"),
    ] {
        let (app, notify) = app();
        let (status, text) = answer(app, request(method.clone(), uri, b"{}".to_vec())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(text, r#"{"error":"not found"}"#);
        assert!(notify.calls().is_empty());
    }
}
