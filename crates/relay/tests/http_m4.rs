//! The milestone 4 routes over fake `WriteService` and `PrefsService` implementations: the token
//! gate, the attachment bodies and their 25 MiB cap, every field the routes parse, every request
//! they refuse themselves (400, 413, 501, 503) and the services' answers passing through.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::body::Bytes;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::agent::{AgentPatch, Effort};
use conductor_remote::contract::{
    AppState, ConductorStatus, PrefsService, Priority, Services, Token,
};
use conductor_remote::delivery::{
    BoxFuture, CreateRequest, SendRequest, SplitDestination, SplitRequest, WriteAnswer,
    WriteService, STRATEGY,
};
use conductor_remote::http::router;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "secret-token";

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Send(SendRequest),
    Stop {
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    },
    NewChat {
        workspace_id: String,
        priority: Priority,
    },
    Dismiss {
        session_id: String,
    },
    Upload {
        session_id: String,
        workspace_id: Option<String>,
        name: String,
        bytes: Bytes,
    },
    Stage {
        name: String,
        bytes: Bytes,
    },
    DiscardStaged {
        stage_id: String,
    },
    Merge {
        workspace_id: String,
    },
    Restore {
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    },
    JoinHistory {
        session_id: String,
        workspace_id: String,
        previous_session_id: String,
    },
    Split {
        session_id: String,
        request: SplitRequest,
        priority: Priority,
    },
    Create {
        request: CreateRequest,
        priority: Priority,
    },
    DismissFirstPrompt {
        workspace_id: String,
    },
}

struct FakeWrites {
    calls: Mutex<Vec<Call>>,
    answer: Mutex<WriteAnswer>,
}

impl FakeWrites {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            answer: Mutex::new(WriteAnswer::json(
                200,
                json!({ "ok": true, "strategy": STRATEGY }),
            )),
        })
    }

    fn answering(&self, answer: WriteAnswer) {
        *self.answer.lock().unwrap() = answer;
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: Call) -> BoxFuture<WriteAnswer> {
        self.calls.lock().unwrap().push(call);
        let answer = self.answer.lock().unwrap().clone();
        Box::pin(async move { answer })
    }
}

impl WriteService for FakeWrites {
    fn available(&self) -> bool {
        true
    }

    fn send_prompt(&self, request: SendRequest) -> BoxFuture<WriteAnswer> {
        self.record(Call::Send(request))
    }

    fn stop_turn(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::Stop {
            session_id,
            workspace_id,
            priority,
        })
    }

    fn new_chat(&self, workspace_id: String, priority: Priority) -> BoxFuture<WriteAnswer> {
        self.record(Call::NewChat {
            workspace_id,
            priority,
        })
    }

    fn parked_prompts(&self) -> Vec<Value> {
        Vec::new()
    }

    fn dismiss_parked(&self, session_id: String) -> BoxFuture<WriteAnswer> {
        self.record(Call::Dismiss { session_id })
    }

    fn upload_attachment(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        name: String,
        bytes: Bytes,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::Upload {
            session_id,
            workspace_id,
            name,
            bytes,
        })
    }

    fn stage_attachment(&self, name: String, bytes: Bytes) -> BoxFuture<WriteAnswer> {
        self.record(Call::Stage { name, bytes })
    }

    fn discard_staged(&self, stage_id: String) -> BoxFuture<WriteAnswer> {
        self.record(Call::DiscardStaged { stage_id })
    }

    fn merge(&self, workspace_id: String) -> BoxFuture<WriteAnswer> {
        self.record(Call::Merge { workspace_id })
    }

    fn restore_chat(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::Restore {
            session_id,
            workspace_id,
            priority,
        })
    }

    fn join_history(
        &self,
        session_id: String,
        workspace_id: String,
        previous_session_id: String,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::JoinHistory {
            session_id,
            workspace_id,
            previous_session_id,
        })
    }

    fn split_chat(
        &self,
        session_id: String,
        request: SplitRequest,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::Split {
            session_id,
            request,
            priority,
        })
    }

    fn create_workspace(
        &self,
        request: CreateRequest,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::Create { request, priority })
    }

    fn dismiss_first_prompt(&self, workspace_id: String) -> BoxFuture<WriteAnswer> {
        self.record(Call::DismissFirstPrompt { workspace_id })
    }

    fn pending_prompts(&self) -> Vec<Value> {
        Vec::new()
    }

    fn chat_history(&self, _workspace_id: &str) -> Value {
        json!({})
    }
}

