//! The dev-server routes over a fake `DevServerService`: the token gate, the missing service, the
//! arguments each service method receives, every request the routes refuse themselves, and the
//! service's answer passing through.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Priority, Services, Token};
use conductor_remote::delivery::{BoxFuture, WriteAnswer};
use conductor_remote::dev::DevServerService;
use conductor_remote::http::router;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const TOKEN: &str = "secret-token";
const URI: &str = "/api/workspaces/w1/dev-server";

#[derive(Clone, Debug, PartialEq)]
enum Call {
    State(String),
    Start(String, Option<String>, Priority),
    Stop(String, Priority),
}

/// Records every call and answers `reply`, or a 200 naming the method.
struct FakeDev {
    calls: Mutex<Vec<Call>>,
    reply: Mutex<Option<WriteAnswer>>,
}

impl FakeDev {
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

    fn answer(&self, call: Call, name: &str) -> BoxFuture<WriteAnswer> {
        self.calls.lock().unwrap().push(call);
        let answer = self
            .reply
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| WriteAnswer::json(200, json!({ "from": name })));
        Box::pin(async move { answer })
    }
}

impl DevServerService for FakeDev {
    fn state(&self, workspace_id: String) -> BoxFuture<WriteAnswer> {
        self.answer(Call::State(workspace_id), "state")
    }

    fn start(
        &self,
        workspace_id: String,
        run_config_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.answer(Call::Start(workspace_id, run_config_id, priority), "start")
    }

    fn stop(&self, workspace_id: String, priority: Priority) -> BoxFuture<WriteAnswer> {
        self.answer(Call::Stop(workspace_id, priority), "stop")
    }
}

fn app_with(dev: Option<Arc<FakeDev>>) -> Router {
    router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
        assets: Arc::new(MemoryAssets::default()),
        reads: None,
        writes: None,
        notify: None,
        services: Services {
            dev: dev.map(|dev| dev as Arc<dyn DevServerService>),
            ..Default::default()
        },
    })
}

