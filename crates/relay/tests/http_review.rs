//! The review routes over HTTP: the diff of a workspace and of one file, the list of its source
//! files, the preview of a source file and a local image. Success answers with their exact
//! headers, every error status and body, the token gate, the "Conductor is not running" answer and
//! revalidation.
//!
//! The diff routes run on a real repository registered as the worktree of a seeded workspace; the
//! previews run over temporary directories in both expose modes.

#[path = "support/seed_review.rs"]
#[allow(dead_code)]
mod seed_review;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Token};
use conductor_remote::db::ConductorDb;
use conductor_remote::files::{ExposeMode, PreviewRoots};
use conductor_remote::http::router;
use conductor_remote::reads::extras::commands::Commands;
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::Reads;
use conductor_remote::testing::{FakeCommands, FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "review-token";
const INTERNAL: &str = r#"{"error":"internal error"}"#;
const BINARY_CSP: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'";
const PUBLIC_REFUSAL: &str =
    "this relay is reachable from the internet, so it previews files inside Conductor workspaces only";
const TAILNET_REFUSAL: &str = "outside the files this relay may read";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n review image bytes";

// ------------------------------------------------------------------ git

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.org",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The id of `HEAD`.
fn head(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn write(dir: &Path, relative: &str, content: impl AsRef<[u8]>) {
    let target = dir.join(relative);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, content).unwrap();
}

/// The worktree of the seeded live workspace: a repository on `main` with one commit holding
/// `README.md`, `data.bin` and `tracked.rs`, then an uncommitted change to `tracked.rs` and two
/// untracked files.
fn make_worktree(test: &TestDb) -> (PathBuf, String) {
    let worktree = test.root().join(seed_review::REPO).join("rev-live-dir");
    std::fs::create_dir_all(&worktree).unwrap();
    git(&worktree, &["init", "-q", "-b", "main"]);
    write(&worktree, "README.md", "# readme\n");
    write(&worktree, "data.bin", b"\0binary");
    write(&worktree, "tracked.rs", "fn old() {}\n");
    git(&worktree, &["add", "-A"]);
    git(&worktree, &["commit", "-q", "-m", "initial"]);
    let commit = head(&worktree);
    write(&worktree, "tracked.rs", "fn new() {}\n");
    write(&worktree, "with space.rs", "one\ntwo\n");
    write(&worktree, "a+b.rs", "plus\n");
    (worktree, commit)
}

// ------------------------------------------------------------------ relay

/// A relay over one seeded database, a fake Conductor and temporary directories for the previews.
struct Relay {
    /// Keeps the database and the workspaces root alive.
    _test: TestDb,
    home: TempDir,
    /// The temporary directory local images may also come from.
    temp: TempDir,
    /// A directory no preview root includes.
    outside: TempDir,
    worktree: PathBuf,
    commit: String,
    conductor: Arc<FakeConductor>,
    app: Router,
}

impl Relay {
    /// A relay whose reads have preview settings in `mode` (none for `None`) and, when given,
    /// extras that run their commands through `commands`.
    fn build(mode: Option<ExposeMode>, commands: Option<Arc<dyn Commands>>) -> Self {
        let test = TestDb::new();
        let repo_root = test.root().join("_repos").join(seed_review::REPO);
        seed_review::seed(&test.conn(), repo_root.to_str().unwrap());
        let (worktree, commit) = make_worktree(&test);
        let db = test.db();
        Self::over(test, db, worktree, commit, mode, commands)
    }

    fn over(
        test: TestDb,
        db: Arc<ConductorDb>,
        worktree: PathBuf,
        commit: String,
        mode: Option<ExposeMode>,
        commands: Option<Arc<dyn Commands>>,
    ) -> Self {
        let home = TempDir::new().unwrap();
        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let mut reads = Reads::new(db, test.root());
        if let Some(mode) = mode {
            let roots = PreviewRoots::new(test.root(), home.path(), None, &[temp.path()]);
            reads = reads.with_preview(roots, mode);
        }
        if let Some(commands) = commands {
            reads = reads.with_extras(Arc::new(Extras::new(commands, home.path())));
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
            _test: test,
            home,
            temp,
            outside,
            worktree,
            commit,
            conductor,
            app,
        }
    }

    /// Previews in `mode`; git runs through `SystemCommands`.
    fn previewing(mode: ExposeMode) -> Self {
        Self::build(Some(mode), None)
    }

    /// No preview settings.
    fn plain() -> Self {
        Self::build(None, None)
    }

    /// A relay whose database file does not exist: any read of it fails.
    fn without_database() -> Self {
        let test = TestDb::new();
        let missing = test.dir().join("absent").join("conductor.db");
        Self::over(
            test,
            Arc::new(ConductorDb::new(missing)),
            PathBuf::new(),
            String::new(),
            Some(ExposeMode::Tailnet),
            None,
        )
    }

    async fn get(&self, uri: &str) -> Response {
        self.send(request(uri, Some(TOKEN), None)).await
    }

    async fn send(&self, request: Request<Body>) -> Response {
        self.app.clone().oneshot(request).await.unwrap()
    }

    /// A file in the home directory, and the URI that previews it.
    fn home_file(&self, relative: &str, content: impl AsRef<[u8]>) -> PathBuf {
        write(self.home.path(), relative, content);
        self.home.path().join(relative)
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

/// Percent-encodes everything but the unreserved characters, as one path segment.
fn enc(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn path_text(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A 200 JSON answer: the shared envelope's headers, and the parsed body.
async fn assert_json_ok(response: Response, what: &str) -> Value {
    assert_eq!(response.status(), StatusCode::OK, "{what}");
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        "application/json; charset=utf-8",
        "{what}"
    );
    assert_eq!(
        header_of(&response, header::CACHE_CONTROL),
        "no-cache",
        "{what}"
    );
    assert_eq!(
        header_of(&response, header::VARY),
        "accept-encoding",
        "{what}"
    );
    assert!(
        header_of(&response, header::ETAG).starts_with("W/\""),
        "{what}"
    );
    json_of(response).await
}

/// A JSON error answer: its status, the no-store header and its exact body.
async fn assert_error(response: Response, status: StatusCode, message: &str, what: &str) {
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
    assert_eq!(
        text_of(response).await,
        json!({ "error": message }).to_string(),
        "{what}"
    );
}

/// A repeat request with the ETag of the first answer is a 304 with no body.
async fn assert_revalidates(relay: &Relay, uri: &str) {
    let first = relay.get(uri).await;
    assert_eq!(first.status(), StatusCode::OK, "{uri}");
    let etag = header_of(&first, header::ETAG);
    assert!(etag.starts_with("W/\""), "{uri}: {etag}");
    let body = text_of(first).await;

    let conditional = relay.send(request(uri, Some(TOKEN), Some(&etag))).await;
    assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED, "{uri}");
    assert_eq!(header_of(&conditional, header::ETAG), etag, "{uri}");
    assert_eq!(
        header_of(&conditional, header::CACHE_CONTROL),
        "no-cache",
        "{uri}"
    );
    assert_eq!(text_of(conditional).await, "", "{uri}");

    let stale = relay
        .send(request(uri, Some(TOKEN), Some("W/\"other\"")))
        .await;
    assert_eq!(stale.status(), StatusCode::OK, "{uri}");
    assert_eq!(text_of(stale).await, body, "{uri}");
}

const LIVE: &str = seed_review::LIVE;

/// One URI of each new route.
fn every_route() -> Vec<String> {
    vec![
        format!("/api/workspaces/{LIVE}/diff"),
        format!("/api/workspaces/{LIVE}/diff/file?path=tracked.rs"),
        format!("/api/workspaces/{LIVE}/files"),
        format!("/api/files/{}", enc("/some/file.rs")),
        format!("/api/local-images/{}", enc("/some/image.png")),
    ]
}

// ------------------------------------------------------------------ token and Conductor status

#[tokio::test]
async fn every_route_needs_the_token() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    for uri in every_route() {
        let response = relay.send(request(&uri, None, None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            text_of(response).await,
            r#"{"error":"unauthorized"}"#,
            "{uri}"
        );
        let wrong = relay.send(request(&uri, Some("wrong-token"), None)).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

#[tokio::test]
async fn every_route_is_503_while_conductor_is_not_running() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    relay.conductor.set_status(ConductorStatus::NotRunning);
    for uri in every_route() {
        assert_error(
            relay.get(&uri).await,
            StatusCode::SERVICE_UNAVAILABLE,
            "Conductor is not running",
            &uri,
        )
        .await;
    }
}

#[tokio::test]
async fn a_failing_database_answers_500_without_naming_anything() {
    let relay = Relay::without_database();
    for uri in &every_route()[..3] {
        let response = relay.get(uri).await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{uri}"
        );
        assert_eq!(text_of(response).await, INTERNAL, "{uri}");
    }
}

#[tokio::test]
async fn the_workspace_route_keeps_its_answer() {
    let relay = Relay::plain();
    let response = relay.get(&format!("/api/workspaces/{LIVE}")).await;
    let body = assert_json_ok(response, "workspace").await;
    assert_eq!(body["workspace"]["id"], json!(LIVE));
}

// ------------------------------------------------------------------ the diff of a workspace

#[tokio::test]
async fn the_diff_of_a_workspace_lists_its_changes() {
    let relay = Relay::plain();
    let response = relay.get(&format!("/api/workspaces/{LIVE}/diff")).await;
    let body = assert_json_ok(response, "diff").await;

    let mut keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "base",
            "dirty",
            "files",
            "mergeBase",
            "patch",
            "truncated",
            "unpushed"
        ]
    );
    assert_eq!(body["base"], json!("main"));
    assert_eq!(body["mergeBase"], json!(relay.commit));
    assert_eq!(
        body["files"],
        json!([
            {"path": "tracked.rs", "added": 1, "removed": 1},
            {"path": "a+b.rs", "added": 1, "removed": 0},
            {"path": "with space.rs", "added": 2, "removed": 0},
        ])
    );
    assert_eq!(body["truncated"], json!(false));
    assert_eq!(body["dirty"], json!(true));
    assert_eq!(body["unpushed"], json!(false));
    let patch = body["patch"].as_str().unwrap();
    assert!(
        patch.contains("diff --git a/tracked.rs b/tracked.rs"),
        "{patch}"
    );
    assert!(patch.contains("-fn old() {}\n+fn new() {}\n"), "{patch}");
    assert!(patch.contains("+plus\n"), "{patch}");
    assert!(patch.contains("+one\n+two\n"), "{patch}");
}

#[tokio::test]
async fn the_diff_of_a_clean_worktree_is_empty() {
    let relay = Relay::plain();
    git(&relay.worktree, &["checkout", "-q", "--", "tracked.rs"]);
    std::fs::remove_file(relay.worktree.join("with space.rs")).unwrap();
    std::fs::remove_file(relay.worktree.join("a+b.rs")).unwrap();
    let response = relay.get(&format!("/api/workspaces/{LIVE}/diff")).await;
    let body = assert_json_ok(response, "diff").await;
    assert_eq!(
        body,
        json!({
            "base": "main",
            "mergeBase": relay.commit,
            "files": [],
            "patch": "",
            "truncated": false,
            "dirty": false,
            "unpushed": false,
        })
    );
}

#[tokio::test]
async fn the_diff_revalidates() {
    let relay = Relay::plain();
    assert_revalidates(&relay, &format!("/api/workspaces/{LIVE}/diff")).await;
}

#[tokio::test]
async fn the_diff_of_a_workspace_that_is_not_live_or_has_no_worktree_is_refused() {
    let relay = Relay::plain();
    for id in ["rev-unknown", seed_review::ARCHIVED] {
        assert_error(
            relay.get(&format!("/api/workspaces/{id}/diff")).await,
            StatusCode::NOT_FOUND,
            "workspace not found",
            id,
        )
        .await;
    }
    assert_error(
        relay
            .get(&format!(
                "/api/workspaces/{}/diff",
                seed_review::NO_WORKTREE
            ))
            .await,
        StatusCode::CONFLICT,
        "worktree path unresolved",
        "no worktree",
    )
    .await;
}

// ------------------------------------------------------------------ the diff of one file

#[tokio::test]
async fn the_diff_of_one_tracked_file_is_its_patch() {
    let relay = Relay::plain();
    let response = relay
        .get(&format!("/api/workspaces/{LIVE}/diff/file?path=tracked.rs"))
        .await;
    let body = assert_json_ok(response, "file diff").await;
    assert_eq!(body.as_object().unwrap().len(), 2);
    assert_eq!(body["path"], json!("tracked.rs"));
    let patch = body["patch"].as_str().unwrap();
    assert!(
        patch.starts_with("diff --git a/tracked.rs b/tracked.rs"),
        "{patch}"
    );
    assert!(patch.contains("-fn old() {}\n+fn new() {}\n"), "{patch}");
    assert!(!patch.contains("plus"), "{patch}");
}

#[tokio::test]
async fn the_path_parameter_is_decoded_as_a_form_value() {
    let relay = Relay::plain();
    // `+` is a space and `%20` too, and an escape may stand for any character; `%2B` is a plus sign.
    for query in ["with+space.rs", "with%20space.rs", "with%20space%2Ers"] {
        let response = relay
            .get(&format!("/api/workspaces/{LIVE}/diff/file?path={query}"))
            .await;
        let body = assert_json_ok(response, query).await;
        assert_eq!(body["path"], json!("with space.rs"), "{query}");
        assert!(body["patch"].as_str().unwrap().contains("+one\n+two\n"));
    }
    let plus = relay
        .get(&format!("/api/workspaces/{LIVE}/diff/file?path=a%2Bb.rs"))
        .await;
    let body = assert_json_ok(plus, "a%2Bb.rs").await;
    assert_eq!(body["path"], json!("a+b.rs"));
    // An unescaped plus is a space: `a b.rs` is no changed file.
    assert_error(
        relay
            .get(&format!("/api/workspaces/{LIVE}/diff/file?path=a+b.rs"))
            .await,
        StatusCode::NOT_FOUND,
        "changed file not found",
        "a+b.rs",
    )
    .await;
}

#[tokio::test]
async fn the_first_path_parameter_is_the_one_taken() {
    let relay = Relay::plain();
    let base = format!("/api/workspaces/{LIVE}/diff/file");
    let response = relay
        .get(&format!("{base}?other=1&path=tracked.rs&path=nothing"))
        .await;
    assert_eq!(
        assert_json_ok(response, "first").await["path"],
        "tracked.rs"
    );
    let response = relay.get(&format!("{base}?path=tracked.rs&path=")).await;
    assert_eq!(
        assert_json_ok(response, "first").await["path"],
        "tracked.rs"
    );
    assert_error(
        relay.get(&format!("{base}?path=&path=tracked.rs")).await,
        StatusCode::BAD_REQUEST,
        "file path is required",
        "empty first",
    )
    .await;
}

#[tokio::test]
async fn a_missing_empty_or_undecodable_path_is_a_400() {
    let relay = Relay::plain();
    let base = format!("/api/workspaces/{LIVE}/diff/file");
    for uri in [
        base.clone(),
        format!("{base}?"),
        format!("{base}?path"),
        format!("{base}?path="),
        format!("{base}?other=tracked.rs"),
        format!("{base}?path=%zz"),
        format!("{base}?path=%4"),
        format!("{base}?path=%"),
        format!("{base}?path=%ff"),
        format!("{base}?path=tracked%C3.rs"),
    ] {
        assert_error(
            relay.get(&uri).await,
            StatusCode::BAD_REQUEST,
            "file path is required",
            &uri,
        )
        .await;
    }
}

#[tokio::test]
async fn a_path_that_is_no_changed_file_is_a_404() {
    let relay = Relay::plain();
    let base = format!("/api/workspaces/{LIVE}/diff/file");
    for query in [
        "README.md",
        "missing.rs",
        "../tracked.rs",
        "%2Fetc%2Fpasswd",
        ".",
    ] {
        assert_error(
            relay.get(&format!("{base}?path={query}")).await,
            StatusCode::NOT_FOUND,
            "changed file not found",
            query,
        )
        .await;
    }
}

#[tokio::test]
async fn the_file_diff_checks_run_in_order() {
    let relay = Relay::plain();
    // The workspace first, even with no path.
    assert_error(
        relay.get("/api/workspaces/rev-unknown/diff/file").await,
        StatusCode::NOT_FOUND,
        "workspace not found",
        "unknown",
    )
    .await;
    assert_error(
        relay
            .get(&format!(
                "/api/workspaces/{}/diff/file?path=tracked.rs",
                seed_review::ARCHIVED
            ))
            .await,
        StatusCode::NOT_FOUND,
        "workspace not found",
        "archived",
    )
    .await;
    // Then the worktree, even with no path.
    let no_worktree = seed_review::NO_WORKTREE;
    for query in ["", "?path=tracked.rs"] {
        assert_error(
            relay
                .get(&format!("/api/workspaces/{no_worktree}/diff/file{query}"))
                .await,
            StatusCode::CONFLICT,
            "worktree path unresolved",
            query,
        )
        .await;
    }
    // Then the path, before any lookup of the file.
    assert_error(
        relay
            .get(&format!("/api/workspaces/{LIVE}/diff/file"))
            .await,
        StatusCode::BAD_REQUEST,
        "file path is required",
        "no path",
    )
    .await;
}

#[tokio::test]
async fn the_file_diff_revalidates() {
    let relay = Relay::plain();
    assert_revalidates(
        &relay,
        &format!("/api/workspaces/{LIVE}/diff/file?path=tracked.rs"),
    )
    .await;
}

// ------------------------------------------------------------------ the source files of a workspace

#[tokio::test]
async fn the_files_of_a_workspace_are_its_previewable_sources() {
    let relay = Relay::plain();
    let response = relay.get(&format!("/api/workspaces/{LIVE}/files")).await;
    let body = assert_json_ok(response, "files").await;
    assert_eq!(
        body,
        json!({
            "files": ["a+b.rs", "with space.rs", "README.md", "tracked.rs"],
            "truncated": false,
        })
    );
}

#[tokio::test]
async fn the_files_of_a_workspace_without_a_worktree_are_an_empty_list() {
    let relay = Relay::plain();
    let response = relay
        .get(&format!(
            "/api/workspaces/{}/files",
            seed_review::NO_WORKTREE
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        "application/json; charset=utf-8"
    );
    assert_eq!(text_of(response).await, r#"{"files":[],"truncated":false}"#);
}

#[tokio::test]
async fn the_files_of_a_workspace_that_is_not_live_are_a_404() {
    let relay = Relay::plain();
    for id in ["rev-unknown", seed_review::ARCHIVED] {
        assert_error(
            relay.get(&format!("/api/workspaces/{id}/files")).await,
            StatusCode::NOT_FOUND,
            "workspace not found",
            id,
        )
        .await;
    }
}

#[tokio::test]
async fn the_files_revalidate() {
    let relay = Relay::plain();
    assert_revalidates(&relay, &format!("/api/workspaces/{LIVE}/files")).await;
}

// ------------------------------------------------------------------ which commands run git

/// Every git call of the review routes goes through the extras' `Commands` when there are
/// extras, and through the real `git` when there are none.
#[tokio::test]
async fn the_review_routes_run_git_through_the_extras_commands() {
    let fake = Arc::new(FakeCommands::new());
    let relay = Relay::build(None, Some(fake.clone()));
    let worktree = path_text(&relay.worktree).to_owned();
    fake.on(
        "git",
        &["-C", &worktree, "ls-files", "--cached"],
        0,
        "scripted.rs\0scripted.bin\0",
    );

    let response = relay.get(&format!("/api/workspaces/{LIVE}/files")).await;
    let body = assert_json_ok(response, "files").await;
    assert_eq!(body, json!({"files": ["scripted.rs"], "truncated": false}));

    // A call the script does not know is a failed call: an empty diff, not the real one.
    let response = relay.get(&format!("/api/workspaces/{LIVE}/diff")).await;
    let body = assert_json_ok(response, "diff").await;
    assert_eq!(body["files"], json!([]));
    assert_eq!(body["patch"], json!(""));

    let calls = fake.calls();
    assert!(!calls.is_empty());
    assert!(
        calls
            .iter()
            .all(|call| call.program == "git" && call.args[..2] == ["-C", &worktree]),
        "{calls:?}"
    );

    // Without extras the same route reads the real repository.
    let plain = Relay::plain();
    let response = plain.get(&format!("/api/workspaces/{LIVE}/files")).await;
    let body = assert_json_ok(response, "files").await;
    assert_eq!(body["files"][0], json!("a+b.rs"));
}

// ------------------------------------------------------------------ the preview of a source file

fn file_uri(reference: &str) -> String {
    format!("/api/files/{}", enc(reference))
}

#[tokio::test]
async fn a_source_file_in_the_workspaces_is_previewed_in_both_modes() {
    for mode in [ExposeMode::Tailnet, ExposeMode::Public] {
        let relay = Relay::previewing(mode);
        let file = relay.worktree.join("tracked.rs");
        let response = relay.get(&file_uri(path_text(&file))).await;
        let body = assert_json_ok(response, "preview").await;
        assert_eq!(
            body,
            json!({
                "path": path_text(&file),
                "line": null,
                "lineStart": 1,
                "lineEnd": 2,
                "totalLines": 2,
                "content": "fn new() {}\n",
                "truncated": false,
            }),
            "{mode:?}"
        );
    }
}

#[tokio::test]
async fn a_location_suffix_and_a_home_reference_are_understood() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    let lines: String = (1..=300).map(|i| format!("line {i}\n")).collect();
    let file = relay.home_file("notes/long.md", &lines);

    let response = relay
        .get(&file_uri(&format!("{}:150:3", path_text(&file))))
        .await;
    let body = assert_json_ok(response, "located").await;
    assert_eq!(body["path"], json!(path_text(&file)));
    assert_eq!(body["line"], json!(150));
    assert_eq!(body["lineStart"], json!(50));
    assert_eq!(body["lineEnd"], json!(250));
    assert_eq!(body["totalLines"], json!(301));
    assert_eq!(body["truncated"], json!(true));
    assert!(body["content"].as_str().unwrap().starts_with("line 50\n"));

    let response = relay.get(&file_uri("~/notes/long.md")).await;
    let body = assert_json_ok(response, "home").await;
    assert_eq!(body["path"], json!(path_text(&file)));
    assert_eq!(body["line"], Value::Null);
    assert_eq!(body["lineEnd"], json!(301));
}

#[tokio::test]
async fn the_slash_of_a_reference_may_travel_unescaped_or_escaped() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    let file = relay.home_file("a.md", "text\n");
    let escaped = relay.get(&file_uri(path_text(&file))).await;
    assert_eq!(escaped.status(), StatusCode::OK);
    // A segment with a raw slash is no route.
    let raw = relay.get(&format!("/api/files/{}", path_text(&file))).await;
    assert_ne!(raw.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_home_directory_is_previewed_in_tailnet_mode_only() {
    let tailnet = Relay::previewing(ExposeMode::Tailnet);
    let file = tailnet.home_file("doc.md", "hello\n");
    assert_eq!(
        tailnet.get(&file_uri(path_text(&file))).await.status(),
        StatusCode::OK
    );

    let public = Relay::previewing(ExposeMode::Public);
    let file = public.home_file("doc.md", "hello\n");
    assert_error(
        public.get(&file_uri(path_text(&file))).await,
        StatusCode::FORBIDDEN,
        PUBLIC_REFUSAL,
        "public",
    )
    .await;
}

#[tokio::test]
async fn a_file_outside_every_root_is_refused_with_the_mode_s_message() {
    for (mode, message) in [
        (ExposeMode::Tailnet, TAILNET_REFUSAL),
        (ExposeMode::Public, PUBLIC_REFUSAL),
    ] {
        let relay = Relay::previewing(mode);
        write(relay.outside.path(), "secret.md", "hidden\n");
        let outside = relay.outside.path().join("secret.md");
        assert_error(
            relay.get(&file_uri(path_text(&outside))).await,
            StatusCode::FORBIDDEN,
            message,
            "outside",
        )
        .await;
        // A missing file outside answers the same: a refusal never shows whether it exists.
        let missing = relay.outside.path().join("missing.md");
        assert_error(
            relay.get(&file_uri(path_text(&missing))).await,
            StatusCode::FORBIDDEN,
            message,
            "outside, missing",
        )
        .await;
    }
}

#[tokio::test]
async fn a_source_file_that_is_not_there_is_a_404() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    let home = relay.home.path().to_owned();
    std::fs::create_dir(home.join("folder.md")).unwrap();
    write(&home, "binary.exe", "text\n");
    for reference in [
        path_text(&home.join("missing.md")).to_owned(),
        path_text(&home.join("folder.md")).to_owned(),
        path_text(&home.join("binary.exe")).to_owned(),
        "relative/path.md".to_owned(),
        format!("{}:0", path_text(&home.join("missing.md"))),
    ] {
        assert_error(
            relay.get(&file_uri(&reference)).await,
            StatusCode::NOT_FOUND,
            "source file not found",
            &reference,
        )
        .await;
    }
}

#[tokio::test]
async fn a_large_or_binary_source_file_is_413_or_415() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    let large = relay.home_file("large.txt", vec![b'a'; 512 * 1024 + 1]);
    assert_error(
        relay.get(&file_uri(path_text(&large))).await,
        StatusCode::PAYLOAD_TOO_LARGE,
        "source file is too large to preview",
        "large",
    )
    .await;

    let exact = relay.home_file("exact.txt", vec![b'a'; 512 * 1024]);
    assert_eq!(
        relay.get(&file_uri(path_text(&exact))).await.status(),
        StatusCode::OK
    );

    let with_nul = relay.home_file("nul.txt", b"abc\0def");
    assert_error(
        relay.get(&file_uri(path_text(&with_nul))).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "source file is not text",
        "nul",
    )
    .await;
    let invalid = relay.home_file("latin.txt", b"caf\xe9\n");
    assert_error(
        relay.get(&file_uri(path_text(&invalid))).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "source file is not text",
        "invalid utf-8",
    )
    .await;
}

#[tokio::test]
async fn without_preview_settings_a_source_file_is_a_404() {
    let relay = Relay::plain();
    let file = relay.worktree.join("tracked.rs");
    assert_error(
        relay.get(&file_uri(path_text(&file))).await,
        StatusCode::NOT_FOUND,
        "source file not found",
        "no settings",
    )
    .await;
}

#[tokio::test]
async fn a_file_preview_revalidates() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    let file = relay.worktree.join("tracked.rs");
    assert_revalidates(&relay, &file_uri(path_text(&file))).await;
}

