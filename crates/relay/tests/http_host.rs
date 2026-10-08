//! The host routes over a fake `HostService`: the token gate, the missing service, the parsed
//! arguments each service method receives, every request the routes refuse themselves, and the
//! service's answer passing through.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Services, Token};
use conductor_remote::delivery::{BoxFuture, WriteAnswer};
use conductor_remote::host::HostService;
use conductor_remote::http::router;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "secret-token";

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Logs(Option<String>, Option<usize>),
    Settings,
    Nosleep,
    Arm(u64),
    Disarm,
    Restart(bool),
    Status,
}

/// Records every call and answers `reply`, or a 200 naming the method.
struct FakeHost {
    calls: Mutex<Vec<Call>>,
    reply: Mutex<Option<WriteAnswer>>,
}

impl FakeHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            reply: Mutex::new(None),
        })
    }

    /// Every service method answers `answer` from now on.
    fn replying(&self, answer: WriteAnswer) {
        *self.reply.lock().unwrap() = Some(answer);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn answer(&self, call: Call, name: &str) -> WriteAnswer {
        self.calls.lock().unwrap().push(call);
        self.reply
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| WriteAnswer::json(200, json!({ "from": name })))
    }
}

impl HostService for FakeHost {
    fn logs(&self, file: Option<String>, limit: Option<usize>) -> WriteAnswer {
        self.answer(Call::Logs(file, limit), "logs")
    }

    fn settings(&self) -> WriteAnswer {
        self.answer(Call::Settings, "settings")
    }

    fn nosleep(&self) -> WriteAnswer {
        self.answer(Call::Nosleep, "nosleep")
    }

    fn arm_nosleep(&self, seconds: u64) -> WriteAnswer {
        self.answer(Call::Arm(seconds), "arm")
    }

    fn disarm_nosleep(&self) -> WriteAnswer {
        self.answer(Call::Disarm, "disarm")
    }

    fn restart_conductor(&self, stop_agents: bool) -> BoxFuture<WriteAnswer> {
        let answer = self.answer(Call::Restart(stop_agents), "restart");
        Box::pin(async move { answer })
    }

    fn status(&self) -> WriteAnswer {
        self.answer(Call::Status, "status")
    }
}

fn app_with(host: Option<Arc<FakeHost>>) -> Router {
    router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
        assets: Arc::new(MemoryAssets::default()),
        reads: None,
        writes: None,
        notify: None,
        services: Services {
            host: host.map(|host| host as Arc<dyn HostService>),
            ..Default::default()
        },
    })
}

fn harness() -> (Router, Arc<FakeHost>) {
    let host = FakeHost::new();
    (app_with(Some(host.clone())), host)
}

fn request(method: Method, uri: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::from(body))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    request(Method::GET, uri, Vec::new())
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

