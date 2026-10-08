//! The routes for images, the context breakdown, the model files and the background facts, over
//! HTTP: success answers with their exact headers, the 404s, the token gate, the "Conductor is not
//! running" answers and revalidation.
//!
//! Each seed gets a database of its own, as the seeds were written independently.

#[allow(dead_code)]
#[path = "support/seed_context.rs"]
mod seed_context;
#[allow(dead_code)]
#[path = "support/seed_extras.rs"]
mod seed_extras;
#[allow(dead_code)]
#[path = "support/seed_images.rs"]
mod seed_images;
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use conductor_remote::contract::{AppState, ConductorStatus, StateResponse, Token};
use conductor_remote::db::ConductorDb;
use conductor_remote::http::router;
use conductor_remote::reads::extras::commands::Commands;
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::{HostPaths, Reads};
use conductor_remote::testing::{FakeCommands, FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "extras-token";
const HOME: &str = "/Users/tester";
const NOT_RUNNING: &str = r#"{"error":"Conductor is not running"}"#;
const INTERNAL: &str = r#"{"error":"internal error"}"#;
const BINARY_CSP: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'";

/// A model catalogue file with every kind of entry the reader has to handle.
const MODEL_CACHE: &str = r#"[
  { "agentType": " claude ", "models": ["Fable 5.1 NEW", "5.6 Sol", "5.6 Terra ", "5.6 Sol", 7],
    "defaultModel": "5.6 Sol NEW", "snapshotAt": 1760000000000,
    "snapshotModels": ["Fable 5.1", "5.6 Sol NEW", null],
    "selections": [ { "model": "5.6 Sol NEW", "selectedAt": 1760000000500 },
                    { "model": "x", "selectedAt": "bad" }, { "model": "  ", "selectedAt": 1 }, 3 ],
    "updatedAt": 1760000001000 },
  { "agentType": "codex", "models": ["5.6 Terra"], "defaultModel": "5.6 Terra",
    "snapshotAt": "soon", "updatedAt": 1760000002000 },
  { "agentType": "  ", "models": ["opencode-go/muse-spark"] },
  { "agentType": "empty", "models": [] },
  { "agentType": 5, "models": ["a"] },
  { "models": ["a"] },
  "junk"
]"#;

const MODEL_SETTINGS: &str = "[models.claude_code]\ndefault_effort_level = \"max\"\n[models.codex]\ndefault_thinking_level = \"xhigh\"\n";

/// A relay over one synthetic database, a fake Conductor and temporary home and state
/// directories.
struct Relay {
    test: TestDb,
    home: TempDir,
    state: TempDir,
    conductor: Arc<FakeConductor>,
    app: Router,
}

impl Relay {
    fn with(seed: impl FnOnce(&TestDb), extras: Option<Arc<Extras>>) -> Self {
        let test = TestDb::new();
        seed(&test);
        let db = test.db();
        Self::over(test, db, extras)
    }

