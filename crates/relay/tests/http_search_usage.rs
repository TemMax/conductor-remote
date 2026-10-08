//! The search and usage routes over HTTP: `/api/search`, `/api/usage` and `/api/usage/tools`.
//! Their answers, every failure, the parameter rules, the token gate and the "Conductor is not
//! running" answer.
//!
//! Search runs over the synthetic database of `seed_search` and a real `SearchIndex` built in a
//! temporary directory. Plan usage runs over a fake `PlanProbe`, so no CLI is started; tool usage
//! scans the synthetic database.

#[path = "support/seed_search.rs"]
mod seed_search;
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Token};
use conductor_remote::db::ConductorDb;
use conductor_remote::http::router;
use conductor_remote::reads::Reads;
use conductor_remote::search::index::SearchIndex;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use conductor_remote::usage::plan::{PlanProbe, PlanUsageService, ProbeError};
use conductor_remote::usage::tools::ToolUsageService;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "search-usage-token";
const INTERNAL: &str = r#"{"error":"internal error"}"#;
const UNAVAILABLE: &str = r#"{"error":"usage is not available"}"#;
const NOT_RUNNING: &str = r#"{"error":"Conductor is not running"}"#;
const BAD_RANGE: &str = r#"{"error":"Choose 24h, 7d, or 30d for tool usage."}"#;
const TOO_LONG: &str = r#"{"error":"Tool usage took too long to read. Try a shorter range."}"#;

// ------------------------------------------------------------------ a fake probe

/// Answers Claude's usage from a fixed value, finds no Codex, and counts its runs.
struct FakeProbe {
    runs: AtomicUsize,
}

impl PlanProbe for FakeProbe {
    fn claude(&self) -> Result<Value, ProbeError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({
            "subscription_type": "max",
            "rate_limits_available": true,
            "rate_limits": {
                "limits": [
                    { "kind": "session", "percent": 17, "resets_at": "2026-09-03T01:19:59Z" }
                ]
            }
        }))
    }

    fn codex(&self) -> Result<Value, ProbeError> {
        Err(ProbeError::NotInstalled)
    }
}

// ------------------------------------------------------------------ relay

/// What a relay is given besides the database.
struct Setup {
    index: bool,
    plan: bool,
    tools: Option<Duration>,
    fill: fn(&Connection),
}

impl Setup {
    /// Search over an index, plan usage over a fake probe and tool usage with the usual limits.
    fn full() -> Self {
        Self {
            index: true,
            plan: true,
            tools: Some(Duration::from_secs(60)),
            fill: nothing,
        }
    }
}

fn nothing(_: &Connection) {}

struct Relay {
    /// Keeps the database and the workspaces root alive.
    _test: TestDb,
    /// Keeps the index file alive.
    _index_dir: TempDir,
    probe: Arc<FakeProbe>,
    conductor: Arc<FakeConductor>,
    app: Router,
}

impl Relay {
    fn build(setup: Setup) -> Self {
        let test = TestDb::new();
        {
            let conn = test.conn();
            seed_search::seed(&conn, test.root());
            (setup.fill)(&conn);
        }
        let index_dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(FakeProbe {
            runs: AtomicUsize::new(0),
        });
        let mut reads = Reads::new(test.db(), test.root());
        if setup.index {
            let index = SearchIndex::open(&index_dir.path().join("search.db")).expect("the index");
            let source = test.db();
            while index.index_step(&source).expect("index step") {}
            reads = reads.with_search(Arc::new(index));
        }
        let plan = PlanUsageService::new(probe.clone());
        let tools = setup.tools.map(|timeout| {
            ToolUsageService::with_limits(
                test.path().to_path_buf(),
                Duration::from_secs(60),
                timeout,
            )
        });
        reads = match (setup.plan, tools) {
            (true, Some(tools)) => reads.with_usage(Arc::new(plan), Arc::new(tools)),
            _ => reads,
        };
        Self::over(test, index_dir, probe, reads)
    }

