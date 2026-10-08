//! `/api/state` with parked prompts, and the viewing stamp of the messages route, over fake
//! `WriteService` and `NotifyService` implementations and synthetic databases.

#[allow(dead_code)]
#[path = "support/seed_messages.rs"]
mod seed_messages;
#[allow(dead_code)]
#[path = "support/seed_workspaces.rs"]
mod seed_workspaces;
mod support;

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
use conductor_remote::notify::{DeviceInfo, NotifyService, PushConfig, Subscription};
use conductor_remote::reads::Reads;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use support::TestDb;
use tower::ServiceExt;

const TOKEN: &str = "parked-token";

/// A write service that only knows its parked prompts.
struct FakeWrites {
    parked: Mutex<Vec<Value>>,
}

impl FakeWrites {
    fn new(parked: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            parked: Mutex::new(parked),
        })
    }

    fn set(&self, parked: Vec<Value>) {
        *self.parked.lock().unwrap() = parked;
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
        self.parked.lock().unwrap().clone()
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
        Vec::new()
    }

    fn chat_history(&self, _workspace_id: &str) -> Value {
        json!({})
    }
}

/// A notifier that only records what devices are viewing.
#[derive(Default)]
struct FakeNotify {
    viewing: Mutex<Vec<(String, String)>>,
}

impl FakeNotify {
    fn viewing(&self) -> Vec<(String, String)> {
        self.viewing.lock().unwrap().clone()
    }
}

impl NotifyService for FakeNotify {
    fn config(&self) -> Result<PushConfig, String> {
        Err("not used".to_owned())
    }

    fn subscribe(
        &self,
        _subscription: Subscription,
        _label: Option<String>,
    ) -> Result<(String, Vec<DeviceInfo>), String> {
        Err("not used".to_owned())
    }

    fn unsubscribe(&self, _endpoint: &str) -> Result<(bool, Vec<DeviceInfo>), String> {
        Err("not used".to_owned())
    }

    fn test(&self, _device_id: String) -> BoxFuture<Result<(), String>> {
        Box::pin(async { Err("not used".to_owned()) })
    }

    fn note_viewing(&self, device_id: &str, session_id: &str) {
        self.viewing
            .lock()
            .unwrap()
            .push((device_id.to_owned(), session_id.to_owned()));
    }
}

fn router_over(
    reads: &Arc<Reads>,
    writes: Option<Arc<FakeWrites>>,
    notify: Option<Arc<FakeNotify>>,
) -> Router {
    conductor_remote::http::router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
        assets: Arc::new(MemoryAssets::default()),
        reads: Some(reads.clone()),
        writes: writes.map(|writes| writes as Arc<dyn WriteService>),
        notify: notify.map(|notify| notify as Arc<dyn NotifyService>),
        services: Default::default(),
    })
}