    fn over(test: TestDb, db: Arc<ConductorDb>, extras: Option<Arc<Extras>>) -> Self {
        let home = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let mut reads = Reads::new(db, test.root()).with_host_paths(HostPaths {
            home: home.path().to_path_buf(),
            state_dir: state.path().to_path_buf(),
        });
        if let Some(extras) = extras {
            reads = reads.with_extras(extras);
        }
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
            test,
            home,
            state,
            conductor,
            app,
        }
    }

    fn images() -> Self {
        Self::with(|test| seed_images::seed(&test.conn()), None)
    }

    fn context() -> Self {
        Self::with(|test| seed_context::seed(&test.conn()), None)
    }

    /// A relay whose database file does not exist: any read of it fails.
    fn without_database() -> Self {
        let test = TestDb::new();
        let missing = test.dir().join("absent").join("conductor.db");
        Self::over(test, Arc::new(ConductorDb::new(missing)), None)
    }

    async fn get(&self, uri: &str) -> Response {
        self.send(request(uri, Some(TOKEN), None)).await
    }

    async fn send(&self, request: Request<Body>) -> Response {
        self.app.clone().oneshot(request).await.unwrap()
    }

    fn write_model_cache(&self, content: &str) {
        std::fs::write(self.state.path().join("model-cache.json"), content).unwrap();
    }

    fn settings_file(&self) -> PathBuf {
        self.home.path().join(".conductor").join("settings.toml")
    }

    fn write_settings(&self, content: &str) {
        let path = self.settings_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Gives a repository a root in a directory of its own holding the given files.
    fn add_repo(&self, name: &str, files: &[(&str, &[u8])]) {
        let root = self
            .test
            .dir()
            .join(format!("root-of-{}", name.replace(' ', "-")));
        for (relative, bytes) in files {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        self.test
            .conn()
            .execute(
                "INSERT INTO repos (id, name, root_path) VALUES (?1, ?2, ?3)",
                rusqlite::params![format!("img-id-{name}"), name, root.to_str().unwrap()],
            )
            .unwrap();
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

async fn bytes_of(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn text_of(response: Response) -> String {
    String::from_utf8(bytes_of(response).await).unwrap()
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

fn golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/tests/contract/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
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

/// A 200 JSON answer carries the shared envelope's headers, and a repeat request with its etag is
/// a 304 with no body and the same etag.
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

    let conditional = relay.send(request(uri, Some(TOKEN), Some(&etag))).await;
    assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED, "{uri}");
    assert_eq!(header_of(&conditional, header::ETAG), etag, "{uri}");
    assert_eq!(text_of(conditional).await, "", "{uri}");

    let stale = relay
        .send(request(uri, Some(TOKEN), Some("W/\"other\"")))
        .await;
    assert_eq!(stale.status(), StatusCode::OK, "{uri}");
}

/// A JSON error answer: its status, the no-store header and its exact body.
async fn assert_error(response: Response, status: StatusCode, body: &str, what: &str) {
    assert_eq!(response.status(), status, "{what}");
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        "application/json; charset=utf-8",
        "{what}"
    );
    assert_eq!(
        header_of(&response, header::CACHE_CONTROL),
        "no-store",
        "{what}"
    );
    assert_eq!(text_of(response).await, body, "{what}");
}

const NEW_ROUTES: [&str; 5] = [
    "/api/repos/anything/icon",
    "/api/tool-images/1.0",
    "/api/sessions/anything/context",
    "/api/models",
    "/api/models/defaults",
];

// ------------------------------------------------------------------ token and Conductor status

#[tokio::test]
async fn every_new_route_needs_the_token() {
    let relay = Relay::images();
    for uri in NEW_ROUTES {
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

#[tokio::test]
async fn every_new_route_is_503_while_conductor_is_not_running() {
    // The database file does not exist: a route that touched it would answer 500.
    let relay = Relay::without_database();
    relay.conductor.set_status(ConductorStatus::NotRunning);
    for uri in NEW_ROUTES {
        assert_error(
            relay.get(uri).await,
            StatusCode::SERVICE_UNAVAILABLE,
            NOT_RUNNING,
            uri,
        )
        .await;
    }
}

// ------------------------------------------------------------------ repository icon

#[tokio::test]
async fn a_repository_icon_is_served_with_its_content_type_and_cache_header() {
    let relay = Relay::images();
    let icon = b"\x89PNG icon bytes".as_slice();
    relay.add_repo("img-project", &[("public/apple-touch-icon.png", icon)]);
    relay.add_repo("img-vector", &[("favicon.svg", b"<svg/>")]);
    relay.add_repo("img-legacy", &[("favicon.ico", b"ico")]);

    let response = relay.get("/api/repos/img-project/icon").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, header::CONTENT_TYPE), "image/png");
    assert_eq!(
        header_of(&response, header::CACHE_CONTROL),
        "public, max-age=300"
    );
    assert_eq!(
        header_of(&response, header::CONTENT_LENGTH),
        icon.len().to_string()
    );
    assert_eq!(
        header_of(&response, header::X_CONTENT_TYPE_OPTIONS),
        "nosniff"
    );
    assert_eq!(
        header_of(&response, header::CONTENT_SECURITY_POLICY),
        BINARY_CSP
    );
    assert_eq!(bytes_of(response).await, icon);

    let svg = relay.get("/api/repos/img-vector/icon").await;
    assert_eq!(header_of(&svg, header::CONTENT_TYPE), "image/svg+xml");
    assert_eq!(bytes_of(svg).await, b"<svg/>");
    let ico = relay.get("/api/repos/img-legacy/icon").await;
    assert_eq!(header_of(&ico, header::CONTENT_TYPE), "image/x-icon");
}

#[tokio::test]
async fn the_repository_name_is_decoded() {
    let relay = Relay::images();
    relay.add_repo("img my project", &[("favicon.png", b"png")]);
    let response = relay.get("/api/repos/img%20my%20project/icon").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await, b"png");
}

#[tokio::test]
async fn a_repository_without_an_icon_is_404() {
    let relay = Relay::images();
    relay.add_repo("img-bare", &[("README.md", b"no icon here")]);
    for name in [
        "img-bare",
        "img-nobody",
        seed_images::REPO_NO_ROOT,
        seed_images::REPO_EMPTY_ROOT,
    ] {
        assert_error(
            relay.get(&format!("/api/repos/{name}/icon")).await,
            StatusCode::NOT_FOUND,
            r#"{"error":"no icon"}"#,
            name,
        )
        .await;
    }
}

// ------------------------------------------------------------------ tool images

#[tokio::test]
async fn a_tool_image_is_served_with_its_media_type_and_cache_header() {
    let relay = Relay::images();
    for (reference, media_type, base64) in [
        ("1.0", "image/png", seed_images::PNG),
        ("2.1", "image/gif", seed_images::GIF),
        ("2.2", "image/jpeg", seed_images::JPEG),
        ("3.3", "image/webp", seed_images::WEBP),
        ("3.4", "application/octet-stream", seed_images::OTHER),
    ] {
        let response = relay.get(&format!("/api/tool-images/{reference}")).await;
        assert_eq!(response.status(), StatusCode::OK, "{reference}");
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            media_type,
            "{reference}"
        );
        assert_eq!(
            header_of(&response, header::CACHE_CONTROL),
            "private, max-age=86400, immutable",
            "{reference}"
        );
        assert_eq!(
            header_of(&response, header::X_CONTENT_TYPE_OPTIONS),
            "nosniff",
            "{reference}"
        );
        assert_eq!(
            header_of(&response, header::CONTENT_SECURITY_POLICY),
            BINARY_CSP,
            "{reference}"
        );
        let expected = STANDARD.decode(base64).unwrap();
        assert_eq!(
            header_of(&response, header::CONTENT_LENGTH),
            expected.len().to_string(),
            "{reference}"
        );
        assert_eq!(bytes_of(response).await, expected, "{reference}");
    }
}