/// The one call the service received: a one-element array to destructure.
fn only_call(writes: &FakeWrites) -> [Call; 1] {
    let calls = writes.calls();
    assert_eq!(calls.len(), 1, "expected exactly one service call");
    calls.try_into().unwrap()
}

struct FakePrefs {
    document: Mutex<Value>,
    patches: Mutex<Vec<Value>>,
    outcome: Mutex<Result<Value, String>>,
}

impl FakePrefs {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            document: Mutex::new(json!({ "readMarks": {}, "drafts": {} })),
            patches: Mutex::new(Vec::new()),
            outcome: Mutex::new(Ok(json!({ "readMarks": { "s1": 4 }, "drafts": {} }))),
        })
    }

    fn patching(&self, outcome: Result<Value, String>) {
        *self.outcome.lock().unwrap() = outcome;
    }

    fn patches(&self) -> Vec<Value> {
        self.patches.lock().unwrap().clone()
    }
}

impl PrefsService for FakePrefs {
    fn get(&self) -> Value {
        self.document.lock().unwrap().clone()
    }

    fn patch(&self, patch: Value) -> Result<Value, String> {
        self.patches.lock().unwrap().push(patch);
        self.outcome.lock().unwrap().clone()
    }
}

fn state(writes: Option<Arc<FakeWrites>>, prefs: Option<Arc<FakePrefs>>) -> Router {
    router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
        assets: Arc::new(MemoryAssets::default()),
        reads: None,
        writes: writes.map(|writes| writes as Arc<dyn WriteService>),
        notify: None,
        services: Services {
            prefs: prefs.map(|prefs| prefs as Arc<dyn PrefsService>),
            host: None,
            dev: None,
        },
    })
}

fn app() -> (Router, Arc<FakeWrites>) {
    let writes = FakeWrites::new();
    (state(Some(writes.clone()), None), writes)
}

fn prefs_app() -> (Router, Arc<FakePrefs>) {
    let prefs = FakePrefs::new();
    (state(None, Some(prefs.clone())), prefs)
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body)).unwrap()
}

fn post(uri: &str, headers: &[(&str, &str)], body: &str) -> Request<Body> {
    request(Method::POST, uri, headers, body.as_bytes().to_vec())
}

fn upload(uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Request<Body> {
    request(Method::POST, uri, headers, body)
}

fn delete(uri: &str) -> Request<Body> {
    request(Method::DELETE, uri, &[], Vec::new())
}

async fn text(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn header_of(response: &Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_str()
        .unwrap()
        .to_owned()
}

fn error_text(message: &str) -> String {
    json!({ "error": message }).to_string()
}

/// Sends the request and asserts the answer is exactly `status` and `{"error": message}` as
/// JSON, and that the service was not called.
async fn assert_refused(
    app: Router,
    writes: &FakeWrites,
    request: Request<Body>,
    status: StatusCode,
    message: &str,
) {
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), status, "{message}");
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(text(response).await, error_text(message));
    assert!(
        writes.calls().is_empty(),
        "the service was called: {message}"
    );
}

async fn assert_post_refused(uri: &str, body: &str, status: StatusCode, message: &str) {
    let (app, writes) = app();
    assert_refused(app, &writes, post(uri, &[], body), status, message).await;
}

