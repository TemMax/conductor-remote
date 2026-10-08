//! `/api/state` with the pending first prompt and the sessions list with the chat history, over
//! fake `WriteService` implementations and synthetic databases.

#[allow(dead_code)]
#[path = "support/seed_sessions.rs"]
mod seed_sessions;
#[allow(dead_code)]
#[path = "support/seed_workspaces.rs"]
mod seed_workspaces;
mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::body::Bytes;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Priority, Token};
use conductor_remote::delivery::{
    BoxFuture, CreateRequest, SendRequest, SplitRequest, WriteAnswer, WriteService,
};
use conductor_remote::reads::Reads;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use support::TestDb;
use tower::ServiceExt;

const TOKEN: &str = "m4-reads-token";

/// A write service that only knows its pending first prompts and its chat links.
#[derive(Default)]
struct FakeWrites {
    pending: Mutex<Vec<Value>>,
    history: Mutex<HashMap<String, Value>>,
}

impl FakeWrites {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn set_pending(&self, pending: Vec<Value>) {
        *self.pending.lock().unwrap() = pending;
    }

    fn set_history(&self, workspace_id: &str, history: Value) {
        self.history
            .lock()
            .unwrap()
            .insert(workspace_id.to_owned(), history);
    }
}

impl WriteService for FakeWrites {
    fn available(&self) -> bool {
        true
    }

    fn send_prompt(&self, _request: SendRequest) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn stop_turn(
        &self,
        _session_id: String,
        _workspace_id: Option<String>,
        _priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn new_chat(&self, _workspace_id: String, _priority: Priority) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn parked_prompts(&self) -> Vec<Value> {
        Vec::new()
    }

    fn dismiss_parked(&self, _session_id: String) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(404, "no parked prompt") })
    }

    fn upload_attachment(
        &self,
        _session_id: String,
        _workspace_id: Option<String>,
        _name: String,
        _bytes: Bytes,
    ) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn stage_attachment(&self, _name: String, _bytes: Bytes) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn discard_staged(&self, _stage_id: String) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn merge(&self, _workspace_id: String) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn restore_chat(
        &self,
        _session_id: String,
        _workspace_id: Option<String>,
        _priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn join_history(
        &self,
        _session_id: String,
        _workspace_id: String,
        _previous_session_id: String,
    ) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn split_chat(
        &self,
        _session_id: String,
        _request: SplitRequest,
        _priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn create_workspace(
        &self,
        _request: CreateRequest,
        _priority: Priority,
    ) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn dismiss_first_prompt(&self, _workspace_id: String) -> BoxFuture<WriteAnswer> {
        Box::pin(async { WriteAnswer::error(500, "not used") })
    }

    fn pending_prompts(&self) -> Vec<Value> {
        self.pending.lock().unwrap().clone()
    }

    fn chat_history(&self, workspace_id: &str) -> Value {
        self.history
            .lock()
            .unwrap()
            .get(workspace_id)
            .cloned()
            .unwrap_or_else(|| json!({}))
    }
}

fn router_over(reads: &Arc<Reads>, writes: Option<Arc<FakeWrites>>) -> Router {
    conductor_remote::http::router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
        assets: Arc::new(MemoryAssets::default()),
        reads: Some(reads.clone()),
        writes: writes.map(|writes| writes as Arc<dyn WriteService>),
        notify: None,
        services: Default::default(),
    })
}

fn get(uri: &str, if_none_match: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    if let Some(etag) = if_none_match {
        builder = builder.header(header::IF_NONE_MATCH, etag);
    }
    builder.body(Body::empty()).unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

async fn body_of(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

fn etag_of(response: &Response) -> String {
    response
        .headers()
        .get(header::ETAG)
        .expect("an etag")
        .to_str()
        .unwrap()
        .to_owned()
}

fn pending(workspace: &str, text: &str) -> Value {
    json!({
        "workspaceId": workspace,
        "text": text,
        "status": "waiting",
        "attempts": 0,
        "earlyAttempts": 0,
        "sendImmediately": false,
        "attachmentIds": [],
        "createdAt": 1_000,
    })
}

fn link(previous: &str, title: &str) -> Value {
    json!({ "previousSessionId": previous, "title": title, "createdAt": "2026-01-01T00:00:00Z" })
}

fn workspaces_relay() -> (TestDb, Arc<Reads>) {
    let test = TestDb::new();
    seed_workspaces::seed(&test.conn(), test.root());
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    (test, reads)
}

fn sessions_relay() -> (TestDb, Arc<Reads>) {
    let test = TestDb::new();
    seed_sessions::seed(&test.conn());
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    (test, reads)
}

fn workspace<'a>(state: &'a Value, id: &str) -> &'a Value {
    state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|workspace| workspace["id"] == id)
        .unwrap_or_else(|| panic!("workspace {id} is in the state"))
}

const SESSIONS: &str = "/api/workspaces/ses-workspace/sessions";

