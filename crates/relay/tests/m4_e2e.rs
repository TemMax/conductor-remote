//! The writes of milestone 4, part 1, end to end: real HTTP requests, shaped as the web app sends
//! them, through the router, the write service, the first-prompt pump, the UI thread and the UI
//! actions, over a synthetic database and an in-memory store. Three things are fake: the window (a
//! `FakeDesktop` built inside the `UiActor::spawn` factory, whose reactions write into the
//! database what Conductor would), `gh` (a `FakeCommands` script) and the lock of the Mac as the
//! first-prompt pump sees it (a flag the test flips). Nothing here reaches the Mac, GitHub or the
//! network, and no real deep link is opened.

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Services, Token};
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::http::router;
use conductor_remote::reads::Reads;
use conductor_remote::state::prefs::Prefs;
use conductor_remote::state::store::Store;
use conductor_remote::testing::{FakeCommands, FakeConductor, MemoryAssets};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::create_link;
use conductor_remote::ui::fake::{
    add_workspace_ui, conductor_app, main_pane, show_workspace, FakeDesktop, WindowSpec,
    WorkspaceUiSpec,
};
use conductor_remote::ui::keys::{Key, Modifiers};
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;
use tower::ServiceExt;

const TOKEN: &str = "secret-token";
const REPO: &str = "relay";
const REPO_ID: &str = "r-1";
/// The live workspace of the seed: repo "relay", branch `BRANCH`, a worktree on disk.
const WORKSPACE: &str = "ws-1";
const BRANCH: &str = "user/feature-x";
const DIRECTORY: &str = "beta-dir";
/// The workspace Conductor's link creates, with its first chat.
const CREATED: &str = "ws-new";
const CREATED_BRANCH: &str = "user/new";
const CREATED_DIRECTORY: &str = "new-dir";
const FIRST_CHAT: &str = "s-first";
/// The open chats of `WORKSPACE`, in tab order, and the chats the fake window can show: (id,
/// title). `s-new-1` is the chat Cmd+T opens; `s-first` is the chat of the created workspace.
const CHATS: [(&str, &str); 4] = [
    ("s-idle", "Idle"),
    ("s-busy", "Busy"),
    ("s-new-1", "Untitled"),
    (FIRST_CHAT, "First chat"),
];
const IDLE: &str = "s-idle";
const BUSY: &str = "s-busy";
/// A closed chat of `WORKSPACE`.
const HIDDEN: &str = "s-hidden";
/// What the web app sends as `x-client-timeout-ms` for a send and for an action.
const CLIENT_TIMEOUT_MS: &str = "75000";
const WAIT: Duration = Duration::from_secs(15);

/// What the fake desktop saw, as its reactions report it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Url(String),
    Key(Key, Modifiers),
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

fn timings() -> WriteTimings {
    WriteTimings {
        delivery: DeliveryTimings {
            confirm_window: ms(60),
            poll: ms(10),
            min_attempt: ms(50),
            min_confirm: ms(10),
            retry_pause: ms(10),
        },
        stop_poll: ms(10),
        stop_checks: 5,
        chat_poll: ms(10),
        chat_checks: 4,
        send_budget: Some(ms(400)),
        restore_poll: ms(10),
        restore_checks: 3,
        create_poll: ms(10),
        create_checks: 3,
    }
}

fn command() -> Modifiers {
    Modifiers {
        command: true,
        ..Modifiers::default()
    }
}

fn insert_chat(conn: &Connection, id: &str, title: &str, hidden: i64, at: &str) {
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
         updated_at) VALUES (?1, ?2, ?3, 'idle', ?4, ?5, ?5)",
        params![id, WORKSPACE, title, hidden, at],
    )
    .unwrap();
}

fn insert_row(conn: &Connection, id: &str, content: &str) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, content, created_at, sent_at) \
         VALUES (?1, ?2, ?3, '2026-09-01T10:05:00.000Z', '2026-09-01T10:05:00.000Z')",
        params![id, IDLE, content],
    )
    .unwrap();
}

