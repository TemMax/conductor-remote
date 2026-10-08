#[allow(dead_code)]
#[path = "support/seed_images.rs"]
mod seed_images;
#[allow(dead_code)]
#[path = "support/seed_review.rs"]
mod seed_review;
#[path = "support/seed_search.rs"]
mod seed_search;
#[path = "support/seed_workspaces.rs"]
mod seed_workspaces;
mod support;

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use conductor_remote::app;
use conductor_remote::contract::{AppState, ConductorControl, ConductorStatus, Token};
use conductor_remote::files::{ExposeMode, PreviewRoots};
use conductor_remote::reads::extras::commands::{
    CommandError, Commands, Limits, Output, SystemCommands,
};
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::Reads;
use conductor_remote::search::index::{spawn_indexer, SearchIndex};
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const TOKEN: &str = "e2e-token";
const INDEX: &str = "<html>e2e app</html>";

struct Relay {
    base: String,
    client: reqwest::Client,
    conductor: Arc<FakeConductor>,
    stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Relay {
    async fn start() -> Self {
        Self::start_with(None).await
    }

    async fn start_with(reads: Option<Arc<Reads>>) -> Self {
        let conductor = Arc::new(FakeConductor::new(ConductorStatus::Running));
        let assets = MemoryAssets::default().with(
            "index.html",
            "text/html; charset=utf-8",
            INDEX.as_bytes(),
        );
        let state = AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: conductor.clone(),
            assets: Arc::new(assets),
            reads,
            writes: None,
            notify: None,
            services: Default::default(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(app::serve(listener, state, async move {
            let _ = stopped.await;
        }));
        Self {
            base,
            client: reqwest::Client::new(),
            conductor,
            stop,
            server,
        }
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client.get(format!("{}{path}", self.base))
    }

    async fn running(&self) -> bool {
        let response = self
            .get("/api/state")
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        body["conductor"]["running"].as_bool().unwrap()
    }
}

#[tokio::test]
async fn state_requires_the_token() {
    let relay = Relay::start().await;
    let response = relay.get("/api/state").send().await.unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn state_follows_the_conductor_status() {
    let relay = Relay::start().await;
    assert!(relay.running().await);
    relay.conductor.set_status(ConductorStatus::NotRunning);
    assert!(!relay.running().await);
}

#[tokio::test]
async fn launch_starts_conductor() {
    let relay = Relay::start().await;
    relay.conductor.set_status(ConductorStatus::NotRunning);
    assert!(!relay.running().await);

    let response = relay
        .client
        .post(format!("{}/api/conductor/launch", relay.base))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), json!({"ok": true}));
    assert!(relay.running().await);
}

#[tokio::test]
async fn pages_are_served_without_a_token() {
    let relay = Relay::start().await;
    for path in ["/", "/some/page"] {
        let response = relay.get(path).send().await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.text().await.unwrap(), INDEX, "{path}");
    }
}

#[tokio::test]
async fn shutdown_stops_the_server() {
    let relay = Relay::start().await;
    assert!(relay.running().await);
    relay.stop.send(()).unwrap();
    let result = relay.server.await.unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn state_carries_the_workspaces_of_the_database() {
    let test = support::TestDb::new();
    seed_workspaces::seed(&test.conn(), test.root());
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    let relay = Relay::start_with(Some(reads)).await;

    let response = relay
        .get("/api/state")
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/json; charset=utf-8"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["conductor"]["running"], json!(true));
    let ids: Vec<&str> = body["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "ws-pinned",
            "ws-iso",
            "ws-space",
            "ws-unread",
            "ws-setting-up"
        ]
    );
}

/// Runs `git` for real and refuses every other program, so neither `gh` nor `ps` runs.
struct GitOnly;

impl Commands for GitOnly {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        if program == "git" {
            SystemCommands.run(program, args, cwd, limits)
        } else {
            Err(CommandError::Spawn {
                program: program.to_owned(),
            })
        }
    }
}

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

async fn get_json(relay: &Relay, path: &str) -> Value {
    let response = relay.get(path).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(response.status(), 200, "{path}");
    response.json().await.unwrap()
}

