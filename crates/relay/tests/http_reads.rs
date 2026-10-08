//! The read routes over HTTP: statuses, bodies against the golden files, revalidation, the token
//! gate, the "Conductor is not running" answers and the response cache.
//!
//! The three seeds were written independently, so each route group gets a database of its own.

#[allow(dead_code)]
#[path = "support/seed_messages.rs"]
mod seed_messages;
#[allow(dead_code)]
#[path = "support/seed_sessions.rs"]
mod seed_sessions;
#[allow(dead_code)]
#[path = "support/seed_workspaces.rs"]
mod seed_workspaces;
mod support;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{
    AppState, ConductorControl, ConductorStatus, StateResponse, Token,
};
use conductor_remote::db::{ConductorDb, DbError};
use conductor_remote::http::router;
use conductor_remote::reads::snapshot::{Key, Snapshot};
use conductor_remote::reads::{close_when_not_running, Reads};
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use support::TestDb;
use tower::ServiceExt;

const TOKEN: &str = "reads-token";
const NOT_RUNNING: &str = r#"{"error":"Conductor is not running"}"#;
const NO_WORKSPACE: &str = r#"{"error":"workspace not found"}"#;
const INTERNAL: &str = r#"{"error":"internal error"}"#;

/// A relay over one synthetic database and a fake Conductor.
struct Relay {
    test: TestDb,
    conductor: Arc<FakeConductor>,
    reads: Arc<Reads>,
    app: Router,
}

impl Relay {
    fn with(seed: impl FnOnce(&TestDb)) -> Self {
        let test = TestDb::new();
        seed(&test);
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let conductor = Arc::new(FakeConductor::new(ConductorStatus::Running));
        let app = router(AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: conductor.clone(),
            assets: Arc::new(MemoryAssets::default()),
            reads: Some(reads.clone()),
            writes: None,
            notify: None,
            services: Default::default(),
        });
        Self {
            test,
            conductor,
            reads,
            app,
        }
    }

    fn workspaces() -> Self {
        Self::with(|test| seed_workspaces::seed(&test.conn(), test.root()))
    }

    fn sessions() -> Self {
        Self::with(|test| seed_sessions::seed(&test.conn()))
    }

    fn messages() -> Self {
        Self::with(|test| seed_messages::seed(&test.conn()))
    }

    /// A relay whose database file does not exist: any read of it fails.
    fn without_database() -> Self {
        let relay = Self::with(|_| {});
        let missing = relay.test.dir().join("absent").join("conductor.db");
        let reads = Arc::new(Reads::new(
            Arc::new(ConductorDb::new(missing)),
            relay.test.root(),
        ));
        let app = router(AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: relay.conductor.clone(),
            assets: Arc::new(MemoryAssets::default()),
            reads: Some(reads.clone()),
            writes: None,
            notify: None,
            services: Default::default(),
        });
        Self {
            reads,
            app,
            ..relay
        }
    }

    async fn send(&self, request: Request<Body>) -> Response {
        self.app.clone().oneshot(request).await.unwrap()
    }

    async fn get(&self, uri: &str) -> Response {
        self.send(request(uri, Some(TOKEN), None)).await
    }

    /// The JSON body of a 200 answer, with the temporary root replaced by `/ROOT`.
    async fn ok(&self, uri: &str) -> Value {
        let response = self.get(uri).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        without_root(json_of(response).await, self.test.root())
    }
}

fn request(uri: &str, token: Option<&str>, if_none_match: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(etag) = if_none_match {
        builder = builder.header(header::IF_NONE_MATCH, etag);
    }
    builder.body(Body::empty()).unwrap()
}

async fn text_of(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn json_of(response: Response) -> Value {
    serde_json::from_str(&text_of(response).await).unwrap()
}

fn header_of(response: &Response, name: header::HeaderName) -> String {
    response
        .headers()
        .get(name)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

/// Replaces the temporary root in every string of `value` with `/ROOT`.
fn without_root(value: Value, root: &Path) -> Value {
    fn walk(value: Value, root: &str) -> Value {
        match value {
            Value::String(s) => Value::String(s.replace(root, "/ROOT")),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| walk(v, root)).collect()),
            Value::Object(map) => {
                Value::Object(map.into_iter().map(|(k, v)| (k, walk(v, root))).collect())
            }
            other => other,
        }
    }
    walk(value, root.to_str().expect("utf-8 root"))
}