/// One repository with a checkout path and one live workspace in it, with two open chats and one
/// closed chat. `IDLE` holds a transcript of four rows: a prompt, prose with a Read call, its
/// result, and prose.
fn seed(conn: &Connection, checkout: &Path) {
    conn.execute(
        "INSERT INTO repos (id, name, root_path) VALUES (?1, ?2, ?3)",
        params![REPO_ID, REPO, checkout.to_string_lossy()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
         directory_name, state) VALUES (?1, ?1, ?2, ?3, 'beta', ?4, 'ready')",
        params![WORKSPACE, REPO_ID, BRANCH, DIRECTORY],
    )
    .unwrap();
    insert_chat(conn, IDLE, "Idle", 0, "2026-09-01 10:00:00");
    insert_chat(conn, BUSY, "Busy", 0, "2026-09-01 10:01:00");
    insert_chat(conn, HIDDEN, "Closed", 1, "2026-09-01 10:02:00");

    insert_row(conn, "seed-1", "Why does the build fail?");
    insert_row(
        conn,
        "seed-2",
        &json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "I'll read the log." },
            { "type": "tool_use", "id": "tool-1", "name": "Read",
              "input": { "file_path": "build.log" } }
        ] } })
        .to_string(),
    );
    insert_row(
        conn,
        "seed-3",
        &json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "tool-1", "content": "ld: missing -lz" }
        ] } })
        .to_string(),
    );
    insert_row(
        conn,
        "seed-4",
        &json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "The linker is missing a flag." }
        ] } })
        .to_string(),
    );
}

/// The window before any deep link: another branch of the same repo in the header, the first chat
/// selected, and a tab for every chat the scenarios send into.
fn spec() -> WindowSpec {
    WindowSpec {
        repo: REPO.to_owned(),
        branch: "main".to_owned(),
        sidebar: vec!["alpha".to_owned(), "beta".to_owned()],
        chats: CHATS.iter().map(|(_, title)| (*title).to_owned()).collect(),
        selected: 0,
        composer_value: None,
    }
}

/// The driver of the UI thread, made on it: a fake desktop whose reactions write into the database
/// at `db` (and under `root`, the workspaces root) what Conductor would.
///
/// - A creation link (`conductor://path=…`) shows the New workspace dialog; pressing Create makes
///   a new live workspace of the repo, its worktree and its first chat.
/// - A workspace link shows that workspace in the window, and un-hides the chat it names.
/// - Return sends the composer's text as a user row of the selected chat and empties it.
/// - Cmd+T opens a new visible chat of `WORKSPACE`.
fn fake_ui(db: PathBuf, root: PathBuf, log: Log) -> UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&spec());
        let desktop = FakeDesktop::new(app.clone());
        let pane = main_pane(&app);
        let area = pane.find_role("AXTextArea").expect("composer");
        let links = Connection::open(&db).expect("open the test database for writing");
        let keys = Connection::open(&db).expect("open the test database for writing");
        let creator = Connection::open(&db).expect("open the test database for writing");
        let dialog = add_workspace_ui(
            &desktop,
            &WorkspaceUiSpec {
                running: vec![],
                continue_button: false,
            },
        );
        dialog.on_create(move || {
            std::fs::create_dir_all(root.join(REPO).join(CREATED_DIRECTORY).join(".git"))
                .expect("the new worktree");
            creator
                .execute(
                    "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, \
                     created_at, updated_at) \
                     VALUES (?1, ?2, 'First chat', 'idle', 0, '2026-09-03 10:00:00', \
                     '2026-09-03 10:00:00')",
                    params![FIRST_CHAT, CREATED],
                )
                .unwrap();
            creator
                .execute(
                    "INSERT INTO workspaces (local_id, id, repository_id, branch, \
                     workspace_name, directory_name, state, active_session_id) \
                     VALUES (?1, ?1, ?2, ?3, 'new', ?4, 'ready', ?5)",
                    params![
                        CREATED,
                        REPO_ID,
                        CREATED_BRANCH,
                        CREATED_DIRECTORY,
                        FIRST_CHAT
                    ],
                )
                .unwrap();
        });

        let shown = app.clone();
        let urls = Arc::clone(&log);
        desktop.on_open_url(move |url| {
            urls.lock().unwrap().push(Seen::Url(url.to_owned()));
            let Some(rest) = url.strip_prefix("conductor://workspace?id=") else {
                // Conductor's creation link: its dialog opens, Create is pressed later.
                return;
            };
            let id = rest.split('&').next().unwrap_or_default();
            let branch = match id {
                WORKSPACE => BRANCH,
                CREATED => CREATED_BRANCH,
                other => panic!("a link to a workspace the test does not know: {other}"),
            };
            show_workspace(&shown, REPO, branch);
            if let Some((_, session)) = rest.split_once("&session=") {
                links
                    .execute("UPDATE sessions SET is_hidden = 0 WHERE id = ?1", [session])
                    .unwrap();
            }
        });

        let mut rows = 0;
        let mut opened = 0;
        desktop.on_key(move |key, modifiers| {
            log.lock().unwrap().push(Seen::Key(key, modifiers));
            match key {
                Key::Return => {
                    let selected = CHATS
                        .iter()
                        .find(|(_, title)| {
                            pane.find_label(&format!("Close chat {title}"))
                                .is_some_and(|radio| radio.is_selected())
                        })
                        .map(|(id, _)| *id)
                        .expect("a selected chat");
                    let text = area.value_text().unwrap_or_default();
                    area.set_value_text(Some(""));
                    rows += 1;
                    keys.execute(
                        "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                         VALUES (?1, ?2, 'user', ?3, 'turn-1')",
                        params![format!("m-{rows}"), selected, text],
                    )
                    .unwrap();
                }
                Key::T if modifiers.command => {
                    opened += 1;
                    keys.execute(
                        "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, \
                         created_at, updated_at) VALUES (?1, ?2, 'Untitled', 'idle', 0, ?3, ?3)",
                        params![
                            format!("s-new-{opened}"),
                            WORKSPACE,
                            format!("2026-09-02 10:00:0{opened}")
                        ],
                    )
                    .unwrap();
                }
                _ => {}
            }
        });
        Box::new(Driver::new(desktop))
    })
}