    fn over(test: TestDb, index_dir: TempDir, probe: Arc<FakeProbe>, reads: Reads) -> Self {
        let conductor = Arc::new(FakeConductor::new(ConductorStatus::Running));
        let app = router(AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: conductor.clone(),
            assets: Arc::new(MemoryAssets::default()),
            reads: Some(Arc::new(reads)),
            writes: None,
            notify: None,
            services: Default::default(),
        });
        Self {
            _test: test,
            _index_dir: index_dir,
            probe,
            conductor,
            app,
        }
    }

    /// Everything, over the synthetic rows and the messages `fill` adds.
    fn with(fill: fn(&Connection)) -> Self {
        Self::build(Setup {
            fill,
            ..Setup::full()
        })
    }

    fn full() -> Self {
        Self::build(Setup::full())
    }

    async fn get(&self, uri: &str) -> Response {
        self.send(request(uri, Some(TOKEN))).await
    }

    async fn send(&self, request: Request<Body>) -> Response {
        self.app.clone().oneshot(request).await.unwrap()
    }

    /// The status and the JSON body of an authorised GET.
    async fn json(&self, uri: &str) -> (StatusCode, Value) {
        let response = self.get(uri).await;
        let status = response.status();
        (status, json_of(response).await)
    }
}

fn request(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
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

/// The workspace ids of a search answer, in order.
fn ids(body: &Value) -> Vec<&str> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["workspace"]["id"].as_str().unwrap())
        .collect()
}

fn sorted(mut values: Vec<&str>) -> Vec<&str> {
    values.sort_unstable();
    values
}

/// Every workspace of the synthetic set says "lantern" in some chat, as follows: `srch-live` in
/// two chats, `srch-archived`, `srch-unknown` and `srch-quiet` once each.
fn lantern(conn: &Connection) {
    let messages = [
        (
            "m01",
            "srch-chat-live",
            "2026-09-10T10:00:00.000Z",
            "lantern lantern wick",
        ),
        (
            "m02",
            "srch-chat-live-2",
            "2026-09-12T10:00:00.000Z",
            "trim the lantern",
        ),
        (
            "m03",
            "srch-chat-archived",
            "2026-08-01T10:00:00.000Z",
            "old lantern notes",
        ),
        (
            "m04",
            "srch-chat-unknown",
            "2026-08-02T10:00:00.000Z",
            "lantern unknown chat",
        ),
        (
            "m05",
            "srch-chat-quiet",
            "2026-08-03T10:00:00.000Z",
            "a lantern on the shelf",
        ),
    ];
    for (id, session, at, text) in messages {
        seed_search::say(conn, id, session, at, text);
    }
}

/// Sixty more workspaces whose names match "ember", so a limit has something to cut.
fn embers(conn: &Connection) {
    for n in 0..60 {
        let id = format!("ember-{n:02}");
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, state,
                                     created_at, updated_at, workspace_name)
             VALUES (?1, ?1, 'srch-repo-two', ?1, ?1, 'ready', '2026-01-01', ?2, ?3)",
            params![
                id,
                format!("2026-04-{:02}", n % 28 + 1),
                format!("Ember {n:02}")
            ],
        )
        .expect("insert workspace");
    }
}

/// A chat updated a minute ago that called the tool `Read` once, for the tool usage routes.
fn one_tool_call(conn: &Connection) {
    conn.execute(
        "INSERT INTO sessions (id, agent_type, is_hidden, updated_at)
         VALUES ('tool-chat', 'claude', 1, strftime('%Y-%m-%d %H:%M:%S', 'now', '-1 minute'))",
        [],
    )
    .expect("insert session");
    let frame = json!({
        "type": "assistant",
        "message": { "content": [
            { "type": "tool_use", "id": "call-1", "name": "Read", "input": { "file": "a.rs" } }
        ] }
    });
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content, created_at)
         VALUES ('tool-m1', 'tool-chat', 'assistant', ?1,
                 strftime('%Y-%m-%d %H:%M:%S', 'now', '-1 minute'))",
        [frame.to_string()],
    )
    .expect("insert message");
}

// ------------------------------------------------------------------ token and Conductor status

const ROUTES: [&str; 5] = [
    "/api/search?q=lantern",
    "/api/usage",
    "/api/usage?refresh=1",
    "/api/usage/tools",
    "/api/usage/tools?range=7d&refresh=1",
];