fn get(uri: &str, device: Option<&str>, if_none_match: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    if let Some(device) = device {
        builder = builder.header("x-relay-device", device);
    }
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

fn entry(workspace: &str, session: &str, text: &str) -> Value {
    json!({
        "workspaceId": workspace,
        "sessionId": session,
        "text": text,
        "status": "waiting",
        "attempts": 0,
        "createdAt": 1_000,
        "reason": "window busy",
    })
}

fn workspaces_relay() -> (TestDb, Arc<Reads>) {
    let test = TestDb::new();
    seed_workspaces::seed(&test.conn(), test.root());
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

#[tokio::test]
async fn state_without_parked_prompts_is_byte_identical_to_the_answer_without_writes() {
    let (_test, reads) = workspaces_relay();
    let plain = router_over(&reads, None, None);
    let with_writes = router_over(&reads, Some(FakeWrites::new(Vec::new())), None);

    let expected = send(&plain, get("/api/state", None, None)).await;
    let actual = send(&with_writes, get("/api/state", None, None)).await;

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
        .all(|workspace| workspace.get("parked_prompts").is_none()));
}

#[tokio::test]
async fn only_workspaces_with_entries_carry_them_in_the_order_given() {
    let (_test, reads) = workspaces_relay();
    let first = entry("ws-pinned", "s-1", "first");
    let second = entry("ws-iso", "s-2", "second");
    let third = entry("ws-pinned", "s-3", "third");
    let stranger = entry("ws-nobody", "s-4", "nobody has this workspace");
    let writes = FakeWrites::new(vec![first.clone(), second.clone(), third.clone(), stranger]);
    let app = router_over(&reads, Some(writes), None);

    let response = send(&app, get("/api/state", None, None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let state: Value = serde_json::from_slice(&body_of(response).await).unwrap();

    let carrying: Vec<&str> = state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|workspace| workspace.get("parked_prompts").is_some())
        .map(|workspace| workspace["id"].as_str().unwrap())
        .collect();
    assert_eq!(carrying.len(), 2, "{carrying:?}");
    assert!(carrying.contains(&"ws-pinned") && carrying.contains(&"ws-iso"));
    assert_eq!(
        workspace(&state, "ws-pinned")["parked_prompts"],
        json!([first, third])
    );
    assert_eq!(
        workspace(&state, "ws-iso")["parked_prompts"],
        json!([second])
    );
}

#[tokio::test]
async fn the_etag_changes_when_an_entry_appears_and_the_cached_body_is_not_kept_changed() {
    let (_test, reads) = workspaces_relay();
    let writes = FakeWrites::new(Vec::new());
    let app = router_over(&reads, Some(writes.clone()), None);

    let before = send(&app, get("/api/state", None, None)).await;
    let before_tag = etag_of(&before);
    let before_body = body_of(before).await;

    writes.set(vec![entry("ws-pinned", "s-1", "hello")]);
    let during = send(&app, get("/api/state", None, Some(&before_tag))).await;
    assert_eq!(
        during.status(),
        StatusCode::OK,
        "the old etag no longer matches"
    );
    let during_tag = etag_of(&during);
    assert_ne!(during_tag, before_tag);

    let revalidated = send(&app, get("/api/state", None, Some(&during_tag))).await;
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);

    writes.set(Vec::new());
    let after = send(&app, get("/api/state", None, None)).await;
    assert_eq!(etag_of(&after), before_tag);
    assert_eq!(body_of(after).await, before_body);
}

fn messages_relay() -> (TestDb, Arc<Reads>) {
    let test = TestDb::new();
    seed_messages::seed(&test.conn());
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    (test, reads)
}

#[tokio::test]
async fn a_messages_request_with_the_header_notes_the_device_and_chat() {
    let (_test, reads) = messages_relay();
    let notify = Arc::new(FakeNotify::default());
    let app = router_over(&reads, None, Some(notify.clone()));

    let first = send(
        &app,
        get(
            "/api/sessions/msg-chat/messages?after=0",
            Some("dev-1"),
            None,
        ),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let tag = etag_of(&first);
    assert_eq!(
        notify.viewing(),
        vec![("dev-1".to_owned(), "msg-chat".to_owned())]
    );

    let repeat = send(
        &app,
        get(
            "/api/sessions/msg-chat/messages?after=0",
            Some("dev-1"),
            Some(&tag),
        ),
    )
    .await;
    assert_eq!(repeat.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        notify.viewing(),
        vec![
            ("dev-1".to_owned(), "msg-chat".to_owned()),
            ("dev-1".to_owned(), "msg-chat".to_owned())
        ]
    );
}

#[tokio::test]
async fn without_the_header_or_with_an_empty_one_nothing_is_noted() {
    let (_test, reads) = messages_relay();
    let notify = Arc::new(FakeNotify::default());
    let app = router_over(&reads, None, Some(notify.clone()));

    let bare = send(&app, get("/api/sessions/msg-chat/messages", None, None)).await;
    assert_eq!(bare.status(), StatusCode::OK);
    let empty = send(&app, get("/api/sessions/msg-chat/messages", Some(""), None)).await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(notify.viewing(), Vec::<(String, String)>::new());
}

#[tokio::test]
async fn another_route_with_the_header_notes_nothing() {
    let (_test, reads) = workspaces_relay();
    let notify = Arc::new(FakeNotify::default());
    let app = router_over(&reads, None, Some(notify.clone()));

    let response = send(&app, get("/api/state", Some("dev-1"), None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(notify.viewing().is_empty());
}