fn golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/tests/contract/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// What `/api/state` answers around the workspaces of a running Conductor.
fn state_with(workspaces: Value) -> Value {
    let mut state = serde_json::to_value(StateResponse::skeleton(ConductorStatus::Running))
        .expect("the skeleton serialises");
    state["workspaces"] = workspaces;
    state
}

/// A 200 answer carries the shared envelope's headers, and a repeat request with its etag is a
/// 304 with no body and the same etag.
async fn assert_revalidates(relay: &Relay, uri: &str) {
    let first = relay.get(uri).await;
    assert_eq!(first.status(), StatusCode::OK, "{uri}");
    assert_eq!(
        header_of(&first, header::CONTENT_TYPE),
        "application/json; charset=utf-8",
        "{uri}"
    );
    assert_eq!(
        header_of(&first, header::CACHE_CONTROL),
        "no-cache",
        "{uri}"
    );
    let etag = header_of(&first, header::ETAG);
    assert!(etag.starts_with("W/\""), "{uri}: {etag}");

    let again = relay.get(uri).await;
    assert_eq!(header_of(&again, header::ETAG), etag, "{uri}");

    let conditional = relay.send(request(uri, Some(TOKEN), Some(&etag))).await;
    assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED, "{uri}");
    assert_eq!(header_of(&conditional, header::ETAG), etag, "{uri}");
    assert_eq!(text_of(conditional).await, "", "{uri}");

    let stale = relay
        .send(request(uri, Some(TOKEN), Some("W/\"other\"")))
        .await;
    assert_eq!(stale.status(), StatusCode::OK, "{uri}");
}