// ---------------------------------------------------------------------------------------------
// The token gate and the missing services
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_new_route_needs_the_token() {
    let routes = [
        (Method::POST, "/api/sessions/s1/attachments"),
        (Method::POST, "/api/attachments"),
        (Method::DELETE, "/api/attachments/abc123"),
        (Method::POST, "/api/workspaces/w1/merge"),
        (Method::POST, "/api/sessions/s1/restore"),
        (Method::POST, "/api/sessions/s1/history"),
        (Method::POST, "/api/sessions/s1/split"),
        (Method::POST, "/api/workspaces"),
        (Method::DELETE, "/api/workspaces/w1/prompt"),
        (Method::GET, "/api/prefs"),
        (Method::PATCH, "/api/prefs"),
    ];
    for (method, uri) in routes {
        let writes = FakeWrites::new();
        let prefs = FakePrefs::new();
        let app = state(Some(writes.clone()), Some(prefs.clone()));
        let request = Request::builder()
            .method(method.clone())
            .uri(uri)
            .body(Body::from("{}"))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri}"
        );
        assert_eq!(text(response).await, error_text("unauthorized"));
        assert!(writes.calls().is_empty(), "{method} {uri}");
        assert!(prefs.patches().is_empty(), "{method} {uri}");
    }
}