/// What a `FakeCommands` call was, without its limits.
type Call = (String, Vec<String>, Option<PathBuf>);

/// The router over the real write service, the first-prompt pump, the UI thread, the preferences
/// and a synthetic database.
struct Rig {
    test: TestDb,
    app: Router,
    log: Log,
    commands: Arc<FakeCommands>,
    /// What the first-prompt pump reads as the lock of the Mac.
    locked: Arc<AtomicBool>,
    checkout: PathBuf,
    state_dir: PathBuf,
    /// Kept so the pump, which holds the writes weakly, goes on.
    _writes: Arc<Writes>,
}

impl Rig {
    /// Starts the pump on the current tokio runtime, so call it from inside a test.
    fn new() -> Rig {
        let test = TestDb::new();
        let checkout = test.dir().join("checkout");
        seed(&test.conn(), &checkout);
        std::fs::create_dir_all(test.root().join(REPO).join(DIRECTORY).join(".git"))
            .expect("the worktree");
        let state_dir = test.dir().join("state");
        let log = Log::default();
        let ui = fake_ui(
            test.path().to_path_buf(),
            test.root().to_path_buf(),
            Arc::clone(&log),
        );
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let store = Arc::new(Store::open_in_memory().expect("an in-memory store"));
        let parked = ParkedQueue::new(
            Arc::clone(&store),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let commands = Arc::new(FakeCommands::new());
        let locked = Arc::new(AtomicBool::new(false));
        let probe = Arc::clone(&locked);
        let writes = Writes::new(Arc::clone(&reads), ui, Arc::new(|| true), timings(), parked)
            .configure(WriteDeps {
                state_dir: state_dir.clone(),
                store: Arc::clone(&store),
                commands: commands.clone(),
                locked: Arc::new(move || Some(probe.load(Ordering::SeqCst))),
            });
        let writes = Arc::new(writes);
        writes.start_first_prompts();
        let app = router(AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
            assets: Arc::new(MemoryAssets::default()),
            reads: Some(reads),
            writes: Some(writes.clone()),
            notify: None,
            services: Services {
                prefs: Some(Arc::new(Prefs::new(store))),
                host: None,
                dev: None,
            },
        });
        Rig {
            test,
            app,
            log,
            commands,
            locked,
            checkout,
            state_dir,
            _writes: writes,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }

    fn urls(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Url(url) => Some(url),
                Seen::Key(..) => None,
            })
            .collect()
    }

    fn keys(&self) -> Vec<(Key, Modifiers)> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Key(key, modifiers) => Some((key, modifiers)),
                Seen::Url(_) => None,
            })
            .collect()
    }

    fn returns(&self) -> usize {
        self.keys()
            .iter()
            .filter(|(key, _)| *key == Key::Return)
            .count()
    }

    fn calls(&self) -> Vec<Call> {
        self.commands
            .calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.cwd))
            .collect()
    }

    /// The worktree of `WORKSPACE`.
    fn worktree(&self) -> PathBuf {
        self.test.root().join(REPO).join(DIRECTORY)
    }

    /// The worktree of `CREATED`.
    fn created_worktree(&self) -> PathBuf {
        self.test.root().join(REPO).join(CREATED_DIRECTORY)
    }

    /// The staging directory of files picked before their workspace exists.
    fn staging(&self) -> PathBuf {
        self.state_dir.join("attachment-staging")
    }

    /// How many user rows of `session_id` hold exactly `text`.
    fn rows_with(&self, session_id: &str, text: &str) -> i64 {
        self.test
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM session_messages \
                 WHERE session_id = ?1 AND role = 'user' AND content = ?2",
                params![session_id, text],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn content_of(&self, message_id: &str) -> String {
        self.test
            .conn()
            .query_row(
                "SELECT content FROM session_messages WHERE id = ?1",
                [message_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn hidden(&self, session_id: &str) -> i64 {
        self.test
            .conn()
            .query_row(
                "SELECT is_hidden FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Sends a request and returns the status and the parsed JSON body.
    async fn call(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        // The write routes answer `application/json`, the read routes add a charset.
        let content_type = response.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(
            content_type
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "{content_type:?}"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.call(web_request(Method::GET, uri, &[], Vec::new()))
            .await
    }

    async fn post(&self, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let bytes = body.map_or_else(Vec::new, |body| body.to_string().into_bytes());
        self.call(web_request(Method::POST, uri, &[], bytes)).await
    }

    async fn patch(&self, uri: &str, body: &[u8]) -> (StatusCode, Value) {
        self.call(web_request(Method::PATCH, uri, &[], body.to_vec()))
            .await
    }

    /// An upload as the web app sends it: the raw bytes, the name percent-encoded in a header.
    async fn upload(&self, uri: &str, name: &str, bytes: &[u8]) -> (StatusCode, Value) {
        let encoded = percent_encode(name);
        let headers = [
            ("x-attachment-name", encoded.as_str()),
            ("content-type", "application/octet-stream"),
        ];
        self.call(web_request(Method::POST, uri, &headers, bytes.to_vec()))
            .await
    }

    async fn send(&self, session_id: &str, text: &str, client_id: &str) -> (StatusCode, Value) {
        self.post(
            &format!("/api/sessions/{session_id}/prompt"),
            Some(json!({ "text": text, "workspaceId": WORKSPACE, "clientId": client_id })),
        )
        .await
    }

    /// The workspace of `/api/state`, as the phone reads it.
    async fn state_of(&self, workspace_id: &str) -> Value {
        let (status, body) = self.get("/api/state").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["workspaces"]
            .as_array()
            .expect("workspaces")
            .iter()
            .find(|workspace| workspace["id"] == workspace_id)
            .cloned()
            .unwrap_or_else(|| panic!("{workspace_id} is not in /api/state: {body}"))
    }

    /// Polls `/api/state` until `done` accepts the workspace.
    async fn until(&self, workspace_id: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let started = Instant::now();
        loop {
            let workspace = self.state_of(workspace_id).await;
            if done(&workspace) {
                return workspace;
            }
            assert!(started.elapsed() < WAIT, "gave up waiting for {what}");
            tokio::time::sleep(ms(50)).await;
        }
    }
}

/// A request with the headers of the web app's `api()` helper; `headers` come on top.
fn web_request(
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header("x-client-timeout-ms", CLIENT_TIMEOUT_MS);
    if !headers.iter().any(|(name, _)| *name == "content-type") {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body)).unwrap()
}

/// `encodeURIComponent`, as far as the file names of the scenarios need it.
fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

// ---- upload into a chat, then send its token ----

#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_attachment_is_sent_as_its_token_and_the_row_is_the_receipt() {
    let rig = Rig::new();

    // 1. The upload writes the file into the worktree and answers with the token.
    let (status, body) = rig
        .upload(
            &format!("/api/sessions/{IDLE}/attachments?workspaceId={WORKSPACE}"),
            "build report.txt",
            b"all green",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    let attachment = &body["attachment"];
    assert_eq!(attachment["name"], "build report.txt");
    assert_eq!(attachment["bytes"], 9);
    let path = attachment["path"].as_str().expect("path");
    assert!(path.starts_with(".context/attachments/"), "{path}");
    assert!(path.ends_with("/build report.txt"), "{path}");
    let token = attachment["token"].as_str().expect("token").to_owned();
    assert!(token.starts_with("@⟦build report.txt⟧("), "{token}");
    assert_eq!(read(&rig.worktree().join(path)), "all green");
    // An upload touches neither the window nor the database.
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());

    // 2. The prompt holds the token; Conductor's row is the receipt.
    let text = format!("{token}\nsummarise this");
    let (status, body) = rig.send(IDLE, &text, "bubble-1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["strategy"], "accessibility");
    assert_eq!(body["attempts"], 1);
    assert_eq!(
        body["receipt"],
        json!({ "kind": "message", "id": "m-1", "rowid": 5, "turnId": "turn-1" })
    );
    assert_eq!(rig.content_of("m-1"), text);
    assert_eq!(rig.rows_with(IDLE, &text), 1);
    assert_eq!(
        rig.urls(),
        [format!(
            "conductor://workspace?id={WORKSPACE}&session={IDLE}"
        )]
    );
    assert_eq!(rig.returns(), 1);
}

// ---- stage, create a workspace, first prompt ----

#[tokio::test(flavor = "multi_thread")]
async fn staged_files_become_a_workspace_and_the_pump_sends_the_first_prompt() {
    let rig = Rig::new();

    // 1. Two files staged before any workspace exists.
    let mut stages = Vec::new();
    for (name, bytes) in [
        ("my notes.txt", &b"first file"[..]),
        ("plan.md", &b"second file"[..]),
    ] {
        let (status, body) = rig.upload("/api/attachments", name, bytes).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["ok"], true);
        let attachment = &body["attachment"];
        assert_eq!(attachment["name"], name);
        assert_eq!(attachment["bytes"], bytes.len());
        stages.push((
            attachment["stageId"].as_str().expect("stage id").to_owned(),
            attachment["path"].as_str().expect("path").to_owned(),
            attachment["token"].as_str().expect("token").to_owned(),
            bytes,
        ));
    }
    for (stage_id, path, _, bytes) in &stages {
        assert_eq!(stage_id.len(), 6, "{stage_id}");
        assert_eq!(
            std::fs::read(rig.staging().join(path)).unwrap(),
            *bytes,
            "{path}"
        );
    }
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());

    // 2. The pump is held back (a locked Mac freezes it), so the entry can be watched.
    rig.locked.store(true, Ordering::SeqCst);
    let objective = format!("{}\n{}\nbuild it", stages[0].2, stages[1].2);
    let (status, body) = rig
        .post(
            "/api/workspaces",
            Some(json!({
                "repo": REPO,
                "prompt": "build it",
                "attachmentIds": [stages[0].0, stages[1].0],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["workspaceId"], CREATED);
    assert_eq!(body["workspace"]["id"], CREATED);
    assert_eq!(body["pendingPrompt"], objective);
    assert_eq!(body["sent"], false);
    assert_eq!(body["configured"], false);
    // The window got Conductor's creation link and nothing else: no key, no workspace link.
    let checkout = rig.checkout.to_string_lossy().into_owned();
    let link = create_link(None, Some(&checkout));
    assert!(!link.contains("prompt="), "{link}");
    assert!(link.starts_with("conductor://path="), "{link}");
    assert_eq!(rig.urls(), [link]);
    assert!(rig.keys().is_empty(), "{:?}", rig.keys());

    // 3. The entry is on the phone's state, still waiting, and the files are still staged.
    let workspace = rig.state_of(CREATED).await;
    let entry = &workspace["pending_prompt"];
    assert_eq!(entry["workspaceId"], CREATED);
    assert_eq!(entry["text"], objective);
    assert_eq!(entry["status"], "waiting");
    assert_eq!(entry["attempts"], 0);
    assert_eq!(entry["earlyAttempts"], 0);
    assert_eq!(entry["sendImmediately"], true);
    assert_eq!(entry["attachmentIds"], json!([stages[0].0, stages[1].0]));
    assert!(
        entry["createdAt"].as_i64().is_some_and(|at| at > 0),
        "{entry}"
    );
    assert!(entry.get("error").is_none(), "{entry}");
    for (_, path, _, _) in &stages {
        assert!(rig.staging().join(path).exists(), "{path}");
        assert!(!rig.created_worktree().join(path).exists(), "{path}");
    }
    assert_eq!(rig.returns(), 0);

    // 4. The Mac unlocks: the pump copies the files into the new worktree, sends the prompt into
    //    the new chat, and the entry goes.
    rig.locked.store(false, Ordering::SeqCst);
    let workspace = rig
        .until(CREATED, "the first prompt to go", |workspace| {
            workspace.get("pending_prompt").is_none()
        })
        .await;
    assert!(workspace.get("pending_prompt").is_none(), "{workspace}");
    for (_, path, _, bytes) in &stages {
        assert_eq!(
            std::fs::read(rig.created_worktree().join(path)).unwrap(),
            *bytes,
            "{path}"
        );
        assert!(!rig.staging().join(path).exists(), "{path}");
    }
    assert_eq!(rig.rows_with(FIRST_CHAT, &objective), 1);
    assert_eq!(
        rig.urls().last().map(String::as_str),
        Some(format!("conductor://workspace?id={CREATED}&session={FIRST_CHAT}").as_str())
    );
    assert_eq!(rig.urls().len(), 2);
    assert_eq!(rig.returns(), 1);
}

// ---- merge ----

#[tokio::test(flavor = "multi_thread")]
async fn merge_uses_the_method_the_repository_allows() {
    let rig = Rig::new();
    rig.commands.on(
        "gh",
        &["repo", "view"],
        0,
        r#"{"squashMergeAllowed":false,"mergeCommitAllowed":true,"rebaseMergeAllowed":true}"#,
    );
    rig.commands.on("gh", &["pr", "merge"], 0, "");

    let (status, body) = rig
        .post(&format!("/api/workspaces/{WORKSPACE}/merge"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "ok": true, "branch": BRANCH, "method": "merge" })
    );

    // `gh` ran twice, in the repository's checkout: the allowed methods, then the merge.
    let root = Some(rig.checkout.clone());
    assert_eq!(
        rig.calls(),
        [
            (
                "gh".to_owned(),
                [
                    "repo",
                    "view",
                    "--json",
                    "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed"
                ]
                .map(str::to_owned)
                .to_vec(),
                root.clone()
            ),
            (
                "gh".to_owned(),
                ["pr", "merge", BRANCH, "--merge"]
                    .map(str::to_owned)
                    .to_vec(),
                root
            ),
        ]
    );
    // A merge happens on GitHub: the window is never touched.
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_gh_refuses_answers_409_with_its_message() {
    let rig = Rig::new();
    // `gh repo view` is not scripted, so the method falls back to squash.
    rig.commands.on_with_stderr(
        "gh",
        &["pr", "merge"],
        1,
        "",
        "Pull request is not mergeable: the base branch policy prohibits the merge.\n",
    );

    let (status, body) = rig
        .post(&format!("/api/workspaces/{WORKSPACE}/merge"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body,
        json!({
            "ok": false,
            "branch": BRANCH,
            "method": "squash",
            "error": "Pull request is not mergeable: the base branch policy prohibits the merge.",
        })
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1].1, ["pr", "merge", BRANCH, "--squash"]);
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
}

// ---- restore ----

#[tokio::test(flavor = "multi_thread")]
async fn restoring_a_hidden_chat_opens_its_link_and_the_chat_comes_back() {
    let rig = Rig::new();
    let (status, closed) = rig
        .get(&format!("/api/workspaces/{WORKSPACE}/sessions/closed"))
        .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["sessions"][0]["id"], HIDDEN, "{closed}");
    assert_eq!(rig.hidden(HIDDEN), 1);

    let uri = format!("/api/sessions/{HIDDEN}/restore");
    let (status, body) = rig
        .post(&uri, Some(json!({ "workspaceId": WORKSPACE })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["strategy"], "deep-link");
    assert!(body.get("alreadyOpen").is_none(), "{body}");
    assert_eq!(body["session"]["id"], HIDDEN);
    assert_eq!(rig.hidden(HIDDEN), 0);
    let link = format!("conductor://workspace?id={WORKSPACE}&session={HIDDEN}");
    assert_eq!(rig.urls(), [link]);
    // A link, not a key press.
    assert!(rig.keys().is_empty(), "{:?}", rig.keys());

    // The chat is among the open ones now, and out of the closed list.
    let (_, open) = rig
        .get(&format!("/api/workspaces/{WORKSPACE}/sessions"))
        .await;
    let ids: Vec<&str> = open["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&HIDDEN), "{ids:?}");
    let (_, closed) = rig
        .get(&format!("/api/workspaces/{WORKSPACE}/sessions/closed"))
        .await;
    assert_eq!(closed["sessions"], json!([]), "{closed}");

    // Once more: already open, and no second link.
    let (status, body) = rig
        .post(&uri, Some(json!({ "workspaceId": WORKSPACE })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["alreadyOpen"], true);
    assert_eq!(rig.urls().len(), 1);
}

// ---- join ----

#[tokio::test(flavor = "multi_thread")]
async fn joining_two_chats_shows_in_the_chat_history() {
    let rig = Rig::new();
    let (_, before) = rig
        .get(&format!("/api/workspaces/{WORKSPACE}/sessions"))
        .await;
    assert!(before.get("chat_history").is_none(), "{before}");

    let (status, body) = rig
        .post(
            &format!("/api/sessions/{BUSY}/history"),
            Some(json!({ "workspaceId": WORKSPACE, "previousSessionId": IDLE })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "ok": true }));

    // The link carries the earlier chat's title and position.
    let (status, after) = rig
        .get(&format!("/api/workspaces/{WORKSPACE}/sessions"))
        .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(
        after["chat_history"],
        json!({
            BUSY: {
                "previousSessionId": IDLE,
                "title": "Idle",
                "createdAt": "2026-09-01 10:00:00",
            }
        })
    );

    // Joining the same pair again changes nothing; the reverse join would close a loop.
    let (status, again) = rig
        .post(
            &format!("/api/sessions/{BUSY}/history"),
            Some(json!({ "workspaceId": WORKSPACE, "previousSessionId": IDLE })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    let (status, cycle) = rig
        .post(
            &format!("/api/sessions/{IDLE}/history"),
            Some(json!({ "workspaceId": WORKSPACE, "previousSessionId": BUSY })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{cycle}");
    assert_eq!(
        cycle,
        json!({ "error": "Chat history cannot contain a cycle" })
    );
    // A join is the relay's own record: the window is never touched.
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
}

// ---- split ----

#[tokio::test(flavor = "multi_thread")]
async fn a_split_opens_a_chat_and_its_text_is_sent_into_it() {
    let rig = Rig::new();

    // 1. The split writes the transcript into the worktree and opens a new chat (Cmd+L, Cmd+T).
    let (status, body) = rig
        .post(
            &format!("/api/sessions/{IDLE}/split"),
            Some(json!({
                "workspaceId": WORKSPACE,
                "destination": "chat",
                "prompt": "keep going",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["destination"], "chat");
    assert_eq!(body["sessionId"], "s-new-1");
    assert_eq!(body["workspaceId"], WORKSPACE);
    let attachment = &body["attachment"];
    assert_eq!(attachment["name"], "Transcript of Idle.md");
    assert_eq!(attachment["kept"], 3);
    let path = attachment["path"].as_str().expect("path");
    let transcript = read(&rig.worktree().join(path));
    assert!(
        transcript.starts_with("# Transcript of Idle\n"),
        "{transcript}"
    );
    assert!(
        transcript.contains("The linker is missing a flag."),
        "{transcript}"
    );
    assert_eq!(attachment["bytes"], transcript.len());
    let text = body["text"].as_str().expect("text").to_owned();
    assert!(
        text.starts_with("Forked from @⟦Transcript of Idle.md⟧("),
        "{text}"
    );
    assert!(text.ends_with(")\n\nkeep going"), "{text}");
    assert_eq!(rig.keys(), [(Key::L, command()), (Key::T, command())]);
    assert_eq!(
        rig.urls(),
        [format!("conductor://workspace?id={WORKSPACE}")]
    );
    assert_eq!(rig.returns(), 0);

    // 2. The returned text goes into the new chat, as the web app does with it.
    let (status, sent) = rig.send("s-new-1", &text, "bubble-1").await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["ok"], true);
    assert_eq!(sent["receipt"]["kind"], "message");
    assert_eq!(sent["receipt"]["id"], "m-1");
    assert_eq!(rig.content_of("m-1"), text);
    assert_eq!(rig.rows_with("s-new-1", &text), 1);
    assert_eq!(
        rig.urls().last().map(String::as_str),
        Some(format!("conductor://workspace?id={WORKSPACE}&session=s-new-1").as_str())
    );
    assert_eq!(rig.returns(), 1);
}

// ---- preferences ----

#[tokio::test(flavor = "multi_thread")]
async fn prefs_are_read_and_merged_through_patch() {
    let rig = Rig::new();
    let empty = json!({ "prefs": { "readMarks": {}, "drafts": {} } });
    let (status, body) = rig.get("/api/prefs").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, empty);

    let patch = json!({
        "readMarks": { IDLE: "2026-09-01T10:05:00.000Z" },
        "drafts": { "ws-1:s-idle": { "text": "half a thought", "updatedAt": 1000 } },
    });
    let (status, patched) = rig.patch("/api/prefs", patch.to_string().as_bytes()).await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(
        patched["prefs"]["readMarks"],
        json!({ IDLE: "2026-09-01T10:05:00.000Z" })
    );
    let draft = &patched["prefs"]["drafts"]["ws-1:s-idle"];
    assert_eq!(draft["text"], "half a thought");
    assert_eq!(draft["updatedAt"], 1000);
    assert_eq!(draft["deleted"], false);
    assert_eq!(draft["attachments"], json!([]));
    let (_, read_back) = rig.get("/api/prefs").await;
    assert_eq!(read_back, patched);

    // An older mark and an older draft lose; a newer draft wins.
    let stale = json!({
        "readMarks": { IDLE: "2026-09-01T09:00:00.000Z" },
        "drafts": { "ws-1:s-idle": { "text": "older", "updatedAt": 900 } },
    });
    let (status, body) = rig.patch("/api/prefs", stale.to_string().as_bytes()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, patched);
    let newer = json!({ "drafts": { "ws-1:s-idle": { "text": "finished", "updatedAt": 2000 } } });
    let (status, body) = rig.patch("/api/prefs", newer.to_string().as_bytes()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["prefs"]["drafts"]["ws-1:s-idle"]["text"], "finished");
    assert_eq!(body["prefs"]["readMarks"], patched["prefs"]["readMarks"]);

    // What the service refuses is a 400 with its text.
    let (status, body) = rig.patch("/api/prefs", b"{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body, json!({ "error": "nothing to sync" }));
    let (status, body) = rig.patch("/api/prefs", b"[]").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body, json!({ "error": "preferences must be an object" }));
    // Preferences are the relay's own: the window is never touched.
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
}