#[tokio::test]
async fn a_missing_tool_image_is_404() {
    let relay = Relay::images();
    // Past the last image of a row, a row without images, a row that does not exist, and
    // references that are not of the form `<rowid>.<index>`.
    for reference in ["1.1", "7.0", "5.0", "99.0", "abc", "1", "1.x", "-1.0", ".0"] {
        assert_error(
            relay.get(&format!("/api/tool-images/{reference}")).await,
            StatusCode::NOT_FOUND,
            r#"{"error":"image not found"}"#,
            reference,
        )
        .await;
    }
}

// ------------------------------------------------------------------ context

#[tokio::test]
async fn the_context_breakdown_equals_the_golden_file() {
    let relay = Relay::context();
    let response = relay
        .get(&format!("/api/sessions/{}/context", seed_context::CHAT))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        "application/json; charset=utf-8"
    );
    assert_eq!(json_of(response).await, golden("context-breakdown.json"));
}

#[tokio::test]
async fn the_context_route_revalidates() {
    let relay = Relay::context();
    assert_revalidates(
        &relay,
        &format!("/api/sessions/{}/context", seed_context::CHAT),
    )
    .await;
}

#[tokio::test]
async fn an_unknown_or_closed_chat_has_no_context() {
    let relay = Relay::context();
    for chat in ["ctx-nobody", seed_context::CLOSED] {
        assert_error(
            relay.get(&format!("/api/sessions/{chat}/context")).await,
            StatusCode::NOT_FOUND,
            r#"{"error":"chat not found"}"#,
            chat,
        )
        .await;
    }
}

// ------------------------------------------------------------------ models

#[tokio::test]
async fn the_catalogue_equals_the_golden_file() {
    let relay = Relay::images();
    relay.write_model_cache(MODEL_CACHE);
    let response = relay.get("/api/models").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_of(response).await, golden("models-catalog.json"));
}

#[tokio::test]
async fn without_a_catalogue_file_the_catalogue_is_empty() {
    let relay = Relay::images();
    let response = relay.get("/api/models").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_of(response).await, json!({"groups": []}));
}

#[tokio::test]
async fn the_default_efforts_equal_the_golden_file() {
    let relay = Relay::images();
    relay.write_settings(MODEL_SETTINGS);
    let response = relay.get("/api/models/defaults").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_of(response).await, golden("models-defaults.json"));
}

#[tokio::test]
async fn without_a_settings_file_both_efforts_are_null() {
    let relay = Relay::images();
    let response = relay.get("/api/models/defaults").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_of(response).await,
        json!({"defaultEfforts": {"claude": null, "codex": null}})
    );
}