#[tokio::test]
async fn without_a_write_service_every_write_route_is_unavailable() {
    let routes = [
        (Method::POST, "/api/sessions/s1/attachments"),
        (Method::POST, "/api/attachments"),
        (Method::DELETE, "/api/attachments/abc123"),
        (Method::POST, "/api/workspaces/w1/merge"),
        (Method::POST, "/api/sessions/s1/restore"),
        (Method::POST, "/api/sessions/s1/history"),
        (Method::POST, "/api/sessions/s1/split"),
        (Method::POST, "/api/workspaces"),
        (Method::DELETE, "/api/workspaces/w1/prompt"),
    ];
    for (method, uri) in routes {
        let app = state(None, None);
        let response = app
            .oneshot(request(method.clone(), uri, &[], b"{}".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {uri}"
        );
        assert_eq!(header_of(&response, "content-type"), "application/json");
        assert_eq!(text(response).await, error_text("writes are unavailable"));
    }
}

#[tokio::test]
async fn without_a_prefs_service_both_prefs_routes_are_unavailable() {
    for method in [Method::GET, Method::PATCH] {
        let (app, _) = app();
        let response = app
            .oneshot(request(method.clone(), "/api/prefs", &[], b"{}".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method}"
        );
        assert_eq!(
            text(response).await,
            error_text("preferences are unavailable")
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_upload_reaches_the_service_with_its_name_workspace_and_bytes() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(
        200,
        json!({ "ok": true, "attachment": { "name": "a b.txt" } }),
    ));
    let response = app
        .oneshot(upload(
            "/api/sessions/s%201/attachments?workspaceId=w%2F1+x&other=1",
            &[
                ("x-attachment-name", "a%20b+c%C3%A9.txt"),
                ("content-type", "application/octet-stream"),
            ],
            vec![0, 1, 2, 255],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(
        text(response).await,
        json!({ "ok": true, "attachment": { "name": "a b.txt" } }).to_string()
    );
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Upload {
            session_id: "s 1".to_owned(),
            workspace_id: Some("w/1 x".to_owned()),
            name: "a b+cé.txt".to_owned(),
            bytes: Bytes::from_static(&[0, 1, 2, 255]),
        }
    );
}

#[tokio::test]
async fn an_upload_without_a_workspace_id_passes_none() {
    let (app, writes) = app();
    app.oneshot(upload(
        "/api/sessions/s1/attachments",
        &[("x-attachment-name", "f.txt")],
        b"data".to_vec(),
    ))
    .await
    .unwrap();
    let [Call::Upload { workspace_id, .. }] = only_call(&writes) else {
        panic!("not an upload");
    };
    assert_eq!(workspace_id, None);
}

#[tokio::test]
async fn a_missing_or_undecodable_name_reaches_the_service_as_empty() {
    for headers in [
        Vec::new(),
        vec![("x-attachment-name", "%FF%FE")],
        vec![("x-attachment-name", "")],
    ] {
        let (app, writes) = app();
        writes.answering(WriteAnswer::error(400, "missing attachment name"));
        let response = app
            .oneshot(upload(
                "/api/sessions/s1/attachments",
                &headers,
                b"x".to_vec(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{headers:?}");
        assert_eq!(text(response).await, error_text("missing attachment name"));
        let [Call::Upload { name, .. }] = only_call(&writes) else {
            panic!("not an upload");
        };
        assert_eq!(name, "", "{headers:?}");
    }
}

#[tokio::test]
async fn staging_reaches_the_service_and_its_answer_passes_through() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(
        201,
        json!({ "ok": true, "attachment": { "stageId": "abc123" } }),
    ));
    let response = app
        .oneshot(upload(
            "/api/attachments?workspaceId=ignored",
            &[("x-attachment-name", "notes%20v2.md")],
            b"# notes".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        text(response).await,
        json!({ "ok": true, "attachment": { "stageId": "abc123" } }).to_string()
    );
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Stage {
            name: "notes v2.md".to_owned(),
            bytes: Bytes::from_static(b"# notes"),
        }
    );
}

#[tokio::test]
async fn an_empty_upload_body_reaches_the_service() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::error(400, "empty attachment"));
    let response = app
        .oneshot(upload(
            "/api/attachments",
            &[("x-attachment-name", "f")],
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let [Call::Stage { bytes, .. }] = only_call(&writes) else {
        panic!("not a stage");
    };
    assert!(bytes.is_empty());
}

const LIMIT: usize = 25 << 20;

#[tokio::test]
async fn a_declared_length_over_the_cap_is_refused_before_the_body_is_read() {
    for uri in ["/api/sessions/s1/attachments", "/api/attachments"] {
        let (app, writes) = app();
        let length = (LIMIT + 1).to_string();
        assert_refused(
            app,
            &writes,
            upload(
                uri,
                &[("x-attachment-name", "big"), ("content-length", &length)],
                b"tiny".to_vec(),
            ),
            StatusCode::PAYLOAD_TOO_LARGE,
            "attachments are limited to 25 MB",
        )
        .await;
    }
}

#[tokio::test]
async fn a_body_over_the_cap_without_a_content_length_is_refused() {
    for uri in ["/api/sessions/s1/attachments", "/api/attachments"] {
        let (app, writes) = app();
        assert_refused(
            app,
            &writes,
            upload(uri, &[("x-attachment-name", "big")], vec![7; LIMIT + 1]),
            StatusCode::PAYLOAD_TOO_LARGE,
            "attachments are limited to 25 MB",
        )
        .await;
    }
}

#[tokio::test]
async fn a_body_of_exactly_the_cap_is_accepted() {
    let (app, writes) = app();
    let length = LIMIT.to_string();
    let response = app
        .oneshot(upload(
            "/api/attachments",
            &[("x-attachment-name", "edge"), ("content-length", &length)],
            vec![9; LIMIT],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let [Call::Stage { bytes, .. }] = only_call(&writes) else {
        panic!("not a stage");
    };
    assert_eq!(bytes.len(), LIMIT);
}

#[tokio::test]
async fn deleting_a_staged_attachment_reaches_the_service() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(404, json!({ "ok": true })));
    let response = app
        .oneshot(delete("/api/attachments/ab%20c123"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(response).await, json!({ "ok": true }).to_string());
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::DiscardStaged {
            stage_id: "ab c123".to_owned()
        }
    );
}

#[tokio::test]
async fn an_unavailable_service_is_refused_before_the_upload_body_is_read() {
    let app = state(None, None);
    let response = app
        .oneshot(upload(
            "/api/attachments",
            &[("x-attachment-name", "f")],
            vec![0; LIMIT + 1],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn merge_reaches_the_service_and_reads_no_body() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(
        200,
        json!({ "ok": true, "merged": true }),
    ));
    let mut body = vec![b'a'; (1 << 20) + 1];
    body[0] = b'{';
    let response = app
        .oneshot(request(
            Method::POST,
            "/api/workspaces/w%201/merge",
            &[],
            body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        text(response).await,
        json!({ "ok": true, "merged": true }).to_string()
    );
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Merge {
            workspace_id: "w 1".to_owned()
        }
    );
}

// ---------------------------------------------------------------------------------------------
// Restore and history
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn restore_reaches_the_service_with_its_workspace_and_priority() {
    let (app, writes) = app();
    app.oneshot(post(
        "/api/sessions/s1/restore",
        &[("x-relay-client", "mcp")],
        r#"{"workspaceId":" w1 "}"#,
    ))
    .await
    .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Restore {
            session_id: "s1".to_owned(),
            workspace_id: Some("w1".to_owned()),
            priority: Priority::Background,
        }
    );
}

#[tokio::test]
async fn restore_accepts_an_empty_body() {
    let (app, writes) = app();
    app.oneshot(post("/api/sessions/s1/restore", &[], ""))
        .await
        .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Restore {
            session_id: "s1".to_owned(),
            workspace_id: None,
            priority: Priority::Interactive,
        }
    );
}

#[tokio::test]
async fn restore_refuses_a_bad_body() {
    let uri = "/api/sessions/s1/restore";
    assert_post_refused(
        uri,
        "{oops",
        StatusCode::BAD_REQUEST,
        "request body must be valid JSON",
    )
    .await;
    assert_post_refused(
        uri,
        "[1]",
        StatusCode::BAD_REQUEST,
        "request body must be a JSON object",
    )
    .await;
    assert_post_refused(
        uri,
        r#"{"workspaceId":4}"#,
        StatusCode::BAD_REQUEST,
        "workspaceId: must be a string",
    )
    .await;
}

#[tokio::test]
async fn history_reaches_the_service() {
    let (app, writes) = app();
    app.oneshot(post(
        "/api/sessions/s%201/history",
        &[],
        r#"{"workspaceId":"w1","previousSessionId":"s0","extra":true}"#,
    ))
    .await
    .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::JoinHistory {
            session_id: "s 1".to_owned(),
            workspace_id: "w1".to_owned(),
            previous_session_id: "s0".to_owned(),
        }
    );
}

#[tokio::test]
async fn history_refuses_missing_or_non_string_fields() {
    let uri = "/api/sessions/s1/history";
    for body in [
        "{}",
        r#"{"workspaceId":"w1"}"#,
        r#"{"previousSessionId":"s0"}"#,
        r#"{"workspaceId":"w1","previousSessionId":3}"#,
        r#"{"workspaceId":null,"previousSessionId":"s0"}"#,
    ] {
        assert_post_refused(
            uri,
            body,
            StatusCode::BAD_REQUEST,
            "workspaceId and previousSessionId are required",
        )
        .await;
    }
    assert_post_refused(
        uri,
        "nope",
        StatusCode::BAD_REQUEST,
        "request body must be valid JSON",
    )
    .await;
    assert_post_refused(
        uri,
        "\"text\"",
        StatusCode::BAD_REQUEST,
        "request body must be a JSON object",
    )
    .await;
    assert_post_refused(
        uri,
        "",
        StatusCode::BAD_REQUEST,
        "request body must be valid JSON",
    )
    .await;
}

// ---------------------------------------------------------------------------------------------
// Split
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn split_defaults_to_thinking_without_tools() {
    let (app, writes) = app();
    app.oneshot(post("/api/sessions/s1/split", &[], "{}"))
        .await
        .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Split {
            session_id: "s1".to_owned(),
            request: SplitRequest {
                destination: SplitDestination::Chat,
                workspace_id: None,
                prompt: None,
                include_thinking: true,
                include_tools: false,
                through_rowid: None,
                only_rowid: None,
            },
            priority: Priority::Interactive,
        }
    );
}

#[tokio::test]
async fn split_passes_every_field() {
    let (app, writes) = app();
    let body = json!({
        "prompt": "  keep going ",
        "workspaceId": "w1",
        "includeThinking": false,
        "includeTools": true,
        "throughRowid": 42,
        "destination": "chat",
    });
    app.oneshot(post(
        "/api/sessions/s%201/split",
        &[("x-relay-client", "mcp")],
        &body.to_string(),
    ))
    .await
    .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Split {
            session_id: "s 1".to_owned(),
            request: SplitRequest {
                destination: SplitDestination::Chat,
                workspace_id: Some("w1".to_owned()),
                prompt: Some("keep going".to_owned()),
                include_thinking: false,
                include_tools: true,
                through_rowid: Some(42),
                only_rowid: None,
            },
            priority: Priority::Background,
        }
    );
}