#[tokio::test]
async fn state_without_a_pending_prompt_is_byte_identical_to_the_answer_without_writes() {
    let (_test, reads) = workspaces_relay();
    let plain = router_over(&reads, None);
    let with_writes = router_over(&reads, Some(FakeWrites::new()));

    let expected = send(&plain, get("/api/state", None)).await;
    let actual = send(&with_writes, get("/api/state", None)).await;

    assert_eq!(expected.status(), StatusCode::OK);
    assert_eq!(actual.status(), StatusCode::OK);
    assert_eq!(etag_of(&actual), etag_of(&expected));
    let expected = body_of(expected).await;
    assert_eq!(body_of(actual).await, expected);
    let state: Value = serde_json::from_slice(&expected).unwrap();
    assert!(state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .all(|workspace| workspace.get("pending_prompt").is_none()));
}

#[tokio::test]
async fn only_the_workspace_with_a_pending_prompt_carries_it() {
    let (_test, reads) = workspaces_relay();
    let entry = pending("ws-iso", "first words");
    let writes = FakeWrites::new();
    writes.set_pending(vec![
        entry.clone(),
        pending("ws-nobody", "no such workspace"),
    ]);
    let app = router_over(&reads, Some(writes));

    let response = send(&app, get("/api/state", None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let state: Value = serde_json::from_slice(&body_of(response).await).unwrap();

    let carrying: Vec<&str> = state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|workspace| workspace.get("pending_prompt").is_some())
        .map(|workspace| workspace["id"].as_str().unwrap())
        .collect();
    assert_eq!(carrying, vec!["ws-iso"]);
    assert_eq!(workspace(&state, "ws-iso")["pending_prompt"], entry);
}

#[tokio::test]
async fn the_state_etag_follows_the_pending_prompt_and_the_cache_is_not_kept_changed() {
    let (_test, reads) = workspaces_relay();
    let writes = FakeWrites::new();
    let app = router_over(&reads, Some(writes.clone()));

    let before = send(&app, get("/api/state", None)).await;
    let before_tag = etag_of(&before);
    let before_body = body_of(before).await;

    writes.set_pending(vec![pending("ws-pinned", "hello")]);
    let during = send(&app, get("/api/state", Some(&before_tag))).await;
    assert_eq!(
        during.status(),
        StatusCode::OK,
        "the old etag no longer matches"
    );
    let during_tag = etag_of(&during);
    assert_ne!(during_tag, before_tag);

    let revalidated = send(&app, get("/api/state", Some(&during_tag))).await;
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);

    writes.set_pending(Vec::new());
    let after = send(&app, get("/api/state", None)).await;
    assert_eq!(etag_of(&after), before_tag);
    assert_eq!(body_of(after).await, before_body);
}

#[tokio::test]
async fn sessions_with_an_empty_chat_history_are_byte_identical_to_the_answer_without_writes() {
    let (_test, reads) = sessions_relay();
    let plain = router_over(&reads, None);
    let writes = FakeWrites::new();
    writes.set_history("ses-workspace", json!({}));
    let with_writes = router_over(&reads, Some(writes));

    let expected = send(&plain, get(SESSIONS, None)).await;
    let actual = send(&with_writes, get(SESSIONS, None)).await;

    assert_eq!(expected.status(), StatusCode::OK);
    assert_eq!(actual.status(), StatusCode::OK);
    assert_eq!(etag_of(&actual), etag_of(&expected));
    let expected = body_of(expected).await;
    assert_eq!(body_of(actual).await, expected);
    let body: Value = serde_json::from_slice(&expected).unwrap();
    assert!(body.get("chat_history").is_none());
}

#[tokio::test]
async fn sessions_carry_the_chat_history_of_their_workspace() {
    let (_test, reads) = sessions_relay();
    let writes = FakeWrites::new();
    let history = json!({ "ses-claude-5m": link("ses-codex-ultra", "Original") });
    writes.set_history("ses-workspace", history.clone());
    writes.set_history("ws-other", json!({ "x": link("y", "Elsewhere") }));
    let app = router_over(&reads, Some(writes));

    let response = send(&app, get(SESSIONS, None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&body_of(response).await).unwrap();

    assert_eq!(body["chat_history"], history);
    assert!(body["sessions"].as_array().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn the_sessions_etag_follows_the_chat_history_and_the_cache_is_not_kept_changed() {
    let (_test, reads) = sessions_relay();
    let writes = FakeWrites::new();
    let app = router_over(&reads, Some(writes.clone()));

    let before = send(&app, get(SESSIONS, None)).await;
    let before_tag = etag_of(&before);
    let before_body = body_of(before).await;

    writes.set_history("ses-workspace", json!({ "a": link("b", "Old chat") }));
    let during = send(&app, get(SESSIONS, Some(&before_tag))).await;
    assert_eq!(
        during.status(),
        StatusCode::OK,
        "the old etag no longer matches"
    );
    let during_tag = etag_of(&during);
    assert_ne!(during_tag, before_tag);

    let revalidated = send(&app, get(SESSIONS, Some(&during_tag))).await;
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);

    writes.set_history("ses-workspace", json!({}));
    let after = send(&app, get(SESSIONS, None)).await;
    assert_eq!(etag_of(&after), before_tag);
    assert_eq!(body_of(after).await, before_body);
}