/// Every host route, with a body where the route reads one.
fn routes() -> Vec<(Method, &'static str, &'static str)> {
    vec![
        (Method::GET, "/api/logs", ""),
        (Method::GET, "/api/settings", ""),
        (Method::GET, "/api/nosleep", ""),
        (Method::POST, "/api/nosleep", r#"{"seconds":60}"#),
        (Method::DELETE, "/api/nosleep", ""),
        (Method::POST, "/api/conductor/restart", "{}"),
    ]
}

/// Asks for the logs with `query` and asserts the service saw `limit`.
async fn assert_limit(query: &str, limit: usize) {
    let (app, host) = harness();
    let (status, _) = answer(app, get(&format!("/api/logs{query}"))).await;
    assert_eq!(status, StatusCode::OK, "{query}");
    assert_eq!(host.calls(), vec![Call::Logs(None, Some(limit))], "{query}");
}

/// Asks for the logs with `query` and asserts the 400 and that the service was not called.
async fn assert_limit_refused(query: &str) {
    let (app, host) = harness();
    let (status, text) = answer(app, get(&format!("/api/logs{query}"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    assert_eq!(text, r#"{"error":"limit must be a number"}"#, "{query}");
    assert!(host.calls().is_empty(), "{query}: the service was called");
}

/// Posts `body` to `uri` and asserts the 400 with `message` and that the service was not called.
async fn assert_refused(uri: &str, body: &str, message: &str) {
    let (app, host) = harness();
    let (status, text) = answer(app, post(uri, body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} {body}");
    assert_eq!(text, format!(r#"{{"error":"{message}"}}"#), "{uri} {body}");
    assert!(
        host.calls().is_empty(),
        "{uri} {body}: the service was called"
    );
}

// ---------------------------------------------------------------------------------------------
// The token gate and the missing service
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_host_route_needs_the_token() {
    for (method, uri, body) in routes() {
        let (app, host) = harness();
        let request = Request::builder()
            .method(method.clone())
            .uri(uri)
            .body(Body::from(body))
            .unwrap();
        let (status, text) = answer(app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(text, r#"{"error":"unauthorized"}"#);
        assert!(host.calls().is_empty(), "{method} {uri}");
    }
}

#[tokio::test]
async fn a_wrong_token_is_refused_too() {
    let (app, host) = harness();
    let request = Request::builder()
        .uri("/api/settings")
        .header(header::AUTHORIZATION, "Bearer nope")
        .body(Body::empty())
        .unwrap();
    let (status, text) = answer(app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(text, r#"{"error":"unauthorized"}"#);
    assert!(host.calls().is_empty());
}

#[tokio::test]
async fn the_token_query_parameter_opens_the_gate() {
    let (app, host) = harness();
    let request = Request::builder()
        .uri(format!("/api/logs?token={TOKEN}&limit=5"))
        .body(Body::empty())
        .unwrap();
    let (status, _) = answer(app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(host.calls(), vec![Call::Logs(None, Some(5))]);
}

#[tokio::test]
async fn without_a_host_service_every_host_route_is_unavailable() {
    let app = app_with(None);
    for (method, uri, body) in routes() {
        let (status, text) = answer(
            app.clone(),
            request(method.clone(), uri, body.as_bytes().to_vec()),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {uri}");
        assert_eq!(text, r#"{"error":"host services are unavailable"}"#);
    }
}

#[tokio::test]
async fn the_missing_service_is_reported_before_the_request_is_checked() {
    let app = app_with(None);
    for (uri, body) in [
        ("/api/nosleep", "{not json"),
        ("/api/nosleep", "{}"),
        ("/api/conductor/restart", "{not json"),
    ] {
        let (status, text) = answer(app.clone(), post(uri, body)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri} {body}");
        assert_eq!(text, r#"{"error":"host services are unavailable"}"#);
    }
    let (status, text) = answer(app, get("/api/logs?limit=abc")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(text, r#"{"error":"host services are unavailable"}"#);
}

// ---------------------------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn logs_default_to_300_lines() {
    assert_limit("", 300).await;
    let (app, host) = harness();
    answer(app, get("/api/logs?file=relay")).await;
    assert_eq!(
        host.calls(),
        vec![Call::Logs(Some("relay".to_owned()), Some(300))]
    );
}

#[tokio::test]
async fn logs_pass_the_file_and_the_limit() {
    let (app, host) = harness();
    let (status, text) = answer(app, get("/api/logs?file=relay&limit=50")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"from":"logs"}"#);
    assert_eq!(
        host.calls(),
        vec![Call::Logs(Some("relay".to_owned()), Some(50))]
    );
}

#[tokio::test]
async fn the_file_is_decoded_and_an_empty_one_is_none() {
    let (app, host) = harness();
    answer(app, get("/api/logs?file=relay%20old+log.txt")).await;
    assert_eq!(
        host.calls(),
        vec![Call::Logs(Some("relay old log.txt".to_owned()), Some(300))]
    );
    let (app, host) = harness();
    answer(app, get("/api/logs?file=&limit=7")).await;
    assert_eq!(host.calls(), vec![Call::Logs(None, Some(7))]);
}

#[tokio::test]
async fn the_limit_is_clamped_to_1_through_2000() {
    assert_limit("?limit=1", 1).await;
    assert_limit("?limit=2000", 2000).await;
    assert_limit("?limit=2001", 2000).await;
    assert_limit("?limit=0", 1).await;
    assert_limit("?limit=-5", 1).await;
    assert_limit("?limit=-0", 1).await;
    assert_limit("?limit=%2B40", 40).await;
    assert_limit("?limit=99999999999999999999999999999999999999999999", 2000).await;
    assert_limit("?limit=-99999999999999999999999999999999999999999999", 1).await;
}

#[tokio::test]
async fn an_empty_limit_is_the_default() {
    assert_limit("?limit=", 300).await;
    assert_limit("?file=&limit=", 300).await;
}

#[tokio::test]
async fn a_limit_that_is_not_an_integer_is_a_400() {
    for query in [
        "?limit=abc",
        "?limit=12abc",
        "?limit=12.5",
        "?limit=1e3",
        "?limit=0x10",
        "?limit=-",
        "?limit=%20x",
        "?limit=%FF",
        "?limit=NaN",
    ] {
        assert_limit_refused(query).await;
    }
}

#[tokio::test]
async fn only_the_first_limit_counts() {
    assert_limit("?limit=10&limit=abc", 10).await;
    assert_limit_refused("?limit=abc&limit=10").await;
}

#[tokio::test]
async fn a_bad_limit_is_refused_before_the_service_is_asked() {
    assert_limit_refused("?file=relay&limit=abc").await;
}

#[tokio::test]
async fn the_services_status_and_body_pass_through_for_logs() {
    let (app, host) = harness();
    host.replying(WriteAnswer::error(404, "unknown log file"));
    let (status, text) = answer(app, get("/api/logs?file=nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(text, r#"{"error":"unknown log file"}"#);
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn settings_are_the_services_answer() {
    let (app, host) = harness();
    let reply = json!({ "settings": {}, "nosleep": { "armed": false }, "screenLocked": null });
    host.replying(WriteAnswer::json(200, reply.clone()));
    let (status, text) = answer(app, get("/api/settings")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), reply);
    assert_eq!(host.calls(), vec![Call::Settings]);
}

#[tokio::test]
async fn a_service_failure_passes_through_for_settings() {
    let (app, host) = harness();
    host.replying(WriteAnswer::error(500, "settings are unreadable"));
    let (status, text) = answer(app, get("/api/settings")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(text, r#"{"error":"settings are unreadable"}"#);
}

// ---------------------------------------------------------------------------------------------
// Keep-awake
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn nosleep_state_is_read_with_a_get() {
    let (app, host) = harness();
    let (status, text) = answer(app, get("/api/nosleep")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"from":"nosleep"}"#);
    assert_eq!(host.calls(), vec![Call::Nosleep]);
}

#[tokio::test]
async fn nosleep_is_armed_for_the_seconds_given() {
    let (app, host) = harness();
    let (status, text) = answer(app, post("/api/nosleep", r#"{"seconds":3600}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"from":"arm"}"#);
    assert_eq!(host.calls(), vec![Call::Arm(3600)]);
}

#[tokio::test]
async fn nosleep_is_disarmed_with_a_delete() {
    let (app, host) = harness();
    let (status, text) = answer(app, request(Method::DELETE, "/api/nosleep", Vec::new())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"from":"disarm"}"#);
    assert_eq!(host.calls(), vec![Call::Disarm]);
}

#[tokio::test]
async fn seconds_must_be_a_positive_integer() {
    for body in [
        "{}",
        "",
        "   ",
        r#"{"seconds":null}"#,
        r#"{"seconds":0}"#,
        r#"{"seconds":-1}"#,
        r#"{"seconds":1.5}"#,
        r#"{"seconds":60.0}"#,
        r#"{"seconds":"60"}"#,
        r#"{"seconds":true}"#,
        r#"{"seconds":[60]}"#,
        r#"{"seconds":18446744073709551616}"#,
        r#"{"second":60}"#,
        r#"{"Seconds":60}"#,
    ] {
        assert_refused("/api/nosleep", body, "seconds must be a positive integer").await;
    }
}

#[tokio::test]
async fn a_body_that_is_not_an_object_has_no_seconds() {
    // Valid JSON that is not an object is refused with the object message.
    for body in ["[]", "60", "null", r#""60""#] {
        assert_refused("/api/nosleep", body, "request body must be a JSON object").await;
    }
}

#[tokio::test]
async fn nosleep_with_malformed_json_is_a_400() {
    for body in ["{", "{seconds:60}", r#"{"seconds":60"#, "nope"] {
        assert_refused("/api/nosleep", body, "request body must be valid JSON").await;
    }
}

#[tokio::test]
async fn the_largest_seconds_a_u64_holds_is_passed_on() {
    let (app, host) = harness();
    answer(
        app,
        post("/api/nosleep", r#"{"seconds":18446744073709551615}"#),
    )
    .await;
    assert_eq!(host.calls(), vec![Call::Arm(u64::MAX)]);
}

#[tokio::test]
async fn the_service_status_passes_through_for_every_nosleep_route() {
    for (method, body) in [
        (Method::GET, ""),
        (Method::POST, r#"{"seconds":5}"#),
        (Method::DELETE, ""),
    ] {
        let (app, host) = harness();
        host.replying(WriteAnswer::error(409, "busy"));
        let (status, text) = answer(
            app,
            request(method.clone(), "/api/nosleep", body.as_bytes().to_vec()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{method}");
        assert_eq!(text, r#"{"error":"busy"}"#, "{method}");
    }
}

#[tokio::test]
async fn retry_after_passes_through() {
    let (app, host) = harness();
    host.replying(WriteAnswer {
        status: 429,
        body: json!({ "error": "slow down" }),
        retry_after_secs: Some(7),
    });
    let response = app.oneshot(get("/api/settings")).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "7");
}

// ---------------------------------------------------------------------------------------------
// Restart
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn restart_leaves_the_agents_running_by_default() {
    for body in [
        "{}",
        "",
        " \n",
        r#"{"stopAgents":false}"#,
        r#"{"stopAgents":null}"#,
    ] {
        let (app, host) = harness();
        let (status, text) = answer(app, post("/api/conductor/restart", body)).await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        assert_eq!(text, r#"{"from":"restart"}"#);
        assert_eq!(host.calls(), vec![Call::Restart(false)], "{body:?}");
    }
}

#[tokio::test]
async fn restart_stops_the_agents_only_for_an_explicit_true() {
    let (app, host) = harness();
    answer(
        app,
        post("/api/conductor/restart", r#"{"stopAgents":true}"#),
    )
    .await;
    assert_eq!(host.calls(), vec![Call::Restart(true)]);

    for body in [
        r#"{"stopAgents":"true"}"#,
        r#"{"stopAgents":1}"#,
        r#"{"stopagents":true}"#,
    ] {
        let (app, host) = harness();
        answer(app, post("/api/conductor/restart", body)).await;
        assert_eq!(host.calls(), vec![Call::Restart(false)], "{body}");
    }
}

#[tokio::test]
async fn restart_with_malformed_json_is_a_400() {
    for body in ["{", "{stopAgents:true}", "nope"] {
        assert_refused(
            "/api/conductor/restart",
            body,
            "request body must be valid JSON",
        )
        .await;
    }
    assert_refused(
        "/api/conductor/restart",
        "[true]",
        "request body must be a JSON object",
    )
    .await;
}

#[tokio::test]
async fn the_service_status_passes_through_for_restart() {
    let (app, host) = harness();
    host.replying(WriteAnswer::error(502, "Conductor did not come back"));
    let (status, text) = answer(app, post("/api/conductor/restart", "{}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(text, r#"{"error":"Conductor did not come back"}"#);
}

// ---------------------------------------------------------------------------------------------
// Bodies and methods
// ---------------------------------------------------------------------------------------------

/// A JSON object padded with spaces to `len` bytes.
fn padded(len: usize, object: &str) -> String {
    format!("{object}{}", " ".repeat(len - object.len()))
}

#[tokio::test]
async fn a_body_of_64_kib_is_read_and_one_byte_more_is_413() {
    let limit = 64 << 10;
    for (uri, object, call) in [
        ("/api/nosleep", r#"{"seconds":9}"#, Call::Arm(9)),
        (
            "/api/conductor/restart",
            r#"{"stopAgents":true}"#,
            Call::Restart(true),
        ),
    ] {
        let (app, host) = harness();
        let (status, _) = answer(app, post(uri, &padded(limit, object))).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(host.calls(), vec![call], "{uri}");

        let (app, host) = harness();
        let (status, text) = answer(app, post(uri, &padded(limit + 1, object))).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
        assert_eq!(text, r#"{"error":"request body is too large"}"#);
        assert!(host.calls().is_empty(), "{uri}");
    }
}

#[tokio::test]
async fn other_methods_are_not_host_routes() {
    for (method, uri) in [
        (Method::POST, "/api/logs"),
        (Method::POST, "/api/settings"),
        (Method::DELETE, "/api/settings"),
        (Method::PUT, "/api/nosleep"),
        (Method::PATCH, "/api/nosleep"),
        (Method::GET, "/api/conductor/restart"),
        (Method::DELETE, "/api/conductor/restart"),
        (Method::GET, "/api/logs/extra"),
        (Method::GET, "/api/nosleep/now"),
    ] {
        let (app, host) = harness();
        let (status, text) = answer(app, request(method.clone(), uri, b"{}".to_vec())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(text, r#"{"error":"not found"}"#);
        assert!(host.calls().is_empty(), "{method} {uri}");
    }
}

// ---------------------------------------------------------------------------------------------
// The status
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_status_reaches_the_service_and_its_body_passes_through() {
    let (app, host) = harness();
    host.replying(WriteAnswer::json(200, json!({ "pid": 7, "port": 8790 })));
    let (status, text) = answer(app, get("/api/host/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"pid":7,"port":8790}"#);
    assert_eq!(host.calls(), vec![Call::Status]);
}

#[tokio::test]
async fn the_status_needs_the_token() {
    let (app, host) = harness();
    let request = Request::builder()
        .uri("/api/host/status")
        .body(Body::empty())
        .unwrap();
    let (status, text) = answer(app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(text, r#"{"error":"unauthorized"}"#);
    assert!(host.calls().is_empty());
}

#[tokio::test]
async fn without_a_host_service_the_status_is_unavailable() {
    let (status, text) = answer(app_with(None), get("/api/host/status")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(text, r#"{"error":"host services are unavailable"}"#);
}