#[tokio::test]
async fn split_passes_only_rowid_and_treats_null_as_absent() {
    let (app, writes) = app();
    app.oneshot(post(
        "/api/sessions/s1/split",
        &[],
        r#"{"onlyRowid":9007199254740991,"throughRowid":null,"destination":null}"#,
    ))
    .await
    .unwrap();
    let [Call::Split { request, .. }] = only_call(&writes) else {
        panic!("not a split");
    };
    assert_eq!(request.only_rowid, Some(9_007_199_254_740_991));
    assert_eq!(request.through_rowid, None);
}

#[tokio::test]
async fn split_refuses_an_unknown_destination() {
    for body in [
        r#"{"destination":"tab"}"#,
        r#"{"destination":""}"#,
        r#"{"destination":5}"#,
        r#"{"destination":["chat"]}"#,
    ] {
        assert_post_refused(
            "/api/sessions/s1/split",
            body,
            StatusCode::BAD_REQUEST,
            "destination must be chat or workspace",
        )
        .await;
    }
}

#[tokio::test]
async fn split_passes_a_workspace_destination() {
    let (app, writes) = app();
    app.oneshot(post(
        "/api/sessions/s1/split",
        &[],
        r#"{"destination":"workspace","workspaceId":"w1"}"#,
    ))
    .await
    .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Split {
            session_id: "s1".to_owned(),
            request: SplitRequest {
                destination: SplitDestination::Workspace,
                workspace_id: Some("w1".to_owned()),
                prompt: None,
                include_thinking: true,
                include_tools: false,
                through_rowid: None,
                only_rowid: None,
            },
            priority: Priority::Interactive,
        }
    );
}