#[tokio::test]
async fn the_model_routes_revalidate() {
    let relay = Relay::images();
    relay.write_model_cache(MODEL_CACHE);
    relay.write_settings(MODEL_SETTINGS);
    assert_revalidates(&relay, "/api/models").await;
    assert_revalidates(&relay, "/api/models/defaults").await;
}

#[tokio::test]
async fn an_unreadable_settings_file_is_a_500_without_a_path() {
    let relay = Relay::images();
    // A directory where the file should be.
    std::fs::create_dir_all(relay.settings_file()).unwrap();
    let response = relay.get("/api/models/defaults").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
    let text = text_of(response).await;
    assert_eq!(text, INTERNAL);
    assert!(!text.contains(relay.home.path().to_str().unwrap()));
}

// ------------------------------------------------------------------ background facts

/// A relay over `seed_extras` whose commands are scripted so that every background refresh
/// finds something. The worktree of `ext-workspace` is a directory with a `.git` entry.
fn extras_relay() -> (Relay, Arc<Extras>) {
    let test = TestDb::new();
    seed_extras::seed(&test.conn());
    let worktree = test
        .root()
        .join(seed_extras::REPO_NAME)
        .join(seed_extras::DIRECTORY);
    std::fs::create_dir_all(worktree.join(".git")).expect("worktree directory");
    let wt = worktree.to_string_lossy().into_owned();

    let fake = Arc::new(FakeCommands::new());
    fake.on("git", &["-C", &wt, "rev-parse"], 0, "");
    fake.on("git", &["-C", &wt, "merge-base"], 0, "abc123\n");
    fake.on(
        "git",
        &["-C", &wt, "diff", "--numstat"],
        0,
        "10\t2\tsrc/a.rs\n3\t1\tb.rs\n",
    );
    fake.on("git", &["-C", &wt, "ls-files"], 0, "");
    let pull_requests = json!([
        pull_request(seed_extras::BRANCH_DRAFT, 11, "OPEN", true),
        pull_request(seed_extras::BRANCH_MERGED, 12, "MERGED", false),
    ]);
    fake.on("gh", &["pr", "list"], 0, &pull_requests.to_string());
    let run_key = wt.replace('/', "--");
    fake.on(
        "ps",
        &["-axww"],
        0,
        &format!("/sbin/launchd\nzsh {HOME}/.conductor/projects/{run_key}/run-run:1.sh\n"),
    );
    fake.on(
        "ps",
        &["-axo"],
        0,
        &format!("100 10:00 claude --resume={}\n", seed_extras::LIVE_CHAT),
    );
    let commands: Arc<dyn Commands> = fake;
    let extras = Arc::new(Extras::new(commands, HOME));
    let db = test.db();
    (Relay::over(test, db, Some(extras.clone())), extras)
}

fn pull_request(branch: &str, number: i64, state: &str, draft: bool) -> Value {
    json!({
        "headRefName": branch,
        "number": number,
        "url": format!("https://example.test/pull/{number}"),
        "state": state,
        "isDraft": draft,
        "updatedAt": "2026-03-01T00:00:00Z",
        "statusCheckRollup": [],
    })
}

/// What `/api/state` answers around the workspaces of a running Conductor.
fn state_with(workspaces: Value) -> Value {
    let mut state = serde_json::to_value(StateResponse::skeleton(ConductorStatus::Running))
        .expect("the skeleton serialises");
    state["workspaces"] = workspaces;
    state
}

#[tokio::test]
async fn the_state_follows_the_background_work_and_equals_the_golden_file() {
    let (relay, extras) = extras_relay();

    // The first answer is built before any background job has finished. Only the first
    // workspace is looked at: the second one of the same repository may already see the pull
    // requests that the first one's lookup fetched.
    let first = json_of(relay.get("/api/state").await).await;
    let workspace = first["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == seed_extras::WORKSPACE)
        .expect("the workspace with a worktree");
    assert_eq!(workspace["change_stats"], Value::Null);
    assert_eq!(workspace["pr_status"], Value::Null);
    assert_eq!(workspace["run_active"], json!(false));

    extras.wait_idle();
    let response = relay.get("/api/state").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        without_root(json_of(response).await, relay.test.root()),
        state_with(golden("workspaces-extras.json"))
    );
}

#[tokio::test]
async fn the_state_route_with_extras_revalidates() {
    let (relay, extras) = extras_relay();
    relay.get("/api/state").await;
    extras.wait_idle();
    assert_revalidates(&relay, "/api/state").await;
}