#[tokio::test]
async fn every_route_needs_the_token() {
    let relay = Relay::full();
    for uri in ROUTES {
        let response = relay.send(request(uri, None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            text_of(response).await,
            r#"{"error":"unauthorized"}"#,
            "{uri}"
        );
        let wrong = relay.send(request(uri, Some("wrong-token"))).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
    assert_eq!(relay.probe.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn every_route_is_503_while_conductor_is_not_running() {
    let relay = Relay::full();
    relay.conductor.set_status(ConductorStatus::NotRunning);
    for uri in ROUTES {
        let response = relay.get(uri).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(text_of(response).await, NOT_RUNNING, "{uri}");
    }
    // The answer comes before any service is asked.
    assert_eq!(relay.probe.runs.load(Ordering::SeqCst), 0);

    relay.conductor.set_status(ConductorStatus::Running);
    let (status, _) = relay.json("/api/usage").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_routes_answer_get_only() {
    let relay = Relay::full();
    for uri in ["/api/search?q=lantern", "/api/usage", "/api/usage/tools"] {
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let response = relay.send(request).await;
        assert_ne!(response.status(), StatusCode::OK, "{uri}");
    }
}

// ------------------------------------------------------------------ search: answers

#[tokio::test]
async fn search_finds_a_workspace_by_what_was_said_in_its_chat() {
    let relay = Relay::with(lantern);
    let response = relay.get("/api/search?q=lantern").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    let body = json_of(response).await;

    assert_eq!(body["query"], json!("lantern"));
    assert_eq!(body["repos"], json!([]));
    assert_eq!(body["index"]["ready"], json!(true));
    assert_eq!(body["index"]["chunks"], json!(5));
    assert_eq!(body["index"]["progress"], json!(1.0));
    assert!(body["index"].get("error").is_none());
    assert_eq!(
        sorted(ids(&body)),
        ["srch-archived", "srch-live", "srch-quiet", "srch-unknown"]
    );
    let live = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["workspace"]["id"] == "srch-live")
        .unwrap();
    assert_eq!(live["hits"], json!(2));
    assert_eq!(live["byName"], json!(true));
    assert!(live["sessionId"]
        .as_str()
        .unwrap()
        .starts_with("srch-chat-live"));
    assert!(!live["snippets"].as_array().unwrap().is_empty());
    let by_chat = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["workspace"]["id"] == "srch-quiet")
        .unwrap();
    assert_eq!(by_chat["byName"], json!(false));
    assert_eq!(by_chat["sessionId"], json!("srch-chat-quiet"));
    assert_eq!(by_chat["sessionTitle"], json!("Shelf talk"));
}

#[tokio::test]
async fn search_finds_a_workspace_by_its_name_without_an_index() {
    let relay = Relay::build(Setup {
        index: false,
        ..Setup::full()
    });
    let (status, body) = relay.json("/api/search?q=beacon").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), ["srch-beacon"]);
    assert_eq!(
        body["index"],
        json!({"chunks": 0, "ready": false, "progress": 0.0})
    );

    // What was said in the chats is not searched.
    let (status, body) = relay.json("/api/search?q=wick").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!ids(&body).contains(&"srch-chat-live"));
}

#[tokio::test]
async fn search_without_words_answers_nothing() {
    let relay = Relay::with(lantern);
    for uri in ["/api/search", "/api/search?q=", "/api/search?q=%20+%20"] {
        let (status, body) = relay.json(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["results"], json!([]), "{uri}");
        assert_eq!(body["index"]["ready"], json!(true), "{uri}");
    }
}

#[tokio::test]
async fn search_values_are_form_decoded() {
    let relay = Relay::with(lantern);
    // `+` and `%20` are spaces; the query keeps the decoded text.
    for uri in [
        "/api/search?q=old+lantern",
        "/api/search?q=old%20lantern",
        "/api/search?q=%6Fld%20lantern",
    ] {
        let (status, body) = relay.json(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["query"], json!("old lantern"), "{uri}");
        assert!(ids(&body).contains(&"srch-archived"), "{uri}");
    }
    let (_, body) = relay.json("/api/search?q=lantern&q=wick").await;
    assert_eq!(body["query"], json!("lantern"), "the first q counts");
}

#[tokio::test]
async fn an_undecodable_q_is_empty() {
    let relay = Relay::with(lantern);
    for uri in [
        "/api/search?q=%ff",
        "/api/search?q=%zz",
        "/api/search?q=%4",
        "/api/search?q=%ff&q=lantern",
    ] {
        let (status, body) = relay.json(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["query"], json!(""), "{uri}");
        assert_eq!(body["results"], json!([]), "{uri}");
    }
}

// ------------------------------------------------------------------ search: parameters