#[tokio::test]
async fn split_refuses_row_ids_that_are_not_positive_integers() {
    for (field, value) in [
        ("throughRowid", "0"),
        ("throughRowid", "-3"),
        ("throughRowid", "1.5"),
        ("throughRowid", "\"7\""),
        ("throughRowid", "true"),
        ("throughRowid", "9007199254740992"),
        ("onlyRowid", "0"),
        ("onlyRowid", "-1"),
        ("onlyRowid", "2.25"),
        ("onlyRowid", "\"7\""),
        ("onlyRowid", "[1]"),
    ] {
        assert_post_refused(
            "/api/sessions/s1/split",
            &format!(r#"{{"{field}":{value}}}"#),
            StatusCode::BAD_REQUEST,
            &format!("{field} must be a positive integer"),
        )
        .await;
    }
}

#[tokio::test]
async fn split_refuses_both_row_ids() {
    assert_post_refused(
        "/api/sessions/s1/split",
        r#"{"throughRowid":3,"onlyRowid":2}"#,
        StatusCode::BAD_REQUEST,
        "throughRowid and onlyRowid cannot be combined",
    )
    .await;
}

#[tokio::test]
async fn split_refuses_a_bad_body() {
    let uri = "/api/sessions/s1/split";
    assert_post_refused(
        uri,
        "{",
        StatusCode::BAD_REQUEST,
        "request body must be valid JSON",
    )
    .await;
    assert_post_refused(
        uri,
        "null",
        StatusCode::BAD_REQUEST,
        "request body must be a JSON object",
    )
    .await;
    assert_post_refused(
        uri,
        r#"{"prompt":1}"#,
        StatusCode::BAD_REQUEST,
        "prompt: must be a string",
    )
    .await;
}

// ---------------------------------------------------------------------------------------------
// Create a workspace
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_defaults_to_sending_immediately() {
    let (app, writes) = app();
    app.oneshot(post("/api/workspaces", &[], "{}"))
        .await
        .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Create {
            request: CreateRequest {
                repo: None,
                prompt: None,
                send_immediately: true,
                attachment_ids: Vec::new(),
                agent: None,
            },
            priority: Priority::Interactive,
        }
    );
}