fn harness() -> (Router, Arc<FakeDev>) {
    let dev = FakeDev::new();
    (app_with(Some(dev.clone())), dev)
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

fn delete(uri: &str) -> Request<Body> {
    request(Method::DELETE, uri, Vec::new())
}

/// The same request as an MCP client sends it.
fn from_mcp(request: Request<Body>) -> Request<Body> {
    let (mut parts, body) = request.into_parts();
    parts
        .headers
        .insert("x-relay-client", "mcp".parse().unwrap());
    Request::from_parts(parts, body)
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

/// Every dev-server route, with a body where the route reads one.
fn routes() -> Vec<(Method, &'static str)> {
    vec![
        (Method::GET, ""),
        (Method::POST, r#"{"runConfigId":"web"}"#),
        (Method::DELETE, ""),
    ]
}

/// Posts `body` and asserts the service saw `run_config_id`.
async fn assert_start(body: &str, run_config_id: Option<&str>) {
    let (app, dev) = harness();
    let (status, _) = answer(app, post(URI, body)).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        dev.calls(),
        vec![Call::Start(
            "w1".to_owned(),
            run_config_id.map(str::to_owned),
            Priority::Interactive
        )],
        "{body:?}"
    );
}

/// Posts `body` and asserts the 400 with `message` and that the service was not called.
async fn assert_refused(body: &str, message: &str) {
    let (app, dev) = harness();
    let (status, text) = answer(app, post(URI, body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(text, format!(r#"{{"error":"{message}"}}"#), "{body:?}");
    assert!(dev.calls().is_empty(), "{body:?}: the service was called");
}

// ---------------------------------------------------------------------------------------------
// The token gate and the missing service
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_dev_route_needs_the_token() {
    for (method, body) in routes() {
        let (app, dev) = harness();
        let request = Request::builder()
            .method(method.clone())
            .uri(URI)
            .body(Body::from(body))
            .unwrap();
        let (status, text) = answer(app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method}");
        assert_eq!(text, r#"{"error":"unauthorized"}"#);
        assert!(dev.calls().is_empty(), "{method}: the service was called");
    }
}

#[tokio::test]
async fn without_a_service_every_dev_route_is_unavailable() {
    for (method, body) in routes() {
        let (status, text) = answer(
            app_with(None),
            request(method.clone(), URI, body.as_bytes().to_vec()),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method}");
        assert_eq!(text, r#"{"error":"the dev server is unavailable"}"#);
    }
}

// ---------------------------------------------------------------------------------------------
// What the service receives
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_asks_for_the_state_of_the_workspace() {
    let (app, dev) = harness();
    let (status, text) = answer(app, get(URI)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"from":"state"}"#);
    assert_eq!(dev.calls(), vec![Call::State("w1".to_owned())]);
}

#[tokio::test]
async fn post_starts_the_dev_server_with_the_run_config() {
    let (app, dev) = harness();
    let (status, text) = answer(app, post(URI, r#"{"runConfigId":"web"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"from":"start"}"#);
    assert_eq!(
        dev.calls(),
        vec![Call::Start(
            "w1".to_owned(),
            Some("web".to_owned()),
            Priority::Interactive
        )]
    );
}

#[tokio::test]
async fn delete_stops_the_dev_server() {
    let (app, dev) = harness();
    let (status, text) = answer(app, delete(URI)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, r#"{"from":"stop"}"#);
    assert_eq!(
        dev.calls(),
        vec![Call::Stop("w1".to_owned(), Priority::Interactive)]
    );
}

#[tokio::test]
async fn delete_reads_no_body() {
    let (app, dev) = harness();
    let (status, _) = answer(app, request(Method::DELETE, URI, b"not json".to_vec())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        dev.calls(),
        vec![Call::Stop("w1".to_owned(), Priority::Interactive)]
    );
}

#[tokio::test]
async fn the_mcp_client_is_background() {
    let dev = FakeDev::new();
    for request in [
        from_mcp(get(URI)),
        from_mcp(post(URI, "{}")),
        from_mcp(delete(URI)),
    ] {
        answer(app_with(Some(dev.clone())), request).await;
    }
    assert_eq!(
        dev.calls(),
        vec![
            Call::State("w1".to_owned()),
            Call::Start("w1".to_owned(), None, Priority::Background),
            Call::Stop("w1".to_owned(), Priority::Background),
        ]
    );
}

#[tokio::test]
async fn the_workspace_id_is_percent_decoded() {
    let (app, dev) = harness();
    answer(app, get("/api/workspaces/w%201/dev-server")).await;
    assert_eq!(dev.calls(), vec![Call::State("w 1".to_owned())]);
}

// ---------------------------------------------------------------------------------------------
// The POST body
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_body_starts_without_a_run_config() {
    assert_start("", None).await;
    assert_start("  \n", None).await;
}

#[tokio::test]
async fn an_absent_or_null_run_config_is_none() {
    assert_start("{}", None).await;
    assert_start(r#"{"runConfigId":null}"#, None).await;
    assert_start(r#"{"other":1}"#, None).await;
}

#[tokio::test]
async fn a_run_config_is_trimmed() {
    assert_start(r#"{"runConfigId":"web"}"#, Some("web")).await;
    assert_start(r#"{"runConfigId":"  web\n"}"#, Some("web")).await;
}

#[tokio::test]
async fn a_run_config_that_is_not_a_non_empty_string_is_refused() {
    for body in [
        r#"{"runConfigId":""}"#,
        r#"{"runConfigId":"   "}"#,
        r#"{"runConfigId":5}"#,
        r#"{"runConfigId":true}"#,
        r#"{"runConfigId":["web"]}"#,
        r#"{"runConfigId":{}}"#,
    ] {
        assert_refused(body, "runConfigId must be a non-empty string").await;
    }
}

#[tokio::test]
async fn a_body_that_is_not_an_object_is_refused() {
    for body in ["[]", "5", "null", r#""web""#] {
        assert_refused(body, "request body must be a JSON object").await;
    }
}

#[tokio::test]
async fn a_body_that_is_not_json_is_refused() {
    for body in ["{", "web", r#"{"runConfigId":"#] {
        assert_refused(body, "request body must be valid JSON").await;
    }
}

#[tokio::test]
async fn a_body_over_the_host_cap_is_refused() {
    let (app, dev) = harness();
    let body = format!(r#"{{"runConfigId":"{}"}}"#, "a".repeat(64 << 10));
    let (status, text) = answer(app, post(URI, &body)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(text, r#"{"error":"request body is too large"}"#);
    assert!(dev.calls().is_empty());
}

// ---------------------------------------------------------------------------------------------
// The service's answer
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_service_status_and_body_pass_through() {
    for (status, body) in [
        (
            200,
            json!({ "ok": true, "state": "running", "changed": true }),
        ),
        (409, json!({ "error": "already running" })),
        (
            502,
            json!({ "error": "tailscale failed", "detail": ["a", 1] }),
        ),
    ] {
        for (method, request_body) in routes() {
            let (app, dev) = harness();
            dev.replying(WriteAnswer::json(status, body.clone()));
            let (got, text) = answer(
                app,
                request(method.clone(), URI, request_body.as_bytes().to_vec()),
            )
            .await;
            assert_eq!(got.as_u16(), status, "{method}");
            assert_eq!(text, body.to_string(), "{method}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Neighbouring routes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_longer_path_is_not_found() {
    for method in [Method::GET, Method::POST, Method::DELETE] {
        let (app, dev) = harness();
        let (status, text) = answer(
            app,
            request(
                method.clone(),
                "/api/workspaces/x/dev-server/extra",
                Vec::new(),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
        assert_eq!(text, r#"{"error":"not found"}"#);
        assert!(dev.calls().is_empty());
    }
}

#[tokio::test]
async fn other_methods_are_not_found() {
    for method in [Method::PUT, Method::PATCH] {
        let (app, dev) = harness();
        let (status, _) = answer(app, request(method.clone(), URI, Vec::new())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
        assert!(dev.calls().is_empty());
    }
}

#[tokio::test]
async fn deleting_a_workspace_prompt_still_reaches_the_first_prompt_dismissal() {
    // No write service is wired, so the dismissal answers with the writes' own 503 rather than
    // the dev server's; the dev service is not asked.
    let (app, dev) = harness();
    let (status, text) = answer(app, delete("/api/workspaces/w1/prompt")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(text, r#"{"error":"writes are unavailable"}"#);
    assert!(dev.calls().is_empty());
}