// ------------------------------------------------------------------ local images

fn image_uri(path: &Path) -> String {
    format!("/api/local-images/{}", enc(path_text(path)))
}

async fn assert_image(response: Response, content_type: &str, what: &str) {
    assert_eq!(response.status(), StatusCode::OK, "{what}");
    assert_eq!(header_of(&response, header::CONTENT_TYPE), content_type);
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
    assert_eq!(
        header_of(&response, header::CONTENT_LENGTH),
        PNG.len().to_string()
    );
    assert_eq!(
        header_of(&response, header::X_CONTENT_TYPE_OPTIONS),
        "nosniff"
    );
    assert_eq!(
        header_of(&response, header::CONTENT_SECURITY_POLICY),
        BINARY_CSP
    );
    assert!(response.headers().get(header::ETAG).is_none(), "{what}");
    assert_eq!(bytes_of(response).await, PNG, "{what}");
}

#[tokio::test]
async fn a_local_image_is_served_from_the_workspaces_and_the_temporary_roots_in_both_modes() {
    for mode in [ExposeMode::Tailnet, ExposeMode::Public] {
        let relay = Relay::previewing(mode);
        write(&relay.worktree, "shots/one.png", PNG);
        write(relay.temp.path(), "two.PNG", PNG);
        assert_image(
            relay
                .get(&image_uri(&relay.worktree.join("shots/one.png")))
                .await,
            "image/png",
            "workspace",
        )
        .await;
        assert_image(
            relay
                .get(&image_uri(&relay.temp.path().join("two.PNG")))
                .await,
            "image/png",
            "temporary",
        )
        .await;
    }
}