#[tokio::test]
async fn create_passes_its_fields_and_the_agent() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(
        200,
        json!({ "ok": true, "workspaceId": "w9" }),
    ));
    let body = json!({
        "repo": " /work/repo ",
        "prompt": "  build it ",
        "sendImmediately": false,
        "attachmentIds": ["a1b2c3", "d4e5f6"],
        "auto": false,
        "model": "Some Model",
        "effort": "high",
        "plan": true,
        "fast": false,
        "send": true,
        "unknown": {},
    });
    let response = app
        .oneshot(post(
            "/api/workspaces",
            &[("x-relay-client", "mcp")],
            &body.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        text(response).await,
        json!({ "ok": true, "workspaceId": "w9" }).to_string()
    );
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Create {
            request: CreateRequest {
                repo: Some("/work/repo".to_owned()),
                prompt: Some("build it".to_owned()),
                send_immediately: false,
                attachment_ids: vec!["a1b2c3".to_owned(), "d4e5f6".to_owned()],
                agent: Some(AgentPatch {
                    model: Some("Some Model".to_owned()),
                    effort: Some(Effort::High),
                    plan: Some(true),
                    fast: Some(false),
                }),
            },
            priority: Priority::Background,
        }
    );
}

#[tokio::test]
async fn create_passes_the_top_level_agent_fields() {
    let (app, writes) = app();
    let body = r#"{"model":" Opus 5.5 ","effort":"max","plan":false,"fast":true,"send":false}"#;
    app.oneshot(post("/api/workspaces", &[], body))
        .await
        .unwrap();
    let [Call::Create { request, .. }] = only_call(&writes) else {
        panic!("expected one create");
    };
    assert_eq!(
        request.agent,
        Some(AgentPatch {
            model: Some("Opus 5.5".to_owned()),
            effort: Some(Effort::Max),
            plan: Some(false),
            fast: Some(true),
        })
    );
}

#[tokio::test]
async fn create_without_agent_fields_has_no_agent() {
    let (app, writes) = app();
    app.oneshot(post(
        "/api/workspaces",
        &[],
        r#"{"prompt":"x","model":"","plan":null,"send":true}"#,
    ))
    .await
    .unwrap();
    let [Call::Create { request, .. }] = only_call(&writes) else {
        panic!("expected one create");
    };
    assert_eq!(request.agent, None);
}

#[tokio::test]
async fn create_refuses_a_bad_agent_field() {
    assert_post_refused(
        "/api/workspaces",
        r#"{"effort":5}"#,
        StatusCode::BAD_REQUEST,
        "effort: must be one of none, low, medium, high, xhigh, max, ultracode",
    )
    .await;
}

#[tokio::test]
async fn create_refuses_auto() {
    assert_post_refused(
        "/api/workspaces",
        r#"{"prompt":"x","auto":true}"#,
        StatusCode::SERVICE_UNAVAILABLE,
        "Auto is unavailable.",
    )
    .await;
}

