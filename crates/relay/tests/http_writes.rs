//! The write routes over a fake `WriteService`: the token gate, the parsed fields each service
//! method receives, every request the routes refuse themselves, and the service's answer passing
//! through.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::body::Bytes;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::agent::{AgentPatch, Effort};
use conductor_remote::contract::{AppState, ConductorStatus, Priority, Token};
use conductor_remote::delivery::{
    BoxFuture, CreateRequest, SendRequest, SplitRequest, WriteAnswer, WriteService, STRATEGY,
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
    Questions(
        String,
        conductor_remote::delivery::questions::AnswerQuestionsRequest,
        Priority,
    ),
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
    SetAgent {
        session_id: String,
        workspace_id: Option<String>,
        patch: AgentPatch,
        priority: Priority,
    },
    ListModels {
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    },
    CloseChat {
        session_id: String,
        workspace_id: Option<String>,
        close_running: bool,
        priority: Priority,
    },
    SetStatus {
        workspace_id: String,
        status: String,
        priority: Priority,
    },
    Archive {
        workspace_id: String,
        stop_agents: bool,
        priority: Priority,
    },
    Continue {
        workspace_id: String,
        session_id: Option<String>,
        priority: Priority,
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
    fn answer_questions(
        &self,
        id: String,
        request: conductor_remote::delivery::questions::AnswerQuestionsRequest,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::Questions(id, request, priority))
    }

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

    fn set_agent(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        patch: AgentPatch,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::SetAgent {
            session_id,
            workspace_id,
            patch,
            priority,
        })
    }

    fn list_models(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::ListModels {
            session_id,
            workspace_id,
            priority,
        })
    }

    fn close_chat(
        &self,
        session_id: String,
        workspace_id: Option<String>,
        close_running: bool,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::CloseChat {
            session_id,
            workspace_id,
            close_running,
            priority,
        })
    }

    fn set_workspace_status(
        &self,
        workspace_id: String,
        status: String,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::SetStatus {
            workspace_id,
            status,
            priority,
        })
    }

    fn archive_workspace(
        &self,
        workspace_id: String,
        stop_agents: bool,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::Archive {
            workspace_id,
            stop_agents,
            priority,
        })
    }

    fn continue_workspace(
        &self,
        workspace_id: String,
        session_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        self.record(Call::Continue {
            workspace_id,
            session_id,
            priority,
        })
    }
}

/// The one call the service received: a one-element array to destructure.
fn only_call(writes: &FakeWrites) -> [Call; 1] {
    let calls = writes.calls();
    assert_eq!(calls.len(), 1, "expected exactly one service call");
    calls.try_into().unwrap()
}

fn app_with(writes: Option<Arc<FakeWrites>>, status: ConductorStatus) -> Router {
    router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor: Arc::new(FakeConductor::new(status)),
        assets: Arc::new(MemoryAssets::default()),
        reads: None,
        writes: writes.map(|writes| writes as Arc<dyn WriteService>),
        notify: None,
        services: Default::default(),
    })
}