#[tokio::test]
async fn a_local_image_has_the_content_type_of_its_extension() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    for (name, content_type) in [
        ("a.avif", "image/avif"),
        ("a.gif", "image/gif"),
        ("a.jpeg", "image/jpeg"),
        ("a.jpg", "image/jpeg"),
        ("a.webp", "image/webp"),
    ] {
        write(relay.temp.path(), name, PNG);
        assert_image(
            relay.get(&image_uri(&relay.temp.path().join(name))).await,
            content_type,
            name,
        )
        .await;
    }
}

#[tokio::test]
async fn the_home_directory_holds_local_images_in_tailnet_mode_only() {
    let tailnet = Relay::previewing(ExposeMode::Tailnet);
    let file = tailnet.home_file("pics/home.png", PNG);
    assert_image(tailnet.get(&image_uri(&file)).await, "image/png", "tailnet").await;
    // A home reference is expanded as well.
    assert_image(
        tailnet.get("/api/local-images/%7E%2Fpics%2Fhome.png").await,
        "image/png",
        "home reference",
    )
    .await;

    let public = Relay::previewing(ExposeMode::Public);
    let file = public.home_file("pics/home.png", PNG);
    assert_error(
        public.get(&image_uri(&file)).await,
        StatusCode::NOT_FOUND,
        "image not found",
        "public",
    )
    .await;
}