#[tokio::test]
async fn search_repo_is_repeatable_deduplicated_and_drops_empty_ones() {
    let relay = Relay::with(lantern);

    let (_, body) = relay.json("/api/search?q=lantern&repo=srch-one").await;
    assert_eq!(body["repos"], json!(["srch-one"]));
    assert_eq!(sorted(ids(&body)), ["srch-archived", "srch-live"]);

    let (_, body) = relay
        .json("/api/search?q=lantern&repo=srch-two&repo=&repo=srch-one&repo=srch-two")
        .await;
    assert_eq!(body["repos"], json!(["srch-two", "srch-one"]));
    assert_eq!(
        sorted(ids(&body)),
        ["srch-archived", "srch-live", "srch-quiet", "srch-unknown"]
    );

    // Only empty values: every repo, and none listed.
    let (_, body) = relay.json("/api/search?q=lantern&repo=&repo=").await;
    assert_eq!(body["repos"], json!([]));
    assert_eq!(ids(&body).len(), 4);

    // A repo no workspace belongs to matches nothing, never everything.
    let (_, body) = relay.json("/api/search?q=lantern&repo=nowhere").await;
    assert_eq!(body["repos"], json!(["nowhere"]));
    assert_eq!(body["results"], json!([]));
}

#[tokio::test]
async fn search_repo_values_are_form_decoded_and_an_undecodable_one_is_dropped() {
    let relay = Relay::with(lantern);

    let (_, body) = relay.json("/api/search?q=lantern&repo=srch%2Done").await;
    assert_eq!(body["repos"], json!(["srch-one"]));

    let (_, body) = relay
        .json("/api/search?q=lantern&repo=%ff&repo=srch-one&repo=%zz")
        .await;
    assert_eq!(body["repos"], json!(["srch-one"]));
    assert_eq!(sorted(ids(&body)), ["srch-archived", "srch-live"]);

    // Dropped, the search is not limited to any repo.
    let (_, body) = relay.json("/api/search?q=lantern&repo=%ff").await;
    assert_eq!(body["repos"], json!([]));
    assert_eq!(ids(&body).len(), 4);

    // A repo name with a space.
    let (_, body) = relay.json("/api/search?q=lantern&repo=a+b%20c").await;
    assert_eq!(body["repos"], json!(["a b c"]));
}

#[tokio::test]
async fn search_archived_is_false_only_for_zero() {
    let relay = Relay::with(lantern);
    let live = ["srch-live", "srch-quiet", "srch-unknown"];
    let all = ["srch-archived", "srch-live", "srch-quiet", "srch-unknown"];

    let (_, body) = relay.json("/api/search?q=lantern&archived=0").await;
    assert_eq!(sorted(ids(&body)), live);

    for value in ["", "1", "false", "00", "no", "%30%30", "true"] {
        let (_, body) = relay
            .json(&format!("/api/search?q=lantern&archived={value}"))
            .await;
        assert_eq!(sorted(ids(&body)), all, "archived={value}");
    }
    let (_, body) = relay.json("/api/search?q=lantern").await;
    assert_eq!(sorted(ids(&body)), all, "archived missing");

    // A form-encoded zero is a zero.
    let (_, body) = relay.json("/api/search?q=lantern&archived=%30").await;
    assert_eq!(sorted(ids(&body)), live);
    // The first archived counts.
    let (_, body) = relay
        .json("/api/search?q=lantern&archived=0&archived=1")
        .await;
    assert_eq!(sorted(ids(&body)), live);
}

#[tokio::test]
async fn search_limit_is_twelve_by_default_and_clamped_between_one_and_fifty() {
    let relay = Relay::with(embers);
    let count = |relay: &Relay, limit: Option<&str>| {
        let uri = match limit {
            Some(limit) => format!("/api/search?q=ember&limit={limit}"),
            None => "/api/search?q=ember".to_owned(),
        };
        let relay = relay.app.clone();
        async move {
            let response = relay.oneshot(request(&uri, Some(TOKEN))).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            json_of(response).await["results"].as_array().unwrap().len()
        }
    };

    // Missing, empty, not a number, not finite or zero: 12.
    assert_eq!(count(&relay, None).await, 12);
    for limit in [
        "", "abc", "12abc", "NaN", "nan", "inf", "-inf", "Infinity", "0", "0.0", "-0", "%ff", "1,5",
    ] {
        assert_eq!(count(&relay, Some(limit)).await, 12, "limit={limit}");
    }
    // Clamped to 1..50, rounded down.
    for (limit, expected) in [
        ("1", 1),
        ("3", 3),
        ("2.9", 2),
        ("0.5", 1),
        ("0.999", 1),
        ("-4", 1),
        ("-0.5", 1),
        ("49.9", 49),
        ("50", 50),
        ("51", 50),
        ("1000", 50),
        ("1e3", 50),
        ("1e1", 10),
        ("+7", 7),
        (".5e1", 5),
        ("%37", 7),
        ("%20%209%20", 9),
        ("+8+", 8),
    ] {
        assert_eq!(count(&relay, Some(limit)).await, expected, "limit={limit}");
    }
}