fn app() -> (Router, Arc<FakeWrites>) {
    let writes = FakeWrites::new();
    (
        app_with(Some(writes.clone()), ConductorStatus::Running),
        writes,
    )
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

/// Sends `body` to the prompt route and returns the status and exact body text, asserting the
/// service was not called.
async fn refused_prompt(body: &str) -> (StatusCode, String) {
    let (app, writes) = app();
    let response = app
        .oneshot(post("/api/sessions/s1/prompt", &[], body))
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(header_of(&response, "cache-control"), "no-store");
    let body = text(response).await;
    assert!(writes.calls().is_empty(), "the service was called");
    (status, body)
}

async fn assert_prompt_refused(body: &str, message: &str) {
    let (status, text) = refused_prompt(body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(text, format!(r#"{{"error":"{message}"}}"#), "{body}");
}

// ---------------------------------------------------------------------------------------------
// The token gate
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_write_route_needs_the_token() {
    for uri in [
        "/api/sessions/s1/prompt",
        "/api/sessions/s1/questions/answer",
        "/api/sessions/s1/stop",
        "/api/workspaces/w1/sessions",
    ] {
        let (app, writes) = app();
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(Body::from(r#"{"text":"hi"}"#))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(text(response).await, r#"{"error":"unauthorized"}"#);
        assert!(writes.calls().is_empty());
    }
}

// ---------------------------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn prompt_reaches_the_service_with_the_parsed_fields() {
    let (app, writes) = app();
    let body = json!({
        "text": "  hello there \n",
        "workspaceId": " w1 ",
        "clientId": "bubble-7",
        "queue": true,
        "unknown": [1, 2, 3],
    });
    let response = app
        .oneshot(post(
            "/api/sessions/s1/prompt",
            &[("x-client-timeout-ms", "2500")],
            &body.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(header_of(&response, "cache-control"), "no-store");
    assert_eq!(
        text(response).await,
        json!({ "ok": true, "strategy": STRATEGY }).to_string()
    );
    assert_eq!(
        writes.calls(),
        [Call::Send(SendRequest {
            session_id: "s1".to_owned(),
            text: "hello there".to_owned(),
            workspace_id: Some("w1".to_owned()),
            client_id: Some("bubble-7".to_owned()),
            queue: true,
            client_timeout_ms: Some(2500),
            priority: Priority::Interactive,
            agent: None,
        })]
    );
}

#[tokio::test]
async fn prompt_defaults_and_empty_ids() {
    let (app, writes) = app();
    let body = r#"{"text":"hi","workspaceId":"   ","clientId":null,"queue":null,"auto":null,"agent":null}"#;
    let response = app
        .oneshot(post("/api/sessions/a%20b/prompt", &[], body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        writes.calls(),
        [Call::Send(SendRequest {
            session_id: "a b".to_owned(),
            text: "hi".to_owned(),
            workspace_id: None,
            client_id: None,
            queue: false,
            client_timeout_ms: None,
            priority: Priority::Interactive,
            agent: None,
        })]
    );
}

#[tokio::test]
async fn prompt_priority_follows_x_relay_client() {
    for (value, expected) in [
        (Some("mcp"), Priority::Background),
        (Some("web"), Priority::Interactive),
        (None, Priority::Interactive),
    ] {
        let (app, writes) = app();
        let headers: Vec<(&str, &str)> = value.map(|v| ("x-relay-client", v)).into_iter().collect();
        app.oneshot(post(
            "/api/sessions/s1/prompt",
            &headers,
            r#"{"text":"hi"}"#,
        ))
        .await
        .unwrap();
        let [Call::Send(sent)] = only_call(&writes) else {
            panic!("expected one send");
        };
        assert_eq!(sent.priority, expected, "{value:?}");
    }
}

#[tokio::test]
async fn an_unparsable_client_timeout_is_none() {
    for value in ["soon", "-5", "1.5", ""] {
        let (app, writes) = app();
        app.oneshot(post(
            "/api/sessions/s1/prompt",
            &[("x-client-timeout-ms", value)],
            r#"{"text":"hi"}"#,
        ))
        .await
        .unwrap();
        let [Call::Send(sent)] = only_call(&writes) else {
            panic!("expected one send");
        };
        assert_eq!(sent.client_timeout_ms, None, "{value:?}");
    }
}

#[tokio::test]
async fn prompt_body_must_be_json() {
    for body in ["", "not json", r#"{"text":"#] {
        assert_prompt_refused(body, "request body must be valid JSON").await;
    }
}

#[tokio::test]
async fn prompt_body_must_be_an_object() {
    for body in ["[]", "\"hi\"", "7", "null", "true"] {
        assert_prompt_refused(body, "request body must be a JSON object").await;
    }
}

#[tokio::test]
async fn prompt_text_must_be_a_string() {
    for body in [
        "{}",
        r#"{"text":null}"#,
        r#"{"text":5}"#,
        r#"{"text":["hi"]}"#,
    ] {
        assert_prompt_refused(body, "text: prompt must be a string").await;
    }
}

#[tokio::test]
async fn prompt_text_must_not_be_empty() {
    for body in [r#"{"text":""}"#, r#"{"text":" \n\t "}"#] {
        assert_prompt_refused(body, "text: empty prompt").await;
    }
}

#[tokio::test]
async fn prompt_ids_must_be_strings() {
    for field in ["workspaceId", "clientId"] {
        for value in [json!(5), json!(true), json!({}), json!([])] {
            let body = json!({ "text": "hi", field: value }).to_string();
            assert_prompt_refused(&body, &format!("{field}: must be a string")).await;
        }
    }
}

#[tokio::test]
async fn prompt_flags_must_be_booleans() {
    for field in ["queue", "auto"] {
        for value in [json!("yes"), json!(1), json!({}), json!([])] {
            let body = json!({ "text": "hi", field: value }).to_string();
            assert_prompt_refused(&body, &format!("{field}: must be a boolean")).await;
        }
    }
}

#[tokio::test]
async fn prompt_agent_must_be_an_object() {
    for value in [json!("fast"), json!(1), json!(true), json!([])] {
        let body = json!({ "text": "hi", "agent": value }).to_string();
        assert_prompt_refused(&body, "agent: must be an object").await;
    }
}

#[tokio::test]
async fn prompt_fields_are_checked_in_order() {
    let all_wrong = json!({
        "text": 1, "workspaceId": 1, "clientId": 1, "queue": 1, "auto": 1, "agent": 1,
    });
    let mut body = all_wrong.as_object().unwrap().clone();
    for (field, message) in [
        ("text", "text: prompt must be a string"),
        ("workspaceId", "workspaceId: must be a string"),
        ("clientId", "clientId: must be a string"),
        ("queue", "queue: must be a boolean"),
        ("auto", "auto: must be a boolean"),
        ("agent", "agent: must be an object"),
    ] {
        assert_prompt_refused(&Value::Object(body.clone()).to_string(), message).await;
        body.remove(field);
        if field == "text" {
            body.insert("text".to_owned(), json!("hi"));
        }
    }
}

#[tokio::test]
async fn auto_is_unavailable_and_the_service_is_not_called() {
    let (status, body) = refused_prompt(r#"{"text":"hi","auto":true}"#).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, r#"{"error":"Auto is unavailable."}"#);
}

#[tokio::test]
async fn auto_false_is_sent() {
    let (app, writes) = app();
    let response = app
        .oneshot(post(
            "/api/sessions/s1/prompt",
            &[],
            r#"{"text":"hi","auto":false}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(writes.calls().len(), 1);
}

#[tokio::test]
async fn an_agent_object_is_parsed() {
    let (app, writes) = app();
    let body = r#"{"text":"hi","agent":{"model":"opus","thinking":"high"}}"#;
    let response = app
        .oneshot(post("/api/sessions/s1/prompt", &[], body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let [Call::Send(sent)] = only_call(&writes) else {
        panic!("expected one send");
    };
    assert_eq!(sent.text, "hi");
    assert_eq!(
        sent.agent,
        Some(AgentPatch {
            model: Some("opus".into()),
            ..Default::default()
        })
    );
}

#[tokio::test]
async fn a_full_agent_object_reaches_the_service_as_a_patch() {
    let (app, writes) = app();
    let body = json!({
        "text": "hi",
        "agent": { "model": " Opus 5.5 ", "effort": "xhigh", "plan": true, "fast": false },
    });
    let response = app
        .oneshot(post("/api/sessions/s1/prompt", &[], &body.to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let [Call::Send(sent)] = only_call(&writes) else {
        panic!("expected one send");
    };
    assert_eq!(
        sent.agent,
        Some(AgentPatch {
            model: Some("Opus 5.5".into()),
            effort: Some(Effort::Xhigh),
            plan: Some(true),
            fast: Some(false),
        })
    );
}

#[tokio::test]
async fn an_empty_or_null_agent_reaches_the_service_as_none() {
    for agent in [json!({}), json!(null)] {
        let (app, writes) = app();
        let body = json!({ "text": "hi", "agent": agent });
        app.oneshot(post("/api/sessions/s1/prompt", &[], &body.to_string()))
            .await
            .unwrap();
        let [Call::Send(sent)] = only_call(&writes) else {
            panic!("expected one send");
        };
        assert_eq!(sent.agent, None, "{body}");
    }
}

#[tokio::test]
async fn a_bad_agent_field_is_refused_with_the_agent_prefix() {
    assert_prompt_refused(
        r#"{"text":"hi","agent":{"effort":"huge"}}"#,
        "agent.effort: must be one of none, low, medium, high, xhigh, max, ultracode",
    )
    .await;
}

// ---------------------------------------------------------------------------------------------
// Stop
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stop_with_an_empty_body_has_no_workspace() {
    let (app, writes) = app();
    let response = app
        .oneshot(post("/api/sessions/s1/stop", &[], ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        writes.calls(),
        [Call::Stop {
            session_id: "s1".to_owned(),
            workspace_id: None,
            priority: Priority::Interactive,
        }]
    );
}

#[tokio::test]
async fn stop_passes_the_trimmed_workspace_and_the_priority() {
    let (app, writes) = app();
    app.oneshot(post(
        "/api/sessions/s1/stop",
        &[("x-relay-client", "mcp")],
        r#"{"workspaceId":" w9 "}"#,
    ))
    .await
    .unwrap();
    assert_eq!(
        writes.calls(),
        [Call::Stop {
            session_id: "s1".to_owned(),
            workspace_id: Some("w9".to_owned()),
            priority: Priority::Background,
        }]
    );
}

#[tokio::test]
async fn stop_empty_or_null_workspace_is_none() {
    for body in [r#"{"workspaceId":""}"#, r#"{"workspaceId":null}"#, "{}"] {
        let (app, writes) = app();
        app.oneshot(post("/api/sessions/s1/stop", &[], body))
            .await
            .unwrap();
        let [Call::Stop { workspace_id, .. }] = only_call(&writes) else {
            panic!("expected one stop");
        };
        assert_eq!(workspace_id, None, "{body}");
    }
}

#[tokio::test]
async fn stop_refuses_a_bad_body() {
    for (body, message) in [
        ("nope", "request body must be valid JSON"),
        ("[1]", "request body must be a JSON object"),
        (r#"{"workspaceId":4}"#, "workspaceId: must be a string"),
    ] {
        let (app, writes) = app();
        let response = app
            .oneshot(post("/api/sessions/s1/stop", &[], body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            text(response).await,
            format!(r#"{{"error":"{message}"}}"#),
            "{body}"
        );
        assert!(writes.calls().is_empty());
    }
}

// ---------------------------------------------------------------------------------------------
// New chat
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn new_chat_reaches_the_service_and_ignores_the_body() {
    let (app, writes) = app();
    let response = app
        .oneshot(post(
            "/api/workspaces/w%201/sessions",
            &[("x-relay-client", "mcp")],
            "this is not json",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        writes.calls(),
        [Call::NewChat {
            workspace_id: "w 1".to_owned(),
            priority: Priority::Background,
        }]
    );
}

// ---------------------------------------------------------------------------------------------
// The answer, the missing service, the body limit and the other routes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_service_status_body_and_retry_after_pass_through() {
    let (app, writes) = app();
    writes.answering(WriteAnswer {
        status: 429,
        body: json!({ "error": "busy", "strategy": STRATEGY }),
        retry_after_secs: Some(7),
    });
    for (uri, body) in [
        ("/api/sessions/s1/prompt", r#"{"text":"hi"}"#),
        (
            "/api/sessions/s1/questions/answer",
            r#"{"workspaceId":"sample-ws","requestId":"sample-call","answers":[{"selected":[0]}]}"#,
        ),
        ("/api/sessions/s1/stop", ""),
        ("/api/workspaces/w1/sessions", ""),
    ] {
        let response = app.clone().oneshot(post(uri, &[], body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{uri}");
        assert_eq!(header_of(&response, "retry-after"), "7");
        assert_eq!(header_of(&response, "content-type"), "application/json");
        assert_eq!(header_of(&response, "cache-control"), "no-store");
        assert_eq!(
            text(response).await,
            json!({ "error": "busy", "strategy": STRATEGY }).to_string()
        );
    }
}

#[tokio::test]
async fn no_retry_after_header_without_a_delay() {
    let (app, _) = app();
    let response = app
        .oneshot(post("/api/sessions/s1/stop", &[], ""))
        .await
        .unwrap();
    assert!(response.headers().get("retry-after").is_none());
}

#[tokio::test]
async fn without_a_service_every_write_route_is_unavailable() {
    let app = app_with(None, ConductorStatus::Running);
    for (uri, body) in [
        ("/api/sessions/s1/prompt", r#"{"text":"hi"}"#),
        (
            "/api/sessions/s1/questions/answer",
            r#"{"workspaceId":"sample-ws","requestId":"sample-call","answers":[{"selected":[0]}]}"#,
        ),
        ("/api/sessions/s1/stop", ""),
        ("/api/workspaces/w1/sessions", ""),
    ] {
        let response = app.clone().oneshot(post(uri, &[], body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(header_of(&response, "content-type"), "application/json");
        assert_eq!(header_of(&response, "cache-control"), "no-store");
        assert_eq!(
            text(response).await,
            r#"{"error":"writes are unavailable"}"#
        );
    }
}

#[tokio::test]
async fn a_body_over_one_mebibyte_is_too_large() {
    let (app, writes) = app();
    let mut body = br#"{"text":""#.to_vec();
    body.resize((1 << 20) + 1, b'a');
    let response = app
        .oneshot(request(Method::POST, "/api/sessions/s1/prompt", &[], body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(
        text(response).await,
        r#"{"error":"request body is too large"}"#
    );
    assert!(writes.calls().is_empty());
}

#[tokio::test]
async fn a_body_of_exactly_one_mebibyte_is_read() {
    let (app, writes) = app();
    let prefix = br#"{"text":"hi","pad":""#;
    let suffix = br#""}"#;
    let mut body = prefix.to_vec();
    body.resize((1 << 20) - suffix.len(), b'a');
    body.extend_from_slice(suffix);
    assert_eq!(body.len(), 1 << 20);
    let response = app
        .oneshot(request(Method::POST, "/api/sessions/s1/prompt", &[], body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(writes.calls().len(), 1);
}

#[tokio::test]
async fn listing_sessions_still_reaches_the_reads_route() {
    let writes = FakeWrites::new();
    let app = app_with(Some(writes.clone()), ConductorStatus::NotRunning);
    let response = app
        .oneshot(request(
            Method::GET,
            "/api/workspaces/w1/sessions",
            &[],
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        text(response).await,
        r#"{"error":"Conductor is not running"}"#
    );
    assert!(writes.calls().is_empty());
}

#[tokio::test]
async fn other_methods_on_the_write_paths_are_not_found() {
    for method in [Method::GET, Method::PUT, Method::DELETE, Method::PATCH] {
        for uri in [
            "/api/sessions/s1/prompt",
            "/api/sessions/s1/stop",
            "/api/sessions/s1/questions/answer",
        ] {
            if method == Method::DELETE && uri == "/api/sessions/s1/prompt" {
                // Reaches `dismiss_parked` now; the dismiss tests below cover it.
                continue;
            }
            let (app, writes) = app();
            let response = app
                .oneshot(request(method.clone(), uri, &[], b"{}".to_vec()))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {uri}");
            assert_eq!(text(response).await, r#"{"error":"not found"}"#);
            assert!(writes.calls().is_empty());
        }
    }
    for method in [Method::PUT, Method::DELETE, Method::PATCH] {
        let (app, _) = app();
        let response = app
            .oneshot(request(
                method.clone(),
                "/api/workspaces/w1/sessions",
                &[],
                Vec::new(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
        assert_eq!(text(response).await, r#"{"error":"not found"}"#);
    }
}

// ---------------------------------------------------------------------------------------------
// Dismiss
// ---------------------------------------------------------------------------------------------

fn delete(uri: &str, body: &[u8]) -> Request<Body> {
    request(Method::DELETE, uri, &[], body.to_vec())
}

#[tokio::test]
async fn deleting_a_session_prompt_reaches_dismiss_parked() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(200, json!({ "ok": true })));
    let response = app
        .oneshot(delete("/api/sessions/s%201/prompt", b""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(text(response).await, r#"{"ok":true}"#);
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Dismiss {
            session_id: "s 1".to_owned()
        }
    );
}

#[tokio::test]
async fn the_dismiss_answer_passes_through() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::error(404, "no parked prompt"));
    let response = app
        .oneshot(delete("/api/sessions/s1/prompt", b""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(response).await, r#"{"error":"no parked prompt"}"#);
    assert_eq!(writes.calls().len(), 1);
}

#[tokio::test]
async fn a_delete_body_is_not_read() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(200, json!({ "ok": true })));
    let mut body = vec![b'a'; (1 << 20) + 1];
    body[0] = b'{';
    let response = app
        .oneshot(delete("/api/sessions/s1/prompt", &body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(writes.calls().len(), 1);
}

#[tokio::test]
async fn deleting_a_workspace_prompt_passes_through_to_the_service() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::error(404, "no pending prompt"));
    let response = app
        .oneshot(delete("/api/workspaces/w%201/prompt", b""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(text(response).await, r#"{"error":"no pending prompt"}"#);
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::DismissFirstPrompt {
            workspace_id: "w 1".to_owned()
        }
    );
}

#[tokio::test]
async fn the_dismiss_routes_need_the_token() {
    for uri in ["/api/sessions/s1/prompt", "/api/workspaces/w1/prompt"] {
        let (app, writes) = app();
        let request = Request::builder()
            .method(Method::DELETE)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(text(response).await, r#"{"error":"unauthorized"}"#);
        assert!(writes.calls().is_empty());
    }
}

#[tokio::test]
async fn without_a_service_dismissing_a_session_prompt_is_unavailable() {
    let app = app_with(None, ConductorStatus::Running);
    let response = app
        .oneshot(delete("/api/sessions/s1/prompt", b""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        text(response).await,
        r#"{"error":"writes are unavailable"}"#
    );
}

// ---------------------------------------------------------------------------------------------
// Agent options
// ---------------------------------------------------------------------------------------------

fn get(uri: &str) -> Request<Body> {
    request(Method::GET, uri, &[], Vec::new())
}

#[tokio::test]
async fn the_agent_route_passes_session_workspace_patch_and_priority() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(200, json!({ "ok": true })));
    let body = json!({
        "workspaceId": " w1 ",
        "model": " Opus 5.5 ",
        "effort": "high",
        "plan": true,
        "fast": false,
        "unknown": 1,
    });
    let response = app
        .oneshot(post(
            "/api/sessions/a%20b/agent",
            &[("x-relay-client", "mcp")],
            &body.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text(response).await, json!({ "ok": true }).to_string());
    assert_eq!(
        writes.calls(),
        [Call::SetAgent {
            session_id: "a b".to_owned(),
            workspace_id: Some("w1".to_owned()),
            patch: AgentPatch {
                model: Some("Opus 5.5".into()),
                effort: Some(Effort::High),
                plan: Some(true),
                fast: Some(false),
            },
            priority: Priority::Background,
        }]
    );
}

#[tokio::test]
async fn the_agent_route_without_a_workspace_passes_none() {
    let (app, writes) = app();
    app.oneshot(post("/api/sessions/s1/agent", &[], r#"{"plan":false}"#))
        .await
        .unwrap();
    assert_eq!(
        writes.calls(),
        [Call::SetAgent {
            session_id: "s1".to_owned(),
            workspace_id: None,
            patch: AgentPatch {
                plan: Some(false),
                ..Default::default()
            },
            priority: Priority::Interactive,
        }]
    );
}

#[tokio::test]
async fn the_agent_route_refuses_what_it_cannot_apply() {
    for (body, message) in [
        ("not JSON", "request body must be valid JSON"),
        ("[]", "request body must be a JSON object"),
        (
            r#"{"workspaceId":1,"plan":true}"#,
            "workspaceId: must be a string",
        ),
        (r#"{"model":1}"#, "model: must be a string"),
        (
            r#"{"effort":"huge"}"#,
            "effort: must be one of none, low, medium, high, xhigh, max, ultracode",
        ),
        (
            r#"{"effort":5}"#,
            "effort: must be one of none, low, medium, high, xhigh, max, ultracode",
        ),
        (r#"{"plan":"yes"}"#, "plan: must be a boolean"),
        (r#"{"fast":1}"#, "fast: must be a boolean"),
        ("{}", "nothing to change"),
        (
            r#"{"workspaceId":"w1","model":"  ","effort":null}"#,
            "nothing to change",
        ),
    ] {
        let (app, writes) = app();
        let response = app
            .oneshot(post("/api/sessions/s1/agent", &[], body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            text(response).await,
            json!({ "error": message }).to_string(),
            "{body}"
        );
        assert!(writes.calls().is_empty(), "{body}");
    }
}

#[tokio::test]
async fn the_agent_route_is_unavailable_without_writes() {
    let app = app_with(None, ConductorStatus::Running);
    let response = app
        .oneshot(post("/api/sessions/s1/agent", &[], r#"{"plan":true}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        text(response).await,
        r#"{"error":"writes are unavailable"}"#
    );
}

#[tokio::test]
async fn the_models_route_passes_the_session_and_the_decoded_workspace() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::json(
        200,
        json!({ "ok": true, "models": ["Opus 5.5"] }),
    ));
    let response = app
        .oneshot(request(
            Method::GET,
            "/api/sessions/s%201/models?x=1&workspaceId=%20w%2F1+a%20&y=2",
            &[("x-relay-client", "mcp")],
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        text(response).await,
        json!({ "ok": true, "models": ["Opus 5.5"] }).to_string()
    );
    assert_eq!(
        writes.calls(),
        [Call::ListModels {
            session_id: "s 1".to_owned(),
            workspace_id: Some("w/1 a".to_owned()),
            priority: Priority::Background,
        }]
    );
}

#[tokio::test]
async fn the_models_route_passes_the_service_status() {
    let (app, writes) = app();
    writes.answering(WriteAnswer::error(409, "busy"));
    let response = app.oneshot(get("/api/sessions/s1/models")).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(text(response).await, json!({ "error": "busy" }).to_string());
}

#[tokio::test]
async fn the_models_route_without_a_workspace_passes_none() {
    for uri in [
        "/api/sessions/s1/models",
        "/api/sessions/s1/models?workspaceId=",
        "/api/sessions/s1/models?workspaceId=%20%20",
        "/api/sessions/s1/models?other=w1",
    ] {
        let (app, writes) = app();
        app.oneshot(get(uri)).await.unwrap();
        assert_eq!(
            writes.calls(),
            [Call::ListModels {
                session_id: "s1".to_owned(),
                workspace_id: None,
                priority: Priority::Interactive,
            }],
            "{uri}"
        );
    }
}

#[tokio::test]
async fn the_models_route_needs_the_token() {
    let (app, writes) = app();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/api/sessions/x/models")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(text(response).await, r#"{"error":"unauthorized"}"#);
    assert!(writes.calls().is_empty());
}

#[tokio::test]
async fn the_models_route_is_unavailable_without_writes() {
    let app = app_with(None, ConductorStatus::Running);
    let response = app.oneshot(get("/api/sessions/s1/models")).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        text(response).await,
        r#"{"error":"writes are unavailable"}"#
    );
}

// ---------------------------------------------------------------------------------------------
// Workspace actions: close a chat, set a status, archive, continue
// ---------------------------------------------------------------------------------------------

const STATUS_MESSAGE: &str =
    "status must be one of backlog, in-progress, in-review, done, canceled";

/// What the route answers with a 409 from the service.
fn running_conflict() -> WriteAnswer {
    WriteAnswer::json(
        409,
        json!({ "error": "an agent is running", "agentRunning": true }),
    )
}

async fn assert_conflict_passes_through(request: Request<Body>, writes: &FakeWrites, app: Router) {
    writes.answering(running_conflict());
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(header_of(&response, "content-type"), "application/json");
    assert_eq!(
        text(response).await,
        json!({ "error": "an agent is running", "agentRunning": true }).to_string()
    );
    assert_eq!(writes.calls().len(), 1);
}

async fn assert_refused(app: Router, request: Request<Body>, message: &str) {
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        text(response).await,
        json!({ "error": message }).to_string()
    );
}

#[tokio::test]
async fn closing_a_chat_passes_the_parsed_fields() {
    let (app, writes) = app();
    let response = app
        .oneshot(delete(
            "/api/sessions/s%201",
            br#"{"workspaceId":" w1 ","closeRunning":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::CloseChat {
            session_id: "s 1".to_owned(),
            workspace_id: Some("w1".to_owned()),
            close_running: true,
            priority: Priority::Interactive,
        }
    );
}

#[tokio::test]
async fn closing_a_chat_with_an_empty_body_uses_the_defaults() {
    let (app, writes) = app();
    let response = app.oneshot(delete("/api/sessions/s1", b"")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::CloseChat {
            session_id: "s1".to_owned(),
            workspace_id: None,
            close_running: false,
            priority: Priority::Interactive,
        }
    );
}

#[tokio::test]
async fn closing_a_chat_follows_x_relay_client() {
    let (app, writes) = app();
    app.oneshot(request(
        Method::DELETE,
        "/api/sessions/s1",
        &[("x-relay-client", "mcp")],
        b"{}".to_vec(),
    ))
    .await
    .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::CloseChat {
            session_id: "s1".to_owned(),
            workspace_id: None,
            close_running: false,
            priority: Priority::Background,
        }
    );
}

#[tokio::test]
async fn closing_a_chat_refuses_a_bad_body() {
    for (body, message) in [
        ("nope", "request body must be valid JSON"),
        ("[]", "request body must be a JSON object"),
        (r#"{"workspaceId":1}"#, "workspaceId: must be a string"),
        (
            r#"{"closeRunning":"yes"}"#,
            "closeRunning: must be a boolean",
        ),
    ] {
        let (app, writes) = app();
        assert_refused(app, delete("/api/sessions/s1", body.as_bytes()), message).await;
        assert!(writes.calls().is_empty(), "{body}");
    }
}

#[tokio::test]
async fn closing_a_chat_passes_the_service_answer_through() {
    let (app, writes) = app();
    assert_conflict_passes_through(delete("/api/sessions/s1", b""), &writes, app).await;
}

#[tokio::test]
async fn closing_a_chat_needs_the_token_and_the_service() {
    let (app, writes) = app();
    let unauthorized = Request::builder()
        .method(Method::DELETE)
        .uri("/api/sessions/s1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(unauthorized).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(text(response).await, r#"{"error":"unauthorized"}"#);
    assert!(writes.calls().is_empty());

    let app = app_with(None, ConductorStatus::Running);
    let response = app.oneshot(delete("/api/sessions/s1", b"")).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        text(response).await,
        r#"{"error":"writes are unavailable"}"#
    );
}

#[tokio::test]
async fn deleting_a_session_prompt_still_dismisses_and_does_not_close() {
    let (app, writes) = app();
    app.oneshot(delete("/api/sessions/x/prompt", b""))
        .await
        .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::Dismiss {
            session_id: "x".to_owned()
        }
    );
}

#[tokio::test]
async fn deleting_a_session_path_with_two_segments_is_not_found() {
    let (app, writes) = app();
    let response = app
        .oneshot(delete("/api/sessions/a/b", b"{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(response).await, r#"{"error":"not found"}"#);
    assert!(writes.calls().is_empty());
}

#[tokio::test]
async fn setting_a_status_passes_the_trimmed_status() {
    let (app, writes) = app();
    let response = app
        .oneshot(post(
            "/api/workspaces/w%201/status",
            &[("x-relay-client", "mcp")],
            r#"{"status":" in-review "}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::SetStatus {
            workspace_id: "w 1".to_owned(),
            status: "in-review".to_owned(),
            priority: Priority::Background,
        }
    );
}

#[tokio::test]
async fn setting_a_status_refuses_what_it_cannot_read() {
    for (body, message) in [
        ("", "request body must be valid JSON"),
        ("nope", "request body must be valid JSON"),
        ("[]", "request body must be a JSON object"),
        ("{}", STATUS_MESSAGE),
        (r#"{"status":null}"#, STATUS_MESSAGE),
        (r#"{"status":3}"#, STATUS_MESSAGE),
    ] {
        let (app, writes) = app();
        assert_refused(app, post("/api/workspaces/w1/status", &[], body), message).await;
        assert!(writes.calls().is_empty(), "{body:?}");
    }
}

#[tokio::test]
async fn setting_a_status_leaves_the_value_check_to_the_service() {
    let (app, writes) = app();
    app.oneshot(post(
        "/api/workspaces/w1/status",
        &[],
        r#"{"status":"bogus"}"#,
    ))
    .await
    .unwrap();
    let [call] = only_call(&writes);
    assert_eq!(
        call,
        Call::SetStatus {
            workspace_id: "w1".to_owned(),
            status: "bogus".to_owned(),
            priority: Priority::Interactive,
        }
    );
}

#[tokio::test]
async fn setting_a_status_passes_the_service_answer_through() {
    let (app, writes) = app();
    assert_conflict_passes_through(
        post("/api/workspaces/w1/status", &[], r#"{"status":"done"}"#),
        &writes,
        app,
    )
    .await;
}

#[tokio::test]
async fn archiving_passes_the_flag() {
    let (app, writes) = app();
    app.clone()
        .oneshot(post(
            "/api/workspaces/w%201/archive",
            &[("x-relay-client", "mcp")],
            r#"{"stopAgents":true}"#,
        ))
        .await
        .unwrap();
    app.oneshot(post("/api/workspaces/w2/archive", &[], ""))
        .await
        .unwrap();
    assert_eq!(
        writes.calls(),
        vec![
            Call::Archive {
                workspace_id: "w 1".to_owned(),
                stop_agents: true,
                priority: Priority::Background,
            },
            Call::Archive {
                workspace_id: "w2".to_owned(),
                stop_agents: false,
                priority: Priority::Interactive,
            },
        ]
    );
}

#[tokio::test]
async fn archiving_refuses_a_bad_body() {
    for (body, message) in [
        ("nope", "request body must be valid JSON"),
        ("[]", "request body must be a JSON object"),
        (r#"{"stopAgents":"yes"}"#, "stopAgents: must be a boolean"),
    ] {
        let (app, writes) = app();
        assert_refused(app, post("/api/workspaces/w1/archive", &[], body), message).await;
        assert!(writes.calls().is_empty(), "{body}");
    }
}

#[tokio::test]
async fn archiving_passes_the_service_answer_through() {
    let (app, writes) = app();
    assert_conflict_passes_through(post("/api/workspaces/w1/archive", &[], ""), &writes, app).await;
}

#[tokio::test]
async fn continuing_passes_the_session() {
    let (app, writes) = app();
    app.clone()
        .oneshot(post(
            "/api/workspaces/w%201/continue",
            &[("x-relay-client", "mcp")],
            r#"{"sessionId":" s1 "}"#,
        ))
        .await
        .unwrap();
    app.oneshot(post("/api/workspaces/w2/continue", &[], ""))
        .await
        .unwrap();
    assert_eq!(
        writes.calls(),
        vec![
            Call::Continue {
                workspace_id: "w 1".to_owned(),
                session_id: Some("s1".to_owned()),
                priority: Priority::Background,
            },
            Call::Continue {
                workspace_id: "w2".to_owned(),
                session_id: None,
                priority: Priority::Interactive,
            },
        ]
    );
}

#[tokio::test]
async fn continuing_refuses_a_bad_body() {
    for (body, message) in [
        ("nope", "request body must be valid JSON"),
        ("[]", "request body must be a JSON object"),
        (r#"{"sessionId":1}"#, "sessionId: must be a string"),
    ] {
        let (app, writes) = app();
        assert_refused(app, post("/api/workspaces/w1/continue", &[], body), message).await;
        assert!(writes.calls().is_empty(), "{body}");
    }
}

#[tokio::test]
async fn continuing_passes_the_service_answer_through() {
    let (app, writes) = app();
    assert_conflict_passes_through(post("/api/workspaces/w1/continue", &[], ""), &writes, app)
        .await;
}

#[tokio::test]
async fn the_workspace_action_posts_need_the_token_and_the_service() {
    for uri in [
        "/api/workspaces/w1/status",
        "/api/workspaces/w1/archive",
        "/api/workspaces/w1/continue",
    ] {
        let (app, writes) = app();
        let unauthorized = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(Body::from(r#"{"status":"done"}"#))
            .unwrap();
        let response = app.oneshot(unauthorized).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(text(response).await, r#"{"error":"unauthorized"}"#);
        assert!(writes.calls().is_empty(), "{uri}");

        let app = app_with(None, ConductorStatus::Running);
        let response = app
            .oneshot(post(uri, &[], r#"{"status":"done"}"#))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(
            text(response).await,
            r#"{"error":"writes are unavailable"}"#
        );
    }
}

#[tokio::test]
async fn question_answer_route_passes_identity_and_batch_to_service() {
    let (app, writes) = app();
    let body = json!({"workspaceId":"sample-ws","requestId":"sample-call","answers":[{"selected":[0]},{"selected":[],"other":"Sample text"}]});
    let response = app
        .oneshot(post(
            "/api/sessions/sample-chat/questions/answer",
            &[],
            &body.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        writes.calls(),
        vec![Call::Questions(
            "sample-chat".into(),
            serde_json::from_value(body).unwrap(),
            Priority::Interactive
        )]
    );
}

#[tokio::test]
async fn question_answers_reject_malformed_payloads_before_service() {
    for body in [
        "{}",
        r#"{"workspaceId":"sample-ws","requestId":"sample-call","answers":[{"selected":[-1]}]}"#,
        r#"{"workspaceId":"sample-ws","requestId":"sample-call","answers":[{"selected":[0],"unexpected":true}]}"#,
    ] {
        let (app, writes) = app();
        let response = app
            .oneshot(post(
                "/api/sessions/sample-chat/questions/answer",
                &[],
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(writes.calls().is_empty());
    }
}