async fn assert_unauthorized(relay: &Relay, uris: &[&str]) {
    for uri in uris {
        let response = relay.send(request(uri, None, None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            text_of(response).await,
            r#"{"error":"unauthorized"}"#,
            "{uri}"
        );
        let wrong = relay.send(request(uri, Some("wrong-token"), None)).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

// ------------------------------------------------------------------ state, repos, workspace

#[tokio::test]
async fn state_serves_the_live_workspaces_inside_the_state_wrapper() {
    let relay = Relay::workspaces();
    let body = relay.ok("/api/state").await;
    assert_eq!(body, state_with(golden("workspaces-state.json")));
    let keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["workspaces", "actuator", "version", "conductor"]);
    assert_eq!(body["conductor"], json!({"running": true}));
}

#[tokio::test]
async fn repos_are_served_inside_the_repos_wrapper() {
    let relay = Relay::workspaces();
    assert_eq!(
        relay.ok("/api/repos").await,
        json!({"repos": golden("workspaces-repos.json")})
    );
}

#[tokio::test]
async fn a_workspace_is_served_inside_the_workspace_wrapper() {
    let relay = Relay::workspaces();
    assert_eq!(
        relay.ok("/api/workspaces/ws-pinned").await,
        json!({"workspace": golden("workspaces-any.json")})
    );
}

#[tokio::test]
async fn an_archived_workspace_is_still_found() {
    let relay = Relay::workspaces();
    let body = relay.ok("/api/workspaces/ws-archived").await;
    assert_eq!(body["workspace"]["archived"], json!(true));
}

#[tokio::test]
async fn an_unknown_workspace_is_a_404() {
    let relay = Relay::workspaces();
    let response = relay.get("/api/workspaces/ws-nobody").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
    assert_eq!(text_of(response).await, NO_WORKSPACE);
}

#[tokio::test]
async fn state_repos_and_workspace_revalidate() {
    let relay = Relay::workspaces();
    for uri in ["/api/state", "/api/repos", "/api/workspaces/ws-pinned"] {
        assert_revalidates(&relay, uri).await;
    }
}

#[tokio::test]
async fn the_token_may_ride_in_the_query() {
    let relay = Relay::workspaces();
    let response = relay
        .send(request(&format!("/api/repos?token={TOKEN}"), None, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn every_route_needs_the_token() {
    let relay = Relay::workspaces();
    assert_unauthorized(
        &relay,
        &[
            "/api/state",
            "/api/repos",
            "/api/workspaces/ws-pinned",
            "/api/workspaces/ws-pinned/sessions",
            "/api/workspaces/ws-pinned/sessions/closed",
            "/api/sessions/msg-chat/messages?after=0",
        ],
    )
    .await;
}

#[tokio::test]
async fn a_workspace_id_is_percent_decoded() {
    let relay = Relay::workspaces();
    relay
        .test
        .conn()
        .execute(
            "INSERT INTO workspaces (local_id, id, directory_name, state) \
             VALUES ('enc-ws', 'a b/c', 'enc-dir', 'ready')",
            [],
        )
        .unwrap();
    let body = relay.ok("/api/workspaces/a%20b%2Fc").await;
    assert_eq!(body["workspace"]["id"], json!("a b/c"));
    assert_eq!(
        relay.ok("/api/workspaces/a%20b%2Fc/sessions").await,
        json!({"sessions": []})
    );
    assert_eq!(
        relay.ok("/api/workspaces/a%20b%2Fc/sessions/closed").await,
        json!({"sessions": []})
    );
    // The same id undecoded is another id.
    let response = relay.get("/api/workspaces/a%2520b%2Fc").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn paths_that_are_not_read_routes_stay_unknown() {
    let relay = Relay::workspaces();
    for uri in [
        "/api/workspaces/",
        "/api/workspaces/ws-pinned/other",
        "/api/workspaces/ws-pinned/sessions/open",
        "/api/sessions/msg-chat",
        "/api/sessions//messages",
        "/api/repos/extra",
    ] {
        let response = relay.get(uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(text_of(response).await, r#"{"error":"not found"}"#, "{uri}");
    }
    let post = relay
        .send(
            Request::builder()
                .method(Method::POST)
                .uri("/api/repos")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(post.status(), StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------------ sessions

#[tokio::test]
async fn open_sessions_are_served_inside_the_sessions_wrapper() {
    let relay = Relay::sessions();
    assert_eq!(
        relay
            .ok(&format!(
                "/api/workspaces/{}/sessions",
                seed_sessions::WORKSPACE
            ))
            .await,
        json!({"sessions": golden("sessions.json")})
    );
}

#[tokio::test]
async fn closed_sessions_are_served_inside_the_sessions_wrapper() {
    let relay = Relay::sessions();
    assert_eq!(
        relay
            .ok(&format!(
                "/api/workspaces/{}/sessions/closed",
                seed_sessions::WORKSPACE
            ))
            .await,
        json!({"sessions": golden("sessions-closed.json")})
    );
}

#[tokio::test]
async fn an_unknown_workspace_has_no_sessions_but_no_closed_ones_either() {
    let relay = Relay::sessions();
    assert_eq!(
        relay.ok("/api/workspaces/ses-nobody/sessions").await,
        json!({"sessions": []})
    );
    let response = relay
        .get("/api/workspaces/ses-nobody/sessions/closed")
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(text_of(response).await, NO_WORKSPACE);
}

#[tokio::test]
async fn sessions_routes_revalidate() {
    let relay = Relay::sessions();
    for uri in [
        format!("/api/workspaces/{}/sessions", seed_sessions::WORKSPACE),
        format!(
            "/api/workspaces/{}/sessions/closed",
            seed_sessions::WORKSPACE
        ),
    ] {
        assert_revalidates(&relay, &uri).await;
    }
}

// ------------------------------------------------------------------ messages

#[tokio::test]
async fn messages_match_the_golden_file() {
    let relay = Relay::messages();
    assert_eq!(
        relay.ok("/api/sessions/msg-chat/messages?after=0").await,
        golden("messages.json")
    );
}

#[tokio::test]
async fn messages_revalidate() {
    let relay = Relay::messages();
    assert_revalidates(&relay, "/api/sessions/msg-chat/messages?after=0").await;
}

#[tokio::test]
async fn an_after_that_is_not_a_64_bit_integer_is_zero() {
    let relay = Relay::messages();
    let from_start = golden("messages.json");
    for query in [
        "",
        "?after=0",
        "?after=",
        "?after=abc",
        "?after=1.5",
        "?after=1e3",
        "?after=0x10",
        "?after=9223372036854775808",
        "?after=-9223372036854775809",
        "?after=%20",
        "?other=7",
        "?afterwards=7",
        "?after=abc&after=7",
    ] {
        let body = relay
            .ok(&format!("/api/sessions/msg-chat/messages{query}"))
            .await;
        assert_eq!(body, from_start, "{query:?}");
    }
}

#[tokio::test]
async fn after_is_the_row_cursor() {
    let relay = Relay::messages();
    let expected = |after: i64| {
        serde_json::to_value(relay.reads.get_messages("msg-chat", after).unwrap()).unwrap()
    };
    for after in [
        3,
        seed_messages::CHAT_FIRST_ROWID,
        seed_messages::CHAT_LAST_ROWID,
        1_000_000,
        i64::MAX,
    ] {
        let uri = format!("/api/sessions/msg-chat/messages?after={after}");
        assert_eq!(relay.ok(&uri).await, expected(after), "{after}");
    }
    let past_the_end = relay
        .ok(&format!(
            "/api/sessions/msg-chat/messages?after={}",
            seed_messages::CHAT_LAST_ROWID
        ))
        .await;
    assert_eq!(past_the_end["entries"], json!([]));
    assert_eq!(
        past_the_end["cursor"],
        json!(seed_messages::CHAT_LAST_ROWID)
    );
    // The queue snapshot is whole whatever the cursor.
    assert_eq!(
        past_the_end["queued"],
        golden("messages.json")["queued"],
        "the queue does not depend on the cursor"
    );
}

#[tokio::test]
async fn after_is_found_beside_the_token_in_the_query() {
    let relay = Relay::messages();
    let response = relay
        .send(request(
            &format!(
                "/api/sessions/msg-chat/messages?token={TOKEN}&after={}",
                seed_messages::CHAT_LAST_ROWID
            ),
            None,
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_of(response).await["entries"], json!([]));
}

#[tokio::test]
async fn an_unknown_chat_has_no_messages() {
    let relay = Relay::messages();
    assert_eq!(
        relay.ok("/api/sessions/msg-nobody/messages?after=0").await,
        json!({"entries": [], "cursor": 0, "queued": []})
    );
}

// ------------------------------------------------------------------ Conductor not running

const READ_ROUTES: [&str; 5] = [
    "/api/repos",
    "/api/workspaces/ws-pinned",
    "/api/workspaces/ws-pinned/sessions",
    "/api/workspaces/ws-pinned/sessions/closed",
    "/api/sessions/msg-chat/messages?after=0",
];

#[tokio::test]
async fn read_routes_are_503_while_conductor_is_not_running() {
    // The database file does not exist: a route that touched it would answer 500.
    let relay = Relay::without_database();
    relay.conductor.set_status(ConductorStatus::NotRunning);
    for uri in READ_ROUTES {
        let response = relay.get(uri).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            "application/json; charset=utf-8"
        );
        assert_eq!(text_of(response).await, NOT_RUNNING, "{uri}");
    }
}

#[tokio::test]
async fn state_is_the_skeleton_while_conductor_is_not_running() {
    let relay = Relay::workspaces();
    relay.conductor.set_status(ConductorStatus::NotRunning);
    let response = relay.get("/api/state").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        "application/json; charset=utf-8"
    );
    assert_eq!(
        json_of(response).await,
        serde_json::to_value(StateResponse::skeleton(ConductorStatus::NotRunning)).unwrap()
    );
    assert_revalidates(&relay, "/api/state").await;
}

#[tokio::test]
async fn the_routes_answer_again_when_conductor_runs_again() {
    let relay = Relay::workspaces();
    relay.conductor.set_status(ConductorStatus::NotRunning);
    assert_eq!(
        relay.get("/api/repos").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    relay.conductor.set_status(ConductorStatus::Running);
    assert_eq!(
        relay.ok("/api/repos").await,
        json!({"repos": golden("workspaces-repos.json")})
    );
}

#[tokio::test]
async fn no_reads_means_the_skeleton_state_and_503_everywhere_else() {
    let conductor = Arc::new(FakeConductor::new(ConductorStatus::Running));
    let app = router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor,
        assets: Arc::new(MemoryAssets::default()),
        reads: None,
        writes: None,
        notify: None,
        services: Default::default(),
    });
    let state = app
        .clone()
        .oneshot(request("/api/state", Some(TOKEN), None))
        .await
        .unwrap();
    assert_eq!(state.status(), StatusCode::OK);
    assert_eq!(
        json_of(state).await,
        serde_json::to_value(StateResponse::skeleton(ConductorStatus::Running)).unwrap()
    );
    for uri in READ_ROUTES {
        let response = app
            .clone()
            .oneshot(request(uri, Some(TOKEN), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(text_of(response).await, NOT_RUNNING, "{uri}");
    }
}

// ------------------------------------------------------------------ read errors

#[tokio::test]
async fn a_failed_read_is_a_500_that_says_nothing_about_the_cause() {
    let relay = Relay::without_database();
    for uri in [
        "/api/state",
        "/api/repos",
        "/api/workspaces/ws-pinned",
        "/api/workspaces/ws-pinned/sessions",
        "/api/workspaces/ws-pinned/sessions/closed",
        "/api/sessions/msg-chat/messages?after=0",
    ] {
        let response = relay.get(uri).await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{uri}"
        );
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
        assert_eq!(text_of(response).await, INTERNAL, "{uri}");
    }
}

// ------------------------------------------------------------------ the cache

fn insert_ready_workspace(relay: &Relay, id: &str) {
    relay
        .test
        .conn()
        .execute(
            "INSERT INTO workspaces (local_id, id, directory_name, state) \
             VALUES (?1, ?1, ?1, 'ready')",
            [id],
        )
        .unwrap();
}

fn worktree_of(state: &Value, id: &str) -> Value {
    state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == id)
        .unwrap_or_else(|| panic!("no workspace {id}"))["worktree"]
        .clone()
}

fn ids_of(state: &Value) -> Vec<String> {
    state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap().to_owned())
        .collect()
}

/// `ws-space` has a directory without a `.git` entry: creating one changes its worktree in a
/// fresh read of the workspaces and leaves the database alone.
fn give_ws_space_a_checkout(relay: &Relay) {
    std::fs::create_dir(relay.test.root().join("compass/needle/.git")).unwrap();
}

#[tokio::test]
async fn an_unchanged_database_answers_state_from_the_cache() {
    let relay = Relay::workspaces();
    let first = relay.ok("/api/state").await;
    assert_eq!(worktree_of(&first, "ws-space"), Value::Null);

    // A change the database does not see: only a fresh read would show it.
    give_ws_space_a_checkout(&relay);
    let second = relay.ok("/api/state").await;
    assert_eq!(second, first);
    assert_eq!(worktree_of(&second, "ws-space"), Value::Null);
}

#[tokio::test]
async fn a_committed_row_shows_in_the_next_state() {
    let relay = Relay::workspaces();
    let before = relay.ok("/api/state").await;
    assert!(!ids_of(&before).contains(&"ws-fresh".to_owned()));
    give_ws_space_a_checkout(&relay);

    insert_ready_workspace(&relay, "ws-fresh");
    let after = relay.ok("/api/state").await;
    assert!(ids_of(&after).contains(&"ws-fresh".to_owned()));
    // The rebuild read everything again.
    assert_eq!(
        worktree_of(&after, "ws-space"),
        json!("/ROOT/compass/needle")
    );

    // And the new answer is the cached one from now on.
    let etag = header_of(&relay.get("/api/state").await, header::ETAG);
    let stale = relay
        .send(request("/api/state", Some(TOKEN), Some(&etag)))
        .await;
    assert_eq!(stale.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn a_committed_row_shows_in_the_next_sessions_answer() {
    let relay = Relay::sessions();
    let uri = format!("/api/workspaces/{}/sessions", seed_sessions::WORKSPACE);
    let before = relay.ok(&uri).await;
    let count = before["sessions"].as_array().unwrap().len();

    relay
        .test
        .conn()
        .execute(
            "INSERT INTO sessions (id, agent_type, created_at, updated_at, workspace_id, is_hidden) \
             VALUES ('ses-fresh', 'claude', '2030-01-01 00:00:00', '2030-01-01 00:00:00', ?1, 0)",
            [seed_sessions::WORKSPACE],
        )
        .unwrap();
    let after = relay.ok(&uri).await;
    let sessions = after["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), count + 1);
    assert_eq!(sessions.last().unwrap()["id"], json!("ses-fresh"));
}

#[test]
fn the_snapshot_builds_once_per_database_state_and_key() {
    let test = TestDb::new();
    let db = test.db();
    let reads = Reads::new(db.clone(), test.root());
    let snapshot = reads.snapshot();
    let builds = std::cell::Cell::new(0);
    let build = |body: &'static str| {
        || -> Result<Vec<u8>, DbError> {
            builds.set(builds.get() + 1);
            Ok(body.as_bytes().to_vec())
        }
    };

    let state = |snapshot: &conductor_remote::reads::snapshot::Snapshot| {
        snapshot
            .get_or_build(&db, Key::State, build("state"))
            .unwrap()
    };
    assert_eq!(state(snapshot), b"state");
    assert_eq!(state(snapshot), b"state");
    assert_eq!(builds.get(), 1);

    // Another key is another body.
    let other = Key::Sessions("w".to_owned());
    assert_eq!(
        snapshot
            .get_or_build(&db, other.clone(), build("w"))
            .unwrap(),
        b"w"
    );
    assert_eq!(
        snapshot
            .get_or_build(&db, other.clone(), build("w2"))
            .unwrap(),
        b"w"
    );
    assert_eq!(builds.get(), 2);

    // A commit from another connection drops every body.
    test.conn()
        .execute(
            "INSERT INTO workspaces (local_id, id, state) VALUES ('s1', 's1', 'ready')",
            [],
        )
        .unwrap();
    assert_eq!(state(snapshot), b"state");
    assert_eq!(builds.get(), 3);
    assert_eq!(
        snapshot
            .get_or_build(&db, other.clone(), build("w3"))
            .unwrap(),
        b"w3"
    );
    assert_eq!(builds.get(), 4);

    // `clear` and `close` drop them too.
    snapshot.clear();
    assert_eq!(state(snapshot), b"state");
    assert_eq!(builds.get(), 5);
    reads.close();
    assert_eq!(state(snapshot), b"state");
    assert_eq!(builds.get(), 6);
}

#[test]
fn a_failed_build_caches_nothing() {
    let test = TestDb::new();
    let reads = Reads::new(test.db(), test.root());
    let snapshot = reads.snapshot();
    let failed: Result<Vec<u8>, DbError> = snapshot.get_or_build(reads.db(), Key::State, || {
        Err(DbError::Query(rusqlite::Error::InvalidQuery))
    });
    assert!(failed.is_err());
    let built = snapshot
        .get_or_build(reads.db(), Key::State, || Ok::<_, DbError>(b"ok".to_vec()))
        .unwrap();
    assert_eq!(built, b"ok");
}

#[test]
fn the_state_body_is_rebuilt_after_its_maximum_age() {
    let test = TestDb::new();
    let db = test.db();
    let snapshot = Snapshot::with_state_max_age(Duration::ZERO);
    let builds = std::cell::Cell::new(0);
    let build = || -> Result<Vec<u8>, DbError> {
        builds.set(builds.get() + 1);
        Ok(format!("build {}", builds.get()).into_bytes())
    };
    let sessions = Key::Sessions("w".to_owned());

    // The database does not change between the calls.
    assert_eq!(
        snapshot.get_or_build(&db, Key::State, build).unwrap(),
        b"build 1"
    );
    assert_eq!(
        snapshot.get_or_build(&db, Key::State, build).unwrap(),
        b"build 2"
    );
    assert_eq!(builds.get(), 2);

    // A sessions body has no age limit.
    assert_eq!(
        snapshot.get_or_build(&db, sessions.clone(), build).unwrap(),
        b"build 3"
    );
    assert_eq!(
        snapshot.get_or_build(&db, sessions.clone(), build).unwrap(),
        b"build 3"
    );
    assert_eq!(builds.get(), 3);

    // A long maximum age keeps the state body.
    let patient = Snapshot::with_state_max_age(Duration::from_secs(3600));
    assert_eq!(
        patient.get_or_build(&db, Key::State, build).unwrap(),
        b"build 4"
    );
    assert_eq!(
        patient.get_or_build(&db, Key::State, build).unwrap(),
        b"build 4"
    );
    assert_eq!(builds.get(), 4);
}

#[test]
fn building_one_key_does_not_block_another() {
    let test = TestDb::new();
    let db = test.db();
    let snapshot = Snapshot::default();
    let state_builds = AtomicUsize::new(0);
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let (sessions_tx, sessions_rx) = mpsc::channel::<Vec<u8>>();

    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            snapshot.get_or_build(&db, Key::State, || -> Result<Vec<u8>, DbError> {
                state_builds.fetch_add(1, Ordering::SeqCst);
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                Ok(b"first".to_vec())
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // The state build is held inside its closure: another key is built and returned.
        scope.spawn(|| {
            let body = snapshot
                .get_or_build(&db, Key::Sessions("w".to_owned()), || {
                    Ok::<_, DbError>(b"sessions".to_vec())
                })
                .unwrap();
            sessions_tx.send(body).unwrap();
        });
        let sessions = sessions_rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(sessions.as_deref(), Ok(b"sessions".as_slice()));

        // A second caller of the state key waits for the first build and shares its body.
        let second = scope.spawn(|| {
            snapshot.get_or_build(&db, Key::State, || -> Result<Vec<u8>, DbError> {
                state_builds.fetch_add(1, Ordering::SeqCst);
                Ok(b"second".to_vec())
            })
        });
        std::thread::sleep(Duration::from_millis(100));
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), b"first");
        assert_eq!(second.join().unwrap().unwrap(), b"first");
    });
    assert_eq!(state_builds.load(Ordering::SeqCst), 1);
}

/// Waits until `check` holds, for at most five seconds.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conductor_stopping_closes_the_database_and_drops_the_cache() {
    let relay = Relay::workspaces();
    let watcher = tokio::spawn(close_when_not_running(
        relay.reads.clone(),
        relay.conductor.subscribe(),
    ));

    let first = relay.ok("/api/state").await;
    assert_eq!(worktree_of(&first, "ws-space"), Value::Null);
    let generation = relay.reads.db().data_version().unwrap().generation;
    give_ws_space_a_checkout(&relay);

    relay.conductor.set_status(ConductorStatus::NotRunning);
    // `data_version` reopens a closed connection, so a new generation shows the close.
    eventually("the connection to be closed", || {
        relay.reads.db().data_version().unwrap().generation != generation
    })
    .await;

    // The database is unchanged, but nothing is served from before the stop.
    relay.conductor.set_status(ConductorStatus::Running);
    let after = relay.ok("/api/state").await;
    assert_eq!(
        worktree_of(&after, "ws-space"),
        json!("/ROOT/compass/needle")
    );
    watcher.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_database_reopens_on_the_first_read_after_conductor_runs_again() {
    let relay = Relay::workspaces();
    let watcher = tokio::spawn(close_when_not_running(
        relay.reads.clone(),
        relay.conductor.subscribe(),
    ));
    relay.ok("/api/repos").await;
    let generation = relay.reads.db().data_version().unwrap().generation;

    relay.conductor.set_status(ConductorStatus::NotRunning);
    eventually("the connection to be closed", || {
        relay.reads.db().data_version().unwrap().generation != generation
    })
    .await;
    relay.conductor.set_status(ConductorStatus::Running);
    assert_eq!(
        relay.ok("/api/repos").await,
        json!({"repos": golden("workspaces-repos.json")})
    );
    watcher.abort();
}