#[tokio::test]
async fn background_facts_icons_and_tool_images_travel_through_a_listener() {
    let test = support::TestDb::new();
    seed_images::seed(&test.conn());

    // A repository whose icon file is in its root, and a workspace whose worktree is a real
    // repository holding one uncommitted line.
    let icon = b"e2e icon bytes";
    let icon_root = test.dir().join("e2e-icon-root");
    std::fs::create_dir(&icon_root).unwrap();
    std::fs::write(icon_root.join("favicon.png"), icon).unwrap();
    let worktree = test.root().join("e2e-repo").join("e2e-dir");
    std::fs::create_dir_all(&worktree).unwrap();
    git(&worktree, &["init", "-q", "-b", "main"]);
    std::fs::write(worktree.join("notes.txt"), "one\ntwo\n").unwrap();
    git(&worktree, &["add", "notes.txt"]);
    git(&worktree, &["commit", "-q", "-m", "init"]);
    std::fs::write(worktree.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
    {
        let conn = test.conn();
        conn.execute(
            "INSERT INTO repos (id, name, root_path, default_branch) \
             VALUES ('e2e-repo-id', 'e2e-repo', ?1, 'main')",
            [icon_root.to_str().unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, state, \
             created_at, updated_at, workspace_name) \
             VALUES ('e2e-workspace', 'e2e-workspace', 'e2e-repo-id', 'e2e-dir', 'e2e-branch', \
             'ready', '2026-03-01 08:00:00', '2026-03-01 10:00:00', 'E2E')",
            [],
        )
        .unwrap();
    }

    let extras = Arc::new(Extras::new(Arc::new(GitOnly), "/e2e-home"));
    let reads = Reads::new(test.db(), test.root()).with_extras(extras.clone());
    let relay = Relay::start_with(Some(Arc::new(reads))).await;

    let workspace_of = |state: &Value| {
        state["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == "e2e-workspace")
            .cloned()
            .expect("the workspace is listed")
    };

    // Nothing has been computed yet when the first answer is built.
    let first = workspace_of(&get_json(&relay, "/api/state").await);
    assert_eq!(first["change_stats"], Value::Null);

    tokio::task::spawn_blocking(move || extras.wait_idle())
        .await
        .unwrap();
    let next = workspace_of(&get_json(&relay, "/api/state").await);
    assert_eq!(next["change_stats"], json!({"added": 1, "removed": 0}));
    assert_eq!(next["pr_status"], Value::Null);
    assert_eq!(next["pr_number"], Value::Null);
    assert_eq!(next["pr_url"], Value::Null);
    assert_eq!(next["run_active"], json!(false));

    let response = relay
        .get("/api/repos/e2e-repo/icon")
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.headers()["cache-control"], "public, max-age=300");
    assert_eq!(response.bytes().await.unwrap().as_ref(), icon);

    let messages = get_json(
        &relay,
        &format!("/api/sessions/{}/messages", seed_images::CHAT),
    )
    .await;
    let reference = messages["entries"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|entry| entry["images"].as_array().into_iter().flatten())
        .map(|reference| reference.as_str().unwrap().to_owned())
        .next()
        .expect("a message carries an image reference");
    let response = relay
        .get(&format!("/api/tool-images/{reference}"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(
        response.headers()["cache-control"],
        "private, max-age=86400, immutable"
    );
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        STANDARD.decode(seed_images::PNG).unwrap()
    );
}

/// Percent-encodes everything but the unreserved characters, as one path segment.
fn encode_segment(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn the_review_routes_travel_through_a_listener() {
    let test = support::TestDb::new();
    let repo_root = test.root().join("_repos").join(seed_review::REPO);
    seed_review::seed(&test.conn(), repo_root.to_str().unwrap());

    // The live workspace's worktree: a real repository with one committed file, one edited and
    // one new.
    let worktree = test.root().join(seed_review::REPO).join("rev-live-dir");
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    git(&worktree, &["init", "-q", "-b", "main"]);
    std::fs::write(worktree.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
    std::fs::write(worktree.join("data.bin"), b"\0").unwrap();
    git(&worktree, &["add", "-A"]);
    git(&worktree, &["commit", "-q", "-m", "init"]);
    let merge_base = {
        let out = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    std::fs::write(worktree.join("src/lib.rs"), "pub fn two() {}\n").unwrap();
    std::fs::write(worktree.join("notes.md"), "# notes\n").unwrap();

    // A local image under a temporary root of its own.
    let home = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    let png = b"\x89PNG e2e image bytes";
    std::fs::write(images.path().join("shot.png"), png).unwrap();

    let roots = PreviewRoots::new(test.root(), home.path(), None, &[images.path()]);
    let extras = Arc::new(Extras::new(Arc::new(GitOnly), home.path()));
    let reads = Reads::new(test.db(), test.root())
        .with_extras(extras)
        .with_preview(roots, ExposeMode::Public);
    let relay = Relay::start_with(Some(Arc::new(reads))).await;

    let diff = get_json(&relay, "/api/workspaces/rev-live/diff").await;
    assert_eq!(diff["base"], json!("main"));
    assert_eq!(diff["mergeBase"], json!(merge_base));
    assert_eq!(
        diff["files"],
        json!([
            {"path": "src/lib.rs", "added": 1, "removed": 1},
            {"path": "notes.md", "added": 1, "removed": 0},
        ])
    );
    assert_eq!(diff["dirty"], json!(true));
    assert_eq!(diff["truncated"], json!(false));
    let patch = diff["patch"].as_str().unwrap();
    assert!(
        patch.contains("-pub fn one() {}\n+pub fn two() {}\n"),
        "{patch}"
    );

    let one = get_json(
        &relay,
        "/api/workspaces/rev-live/diff/file?path=src%2Flib.rs",
    )
    .await;
    assert_eq!(one["path"], json!("src/lib.rs"));
    assert!(one["patch"]
        .as_str()
        .unwrap()
        .starts_with("diff --git a/src/lib.rs b/src/lib.rs"));

    let files = get_json(&relay, "/api/workspaces/rev-live/files").await;
    assert_eq!(
        files,
        json!({"files": ["notes.md", "src/lib.rs"], "truncated": false})
    );

    let source = worktree.join("src/lib.rs");
    let source = source.to_str().unwrap();
    let preview = get_json(&relay, &format!("/api/files/{}", encode_segment(source))).await;
    assert_eq!(
        preview,
        json!({
            "path": source,
            "line": null,
            "lineStart": 1,
            "lineEnd": 2,
            "totalLines": 2,
            "content": "pub fn two() {}\n",
            "truncated": false,
        })
    );

    let image = images.path().join("shot.png");
    let response = relay
        .get(&format!(
            "/api/local-images/{}",
            encode_segment(image.to_str().unwrap())
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.bytes().await.unwrap().as_ref(), png);

    // The token gate holds on the real listener too.
    let response = relay
        .get("/api/workspaces/rev-live/diff")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_search_finds_the_text_of_a_chat_once_the_indexer_ran() {
    let test = support::TestDb::new();
    {
        let conn = test.conn();
        seed_search::seed(&conn, test.root());
        seed_search::say(
            &conn,
            "e2e-search-1",
            "srch-chat-live",
            "2026-09-10T10:00:00.000Z",
            "the zeppelin hangar needs a new door",
        );
    }
    let index_dir = tempfile::tempdir().unwrap();
    let index = Arc::new(SearchIndex::open(&index_dir.path().join("search.db")).unwrap());
    let reads = Reads::new(test.db(), test.root()).with_search(index.clone());
    let relay = Relay::start_with(Some(Arc::new(reads))).await;

    // The indexer has not run: the index is empty and the words of the chat are not found.
    let before = get_json(&relay, "/api/search?q=zeppelin").await;
    assert_eq!(before["results"], json!([]));
    assert_eq!(before["index"]["ready"], json!(false));

    // As `main` starts it: over the database file, running while Conductor does.
    let stop = Arc::new(AtomicBool::new(false));
    let conductor = relay.conductor.clone();
    let indexer = spawn_indexer(
        index,
        test.path().to_path_buf(),
        move || conductor.status().is_running(),
        stop.clone(),
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let found = loop {
        let body = get_json(&relay, "/api/search?q=zeppelin").await;
        if body["index"]["ready"] == json!(true) && !body["results"].as_array().unwrap().is_empty()
        {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "the indexer did not catch up: {body}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(found["query"], json!("zeppelin"));
    assert_eq!(found["index"]["chunks"], json!(1));
    let results = found["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["workspace"]["id"], json!("srch-live"));
    assert_eq!(results[0]["sessionId"], json!("srch-chat-live"));
    assert_eq!(results[0]["byName"], json!(false));
    let snippet = results[0]["snippets"][0]["text"].as_str().unwrap();
    assert!(snippet.contains("\u{1}zeppelin\u{2}"), "{snippet:?}");

    // The token gate holds on the real listener.
    let response = relay.get("/api/search?q=zeppelin").send().await.unwrap();
    assert_eq!(response.status(), 401);

    // Stopping ends the thread.
    stop.store(true, Ordering::Relaxed);
    tokio::task::spawn_blocking(move || indexer.join())
        .await
        .unwrap()
        .expect("the indexer thread ends cleanly");
}