#[tokio::test]
async fn search_cuts_the_results_to_the_limit_by_name_and_by_chat_alike() {
    let relay = Relay::with(lantern);
    let (_, body) = relay.json("/api/search?q=lantern&limit=2").await;
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
    assert_eq!(body["index"]["ready"], json!(true));
}

// ------------------------------------------------------------------ plan usage

#[tokio::test]
async fn usage_answers_the_plan_snapshot() {
    let relay = Relay::full();
    let response = relay.get("/api/usage").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    let body = json_of(response).await;

    assert!(body["fetchedAt"].as_i64().unwrap() > 0);
    let providers = body["providers"].as_array().unwrap();
    let names: Vec<&str> = providers
        .iter()
        .map(|p| p["provider"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["claude", "codex", "cursor", "opencode"]);
    let claude = &providers[0];
    assert_eq!(claude["label"], json!("Claude Code"));
    assert_eq!(claude["status"], json!("available"));
    assert_eq!(claude["plan"], json!("max"));
    let window = &claude["buckets"][0]["windows"][0];
    assert_eq!(window["label"], json!("Current session"));
    assert_eq!(window["usedPercent"], json!(17.0));
    assert_eq!(window["resetsAt"], json!(1_788_398_399_000_i64));
    // No Codex was found, and the two others have no plan limits to read.
    assert_eq!(providers[1]["status"], json!("unavailable"));
    assert_eq!(providers[2]["status"], json!("unavailable"));
    assert_eq!(providers[3]["status"], json!("unavailable"));
    assert_eq!(relay.probe.runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn usage_is_cached_until_refresh_is_one() {
    let relay = Relay::full();
    let runs = || relay.probe.runs.load(Ordering::SeqCst);

    relay.json("/api/usage").await;
    relay.json("/api/usage").await;
    assert_eq!(runs(), 1);

    // Only `refresh=1` forces a read.
    for value in ["0", "", "true", "11", "yes"] {
        relay.json(&format!("/api/usage?refresh={value}")).await;
        assert_eq!(runs(), 1, "refresh={value}");
    }
    let (status, _) = relay.json("/api/usage?refresh=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(runs(), 2);
    relay.json("/api/usage?refresh=%31").await;
    assert_eq!(runs(), 3);
    relay.json("/api/usage?x=1&refresh=1").await;
    assert_eq!(runs(), 4);
}

#[tokio::test]
async fn usage_without_the_services_is_503() {
    let relay = Relay::build(Setup {
        plan: false,
        tools: None,
        ..Setup::full()
    });
    for uri in [
        "/api/usage",
        "/api/usage?refresh=1",
        "/api/usage/tools",
        "/api/usage/tools?range=7d",
    ] {
        let response = relay.get(uri).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(text_of(response).await, UNAVAILABLE, "{uri}");
    }
    // The search still works.
    let (status, _) = relay.json("/api/search?q=beacon").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_relay_built_without_search_or_usage_answers_the_routes_plainly() {
    let test = TestDb::new();
    seed_search::seed(&test.conn(), test.root());
    let reads = Reads::new(test.db(), test.root());
    let probe = Arc::new(FakeProbe {
        runs: AtomicUsize::new(0),
    });
    let relay = Relay::over(test, tempfile::tempdir().unwrap(), probe, reads);

    let (status, body) = relay.json("/api/search?q=beacon").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body), ["srch-beacon"]);
    let response = relay.get("/api/usage").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(text_of(response).await, UNAVAILABLE);
}

// ------------------------------------------------------------------ tool usage

#[tokio::test]
async fn tool_usage_answers_the_snapshot_of_the_range() {
    let relay = Relay::with(one_tool_call);

    // `24h` is the default.
    for (uri, range) in [
        ("/api/usage/tools", "24h"),
        ("/api/usage/tools?range=24h", "24h"),
        ("/api/usage/tools?range=7d", "7d"),
        ("/api/usage/tools?range=30d", "30d"),
        ("/api/usage/tools?range=%37d", "7d"),
        ("/api/usage/tools?range=30d&refresh=1", "30d"),
        ("/api/usage/tools?refresh=1", "24h"),
    ] {
        let response = relay.get(uri).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json; charset=utf-8",
            "{uri}"
        );
        let body = json_of(response).await;
        assert_eq!(body["range"], json!(range), "{uri}");
        assert!(body["since"].as_str().unwrap().ends_with('Z'), "{uri}");
        assert!(body["until"].as_str().unwrap().ends_with('Z'), "{uri}");
        assert!(body["fetchedAt"].as_i64().unwrap() > 0, "{uri}");
        let provider = &body["providers"][0];
        assert_eq!(provider["provider"], json!("claude"), "{uri}");
        assert_eq!(provider["sessionCount"], json!(1), "{uri}");
        assert_eq!(provider["tools"][0]["name"], json!("Read"), "{uri}");
        assert_eq!(provider["tools"][0]["calls"], json!(1), "{uri}");
    }
}

#[tokio::test]
async fn tool_usage_refresh_scans_again_and_the_default_does_not() {
    let relay = Relay::with(one_tool_call);
    let (_, first) = relay.json("/api/usage/tools").await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (_, cached) = relay.json("/api/usage/tools").await;
    assert_eq!(cached, first);
    let (_, ignored) = relay.json("/api/usage/tools?refresh=0").await;
    assert_eq!(ignored, first);
    let (_, fresh) = relay.json("/api/usage/tools?refresh=1").await;
    assert!(fresh["fetchedAt"].as_i64() > first["fetchedAt"].as_i64());
}

#[tokio::test]
async fn an_invalid_tool_usage_range_is_400() {
    let relay = Relay::with(one_tool_call);
    for uri in [
        "/api/usage/tools?range=",
        "/api/usage/tools?range=1h",
        "/api/usage/tools?range=24H",
        "/api/usage/tools?range=7d%20",
        "/api/usage/tools?range=week",
        "/api/usage/tools?range=%ff",
        "/api/usage/tools?range=%zz",
        "/api/usage/tools?range=1h&range=7d",
        "/api/usage/tools?range=1h&refresh=1",
    ] {
        let response = relay.get(uri).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(text_of(response).await, BAD_RANGE, "{uri}");
    }
    // The first range counts, as the first of any parameter does.
    let (status, body) = relay.json("/api/usage/tools?range=7d&range=1h").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["range"], json!("7d"));
}

#[tokio::test]
async fn a_tool_usage_scan_that_takes_too_long_is_504() {
    let relay = Relay::build(Setup {
        tools: Some(Duration::from_nanos(1)),
        fill: one_tool_call,
        ..Setup::full()
    });
    for uri in ["/api/usage/tools", "/api/usage/tools?range=30d&refresh=1"] {
        let response = relay.get(uri).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT, "{uri}");
        assert_eq!(text_of(response).await, TOO_LONG, "{uri}");
    }
}