#[tokio::test]
async fn create_refuses_bad_attachment_ids() {
    for value in ["\"a1b2c3\"", "5", "{}", "[1]", "[\"a\",null]", "[[\"a\"]]"] {
        assert_post_refused(
            "/api/workspaces",
            &format!(r#"{{"attachmentIds":{value}}}"#),
            StatusCode::BAD_REQUEST,
            "attachmentIds: must be an array of strings",
        )
        .await;
    }
}

#[tokio::test]
async fn create_refuses_a_bad_body() {
    assert_post_refused(
        "/api/workspaces",
        "{x",
        StatusCode::BAD_REQUEST,
        "request body must be valid JSON",
    )
    .await;
    assert_post_refused(
        "/api/workspaces",
        "[]",
        StatusCode::BAD_REQUEST,
        "request body must be a JSON object",
    )
    .await;
    assert_post_refused(
        "/api/workspaces",
        r#"{"sendImmediately":"yes"}"#,
        StatusCode::BAD_REQUEST,
        "sendImmediately: must be a boolean",
    )
    .await;
    assert_post_refused(
        "/api/workspaces",
        r#"{"repo":3}"#,
        StatusCode::BAD_REQUEST,
        "repo: must be a string",
    )
    .await;
}

// ---------------------------------------------------------------------------------------------
// JSON body size
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn json_routes_refuse_a_body_over_one_mebibyte() {
    for uri in [
        "/api/sessions/s1/restore",
        "/api/sessions/s1/history",
        "/api/sessions/s1/split",
        "/api/workspaces",
    ] {
        let (app, writes) = app();
        let mut body = vec![b' '; (1 << 20) + 1];
        body[0] = b'{';
        assert_refused(
            app,
            &writes,
            request(Method::POST, uri, &[], body),
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body is too large",
        )
        .await;
    }
}

#[tokio::test]
async fn the_prefs_patch_refuses_a_body_over_one_mebibyte() {
    let (app, prefs) = prefs_app();
    let mut body = vec![b' '; (1 << 20) + 1];
    body[0] = b'{';
    let response = app
        .oneshot(request(Method::PATCH, "/api/prefs", &[], body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        text(response).await,
        error_text("request body is too large")
    );
    assert!(prefs.patches().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Preferences
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn getting_the_prefs_wraps_the_document() {
    let (app, _) = prefs_app();
    let response = app
        .oneshot(request(Method::GET, "/api/prefs", &[], Vec::new()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(
        text(response).await,
        json!({ "prefs": { "readMarks": {}, "drafts": {} } }).to_string()
    );
}

#[tokio::test]
async fn patching_the_prefs_returns_the_merged_document() {
    let (app, prefs) = prefs_app();
    let response = app
        .oneshot(request(
            Method::PATCH,
            "/api/prefs",
            &[],
            br#"{"readMarks":{"s1":4}}"#.to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        text(response).await,
        json!({ "prefs": { "readMarks": { "s1": 4 }, "drafts": {} } }).to_string()
    );
    assert_eq!(prefs.patches(), [json!({ "readMarks": { "s1": 4 } })]);
}

#[tokio::test]
async fn an_empty_prefs_patch_counts_as_an_empty_object() {
    let (app, prefs) = prefs_app();
    prefs.patching(Err("nothing to sync".to_owned()));
    let response = app
        .oneshot(request(Method::PATCH, "/api/prefs", &[], Vec::new()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(text(response).await, error_text("nothing to sync"));
    assert_eq!(prefs.patches(), [json!({})]);
}

#[tokio::test]
async fn a_prefs_patch_that_is_not_json_is_refused_without_the_service() {
    let (app, prefs) = prefs_app();
    let response = app
        .oneshot(request(Method::PATCH, "/api/prefs", &[], b"{nope".to_vec()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        text(response).await,
        error_text("request body must be valid JSON")
    );
    assert!(prefs.patches().is_empty());
}

#[tokio::test]
async fn a_non_object_prefs_patch_is_the_services_to_judge() {
    let (app, prefs) = prefs_app();
    prefs.patching(Err("preferences must be an object".to_owned()));
    let response = app
        .oneshot(request(Method::PATCH, "/api/prefs", &[], b"[1,2]".to_vec()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        text(response).await,
        error_text("preferences must be an object")
    );
    assert_eq!(prefs.patches(), [json!([1, 2])]);
}

#[tokio::test]
async fn other_methods_on_the_prefs_path_are_not_found() {
    let (app, prefs) = prefs_app();
    let response = app
        .oneshot(request(Method::POST, "/api/prefs", &[], b"{}".to_vec()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(prefs.patches().is_empty());
}