#[tokio::test]
async fn an_image_that_may_not_be_served_is_a_404() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    write(relay.outside.path(), "out.png", PNG);
    write(relay.temp.path(), "text.txt", PNG);
    std::fs::create_dir(relay.temp.path().join("dir.png")).unwrap();
    for path in [
        relay.outside.path().join("out.png"),
        relay.temp.path().join("missing.png"),
        relay.temp.path().join("text.txt"),
        relay.temp.path().join("dir.png"),
        PathBuf::from("relative.png"),
    ] {
        assert_error(
            relay.get(&image_uri(&path)).await,
            StatusCode::NOT_FOUND,
            "image not found",
            path_text(&path),
        )
        .await;
    }
}

#[tokio::test]
async fn a_large_image_is_a_413() {
    let relay = Relay::previewing(ExposeMode::Tailnet);
    let limit: u64 = 10 * 1024 * 1024;
    let large = relay.temp.path().join("large.png");
    std::fs::File::create(&large)
        .unwrap()
        .set_len(limit + 1)
        .unwrap();
    assert_error(
        relay.get(&image_uri(&large)).await,
        StatusCode::PAYLOAD_TOO_LARGE,
        "image is too large",
        "large",
    )
    .await;

    let exact = relay.temp.path().join("exact.png");
    std::fs::File::create(&exact)
        .unwrap()
        .set_len(limit)
        .unwrap();
    let response = relay.get(&image_uri(&exact)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes_of(response).await.len() as u64, limit);
}

#[tokio::test]
async fn without_preview_settings_a_local_image_is_a_404() {
    let relay = Relay::plain();
    write(relay.temp.path(), "a.png", PNG);
    assert_error(
        relay
            .get(&image_uri(&relay.temp.path().join("a.png")))
            .await,
        StatusCode::NOT_FOUND,
        "image not found",
        "no settings",
    )
    .await;
}