#[tokio::test]
async fn a_tool_usage_scan_that_fails_is_a_500_without_its_cause() {
    let test = TestDb::new();
    seed_search::seed(&test.conn(), test.root());
    let missing = test.dir().join("absent").join("conductor.db");
    let reads = Reads::new(test.db(), test.root()).with_usage(
        Arc::new(PlanUsageService::new(Arc::new(FakeProbe {
            runs: AtomicUsize::new(0),
        }))),
        Arc::new(ToolUsageService::new(missing)),
    );
    let probe = Arc::new(FakeProbe {
        runs: AtomicUsize::new(0),
    });
    let relay = Relay::over(test, tempfile::tempdir().unwrap(), probe, reads);

    let response = relay.get("/api/usage/tools").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(text_of(response).await, INTERNAL);
}

#[tokio::test]
async fn a_search_whose_database_cannot_be_read_is_a_500() {
    let test = TestDb::new();
    let missing = test.dir().join("absent").join("conductor.db");
    let reads = Reads::new(Arc::new(ConductorDb::new(missing)), test.root());
    let probe = Arc::new(FakeProbe {
        runs: AtomicUsize::new(0),
    });
    let relay = Relay::over(test, tempfile::tempdir().unwrap(), probe, reads);

    let response = relay.get("/api/search?q=beacon").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(text_of(response).await, INTERNAL);
    // No words, no read.
    let (status, body) = relay.json("/api/search").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"], json!([]));
}
