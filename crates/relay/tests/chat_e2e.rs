//! The workspace actions end to end: real HTTP requests through the router, the write service, the
//! UI thread and the UI actions, over a fake window with the workspace controls and a synthetic
//! database. The fake desktop lives on the UI thread, built inside the `UiActor::spawn` factory;
//! the test sees what a wrapper driver copies into a shared snapshot after each command, and what
//! the fake's reactions write into the database. Nothing here reaches the Mac.

mod support;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::agent::AgentPatch;
use conductor_remote::contract::{AppState, ConductorStatus, Token};
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::http::router;
use conductor_remote::reads::{HostPaths, Reads};
use conductor_remote::state::store::Store;
use conductor_remote::testing::{FakeCommands, FakeConductor, MemoryAssets};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::{
    create_link, AgentFailure, AgentOutcome, Target, UiDriver as Commands, UiError, ViewReport,
};
use conductor_remote::ui::fake::{
    add_workspace_ui, conductor_app, show_workspace, FakeDesktop, FakeEvent, WindowSpec,
    WorkspaceUi, WorkspaceUiSpec,
};
use conductor_remote::ui::screen::SessionState;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "secret-token";
const WORKSPACE: &str = "ws-1";
const WORKSPACE_NAME: &str = "beta";
const REPO: &str = "relay";
const REPO_ROOT: &str = "/src/relay";
const BRANCH: &str = "user/feature-x";
/// The branch Conductor gives the workspace when Continue is pressed.
const CONTINUED_BRANCH: &str = "user/feature-x-2";
/// The open chats of the workspace, in tab order: (id, title).
const IDLE: (&str, &str) = ("s-idle", "Idle");
const BUSY: (&str, &str) = ("s-busy", "Busy");
const CHATS: [(&str, &str); 2] = [IDLE, BUSY];
/// The workspace Conductor makes when Create is pressed, and its only chat.
const NEW_WORKSPACE: &str = "ws-new";
const NEW_CHAT: &str = "s-new";
const CLIENT_TIMEOUT_MS: &str = "75000";
const CLOSE_ANYWAY: &str = "press Close anyway ⌘ Enter";
const ARCHIVE_ANYWAY: &str = "press Stop agents and archive ⌘ Enter";

/// What the test thread sees of the UI thread, as of the end of the last command.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    /// The fake desktop's events, pauses left out, as plain text.
    events: Vec<String>,
    closed: Vec<String>,
    archived: bool,
    status: Option<(String, String)>,
    continued: bool,
    created: usize,
    dialog_open: bool,
    menu_open: bool,
}

type Shared = Arc<Mutex<Snapshot>>;

/// What the fake window shows and how the Mac is.
#[derive(Clone, Copy, Default)]
struct Scene {
    /// The titles of the chats whose agent is running.
    running: &'static [&'static str],
    continue_button: bool,
    locked: bool,
}

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

/// One live workspace with the open chats of `CHATS`, both idle, in a repository with a checkout.
fn seed(conn: &Connection) {
    conn.execute(
        "INSERT INTO repos (id, name, root_path) VALUES ('r-1', ?1, ?2)",
        [REPO, REPO_ROOT],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state) \
         VALUES (?1, ?1, 'r-1', ?2, ?3, 'ready')",
        [WORKSPACE, BRANCH, WORKSPACE_NAME],
    )
    .unwrap();
    for (index, (id, title)) in CHATS.iter().enumerate() {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
             updated_at) VALUES (?1, ?2, ?3, 'idle', 0, ?4, ?4)",
            params![id, WORKSPACE, title, format!("2026-09-01 10:0{index}:00")],
        )
        .unwrap();
    }
}

fn spec() -> WindowSpec {
    WindowSpec {
        repo: REPO.to_owned(),
        branch: BRANCH.to_owned(),
        sidebar: vec!["alpha".to_owned(), WORKSPACE_NAME.to_owned()],
        chats: CHATS.iter().map(|(_, title)| (*title).to_owned()).collect(),
        selected: 0,
        composer_value: None,
    }
}

fn describe(event: &FakeEvent) -> Option<String> {
    match event {
        FakeEvent::OpenUrl(url) => Some(format!("open {url}")),
        FakeEvent::Activate(_) => Some("activate".to_owned()),
        FakeEvent::Key { key, modifiers, .. } => Some(format!("key {key:?} {modifiers:?}")),
        FakeEvent::Press(label) => Some(format!("press {}", label.as_deref().unwrap_or(""))),
        FakeEvent::ShowMenu(label) => Some(format!("menu {}", label.as_deref().unwrap_or(""))),
        FakeEvent::SetValue { value, .. } => Some(format!("set {value}")),
        FakeEvent::SetFocused { focused, .. } => Some(format!("focus {focused}")),
        FakeEvent::Pause(_) => None,
    }
}

/// The driver of the UI thread: the real commands over the fake desktop, and after each of them a
/// copy of what the test wants to see into `shared`.
struct UiDriver {
    driver: Driver<FakeDesktop>,
    controls: WorkspaceUi,
    shared: Shared,
}

impl UiDriver {
    fn new(driver: Driver<FakeDesktop>, controls: WorkspaceUi, shared: Shared) -> UiDriver {
        let wrapper = UiDriver {
            driver,
            controls,
            shared,
        };
        wrapper.publish();
        wrapper
    }

    fn publish(&self) {
        let snapshot = Snapshot {
            events: self
                .driver
                .desktop()
                .events()
                .iter()
                .filter_map(describe)
                .collect(),
            closed: self.controls.closed(),
            archived: self.controls.archived(),
            status: self.controls.status(),
            continued: self.controls.continued(),
            created: self.controls.created(),
            dialog_open: self.controls.dialog_open(),
            menu_open: self.controls.menu_open(),
        };
        *self.shared.lock().unwrap() = snapshot;
    }
}

impl Commands for UiDriver {
    fn trusted(&self) -> bool {
        let trusted = self.driver.trusted();
        self.publish();
        trusted
    }

    fn send_prompt(&mut self, target: &Target, text: &str, queue: bool) -> Result<u32, UiError> {
        let result = self.driver.send_prompt(target, text, queue);
        self.publish();
        result
    }

    fn stop_turn(&mut self, target: &Target) -> Result<(), UiError> {
        let result = self.driver.stop_turn(target);
        self.publish();
        result
    }

    fn new_chat(&mut self, target: &Target) -> Result<(), UiError> {
        let result = self.driver.new_chat(target);
        self.publish();
        result
    }

    fn locate(&mut self) -> Result<ViewReport, UiError> {
        let result = self.driver.locate();
        self.publish();
        result
    }

    fn open_link(&mut self, url: &str) -> Result<(), UiError> {
        let result = self.driver.open_link(url);
        self.publish();
        result
    }

    fn list_models(&mut self, target: &Target) -> Result<Vec<String>, UiError> {
        let result = self.driver.list_models(target);
        self.publish();
        result
    }

    fn set_agent(
        &mut self,
        target: &Target,
        patch: &AgentPatch,
    ) -> Result<AgentOutcome, AgentFailure> {
        let result = self.driver.set_agent(target, patch);
        self.publish();
        result
    }

    fn close_chat(&mut self, target: &Target, confirm: bool) -> Result<(), UiError> {
        let result = self.driver.close_chat(target, confirm);
        self.publish();
        result
    }

    fn set_status(&mut self, target: &Target, row: &str, label: &str) -> Result<(), UiError> {
        let result = self.driver.set_status(target, row, label);
        self.publish();
        result
    }

    fn archive(&mut self, target: &Target, confirm: bool) -> Result<(), UiError> {
        let result = self.driver.archive(target, confirm);
        self.publish();
        result
    }

    fn press_continue(&mut self, target: &Target) -> Result<(), UiError> {
        let result = self.driver.press_continue(target);
        self.publish();
        result
    }

    fn confirm_create(&mut self) -> Result<(), UiError> {
        let result = self.driver.confirm_create();
        self.publish();
        result
    }
}

/// The UI thread: a fake window with the workspace controls, whose reactions write into the
/// database at `db` what Conductor would.
fn fake_ui(db: PathBuf, scene: Scene, shared: Shared) -> UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&spec());
        let desktop = FakeDesktop::new(app.clone());
        if scene.locked {
            desktop.set_session(Some(SessionState {
                locked: true,
                on_console: true,
            }));
        }
        let controls = add_workspace_ui(
            &desktop,
            &WorkspaceUiSpec {
                running: scene
                    .running
                    .iter()
                    .map(|title| (*title).to_owned())
                    .collect(),
                continue_button: scene.continue_button,
            },
        );
        let conn = Rc::new(Connection::open(&db).expect("open the test database for writing"));

        // The deep link shows the target workspace.
        let shown = app.clone();
        desktop.on_open_url(move |url| {
            if url.starts_with(&format!("conductor://workspace?id={WORKSPACE}")) {
                show_workspace(&shown, REPO, BRANCH);
            }
        });

        // A closed chat is hidden.
        let writer = Rc::clone(&conn);
        controls.on_close(move |title| {
            writer
                .execute(
                    "UPDATE sessions SET is_hidden = 1 WHERE workspace_id = ?1 AND title = ?2",
                    params![WORKSPACE, title],
                )
                .unwrap();
        });

        // A status set on a sidebar row becomes the workspace's manual status.
        let writer = Rc::clone(&conn);
        controls.on_status(move |row, label| {
            writer
                .execute(
                    "UPDATE workspaces SET manual_status = ?1 WHERE workspace_name = ?2",
                    params![label.to_lowercase().replace(' ', "-"), row],
                )
                .unwrap();
        });

        let writer = Rc::clone(&conn);
        controls.on_archive(move || {
            writer
                .execute(
                    "UPDATE workspaces SET state = 'archived' WHERE id = ?1",
                    [WORKSPACE],
                )
                .unwrap();
        });

        let writer = Rc::clone(&conn);
        controls.on_continue(move || {
            writer
                .execute(
                    "UPDATE workspaces SET branch = ?2 WHERE id = ?1",
                    [WORKSPACE, CONTINUED_BRANCH],
                )
                .unwrap();
        });

        // Create makes a ready workspace with one chat.
        let writer = Rc::clone(&conn);
        controls.on_create(move || {
            writer
                .execute(
                    "INSERT INTO workspaces (local_id, id, repository_id, branch, \
                     workspace_name, state, active_session_id) \
                     VALUES (?1, ?1, 'r-1', 'user/new-work', 'gamma', 'ready', ?2)",
                    [NEW_WORKSPACE, NEW_CHAT],
                )
                .unwrap();
            writer
                .execute(
                    "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, \
                     created_at, updated_at) VALUES (?1, ?2, 'Untitled', 'idle', 0, \
                     '2026-09-02 10:00:00', '2026-09-02 10:00:00')",
                    [NEW_CHAT, NEW_WORKSPACE],
                )
                .unwrap();
        });

        Box::new(UiDriver::new(
            Driver::new(desktop),
            controls,
            Arc::clone(&shared),
        ))
    })
}

/// The router over the real service, the UI thread and a synthetic database.
struct Rig {
    test: TestDb,
    app: Router,
    shared: Shared,
    /// Holds the model cache; kept alive for the test.
    _state: TempDir,
}

impl Rig {
    fn new(scene: Scene) -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        let state = tempfile::tempdir().expect("temporary state directory");
        let shared = Shared::default();
        let ui = fake_ui(test.path().to_path_buf(), scene, Arc::clone(&shared));
        let reads = Arc::new(
            Reads::new(test.db(), test.root()).with_host_paths(HostPaths {
                home: state.path().to_path_buf(),
                state_dir: state.path().to_path_buf(),
            }),
        );
        let store = Arc::new(Store::open_in_memory().expect("an in-memory store"));
        let parked = ParkedQueue::new(
            Arc::clone(&store),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let writes = Writes::new(Arc::clone(&reads), ui, Arc::new(|| true), timings(), parked)
            .configure(WriteDeps {
                state_dir: state.path().to_path_buf(),
                store,
                commands: Arc::new(FakeCommands::default()),
                locked: Arc::new(|| Some(false)),
            });
        let app = router(AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
            assets: Arc::new(MemoryAssets::default()),
            reads: Some(reads),
            writes: Some(Arc::new(writes)),
            notify: None,
            services: Default::default(),
        });
        Rig {
            test,
            app,
            shared,
            _state: state,
        }
    }

    fn snapshot(&self) -> Snapshot {
        self.shared.lock().unwrap().clone()
    }

    /// Sets the status Conductor shows for a chat in the database.
    fn set_chat_status(&self, session_id: &str, status: &str) {
        self.test
            .conn()
            .execute(
                "UPDATE sessions SET status = ?2 WHERE id = ?1",
                [session_id, status],
            )
            .unwrap();
    }

    /// The ids of the open chats of the workspace, in tab order.
    fn visible_sessions(&self) -> Vec<String> {
        let conn = self.test.conn();
        let mut statement = conn
            .prepare(
                "SELECT id FROM sessions WHERE workspace_id = ?1 AND is_hidden = 0 \
                 ORDER BY created_at",
            )
            .unwrap();
        let ids = statement
            .query_map([WORKSPACE], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap();
        ids
    }

    /// The workspace's `(state, manual_status, branch)` as the database has them.
    fn workspace_row(&self) -> (String, Option<String>, String) {
        self.test
            .conn()
            .query_row(
                "SELECT state, manual_status, branch FROM workspaces WHERE id = ?1",
                [WORKSPACE],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    async fn request(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-client-timeout-ms", CLIENT_TIMEOUT_MS)
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let content_type = response.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(
            content_type
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "{content_type:?}"
        );
        (status, json_body(response).await)
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.request(Method::POST, uri, Some(body)).await
    }

    /// `DELETE /api/sessions/<session_id>` for a chat of the workspace.
    async fn close(&self, session_id: &str, extra: Value) -> (StatusCode, Value) {
        let mut body = json!({ "workspaceId": WORKSPACE });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.request(
            Method::DELETE,
            &format!("/api/sessions/{session_id}"),
            Some(body),
        )
        .await
    }

    async fn set_status(&self, status: &str) -> (StatusCode, Value) {
        self.post(
            &format!("/api/workspaces/{WORKSPACE}/status"),
            json!({ "status": status }),
        )
        .await
    }

    async fn archive(&self, body: Value) -> (StatusCode, Value) {
        self.post(&format!("/api/workspaces/{WORKSPACE}/archive"), body)
            .await
    }

    async fn continue_workspace(&self) -> (StatusCode, Value) {
        self.post(&format!("/api/workspaces/{WORKSPACE}/continue"), json!({}))
            .await
    }
}

async fn json_body(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Whether the events hold something the fake would react to: a press, a key or a link.
fn acted(events: &[String]) -> bool {
    events.iter().any(|event| {
        event.starts_with("press ") || event.starts_with("key ") || event.starts_with("open ")
    })
}

// ---- close a chat ----

#[tokio::test(flavor = "multi_thread")]
async fn closing_an_idle_chat_hides_it_and_names_a_remaining_one() {
    let rig = Rig::new(Scene::default());
    assert_eq!(rig.visible_sessions(), [IDLE.0, BUSY.0]);

    let (status, body) = rig.close(IDLE.0, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["activeSessionId"], BUSY.0, "{body}");

    assert_eq!(rig.visible_sessions(), [BUSY.0]);
    let seen = rig.snapshot();
    assert_eq!(seen.closed, [IDLE.1]);
    assert!(!seen.dialog_open);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_that_started_running_is_closed_only_when_confirmed() {
    // The database says idle: the agent started after the phone looked.
    let rig = Rig::new(Scene {
        running: &[BUSY.1],
        ..Scene::default()
    });

    let (status, body) = rig.close(BUSY.0, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["agentRunning"], true, "{body}");
    assert_eq!(
        body["error"], "The agent is still working in this chat. Confirm closing it anyway.",
        "{body}"
    );
    let seen = rig.snapshot();
    assert!(seen.closed.is_empty(), "{:?}", seen.closed);
    assert!(!seen.dialog_open);
    assert!(
        !seen.events.iter().any(|event| event == CLOSE_ANYWAY),
        "{:?}",
        seen.events
    );
    assert_eq!(rig.visible_sessions(), [IDLE.0, BUSY.0]);

    let (status, body) = rig.close(BUSY.0, json!({ "closeRunning": true })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    let seen = rig.snapshot();
    assert_eq!(seen.closed, [BUSY.1]);
    assert!(!seen.dialog_open);
    assert!(
        seen.events.iter().any(|event| event == CLOSE_ANYWAY),
        "{:?}",
        seen.events
    );
    assert_eq!(rig.visible_sessions(), [IDLE.0]);
}

// ---- the status of a workspace ----

#[tokio::test(flavor = "multi_thread")]
async fn a_status_is_set_through_the_row_menu_and_a_wrong_one_is_refused() {
    let rig = Rig::new(Scene::default());
    let (status, body) = rig.set_status("in-review").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["workspace"]["manual_status"], "in-review", "{body}");

    let seen = rig.snapshot();
    assert_eq!(
        seen.status,
        Some((WORKSPACE_NAME.to_owned(), "In review".to_owned()))
    );
    assert!(!seen.menu_open);

    let (status, body) = rig.set_status("nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"], "status must be one of backlog, in-progress, in-review, done, canceled",
        "{body}"
    );
    assert_eq!(
        rig.snapshot().status,
        Some((WORKSPACE_NAME.to_owned(), "In review".to_owned()))
    );
    assert_eq!(rig.workspace_row().1.as_deref(), Some("in-review"));
}

// ---- archive ----

#[tokio::test(flavor = "multi_thread")]
async fn archiving_waits_for_the_agents_to_be_stopped_and_is_idempotent() {
    let rig = Rig::new(Scene {
        running: &[BUSY.1],
        ..Scene::default()
    });
    rig.set_chat_status(BUSY.0, "working");

    let (status, body) = rig.archive(json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["agentsRunning"], true, "{body}");
    let seen = rig.snapshot();
    assert!(!acted(&seen.events), "{:?}", seen.events);
    assert!(!seen.archived);
    assert_eq!(rig.workspace_row().0, "ready");

    let (status, body) = rig.archive(json!({ "stopAgents": true })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["workspace"]["archived"], true, "{body}");
    let seen = rig.snapshot();
    assert!(seen.archived);
    assert!(!seen.dialog_open);
    assert!(
        seen.events.iter().any(|event| event == ARCHIVE_ANYWAY),
        "{:?}",
        seen.events
    );
    assert_eq!(rig.workspace_row().0, "archived");

    let before = seen.events.len();
    let (status, body) = rig.archive(json!({ "stopAgents": true })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["alreadyArchived"], true, "{body}");
    assert_eq!(rig.snapshot().events.len(), before);
}

// ---- Continue ----

#[tokio::test(flavor = "multi_thread")]
async fn continue_presses_the_button_and_answers_the_new_branch() {
    let rig = Rig::new(Scene {
        continue_button: true,
        ..Scene::default()
    });
    let (status, body) = rig.continue_workspace().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["previousBranch"], BRANCH, "{body}");
    assert_eq!(body["workspace"]["branch"], CONTINUED_BRANCH, "{body}");
    assert_ne!(
        body["workspace"]["branch"], body["previousBranch"],
        "{body}"
    );
    assert!(rig.snapshot().continued);
    assert_eq!(rig.workspace_row().2, CONTINUED_BRANCH);
}

#[tokio::test(flavor = "multi_thread")]
async fn continue_without_the_button_answers_502_and_changes_nothing() {
    let rig = Rig::new(Scene::default());
    let (status, body) = rig.continue_workspace().await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["error"], UiError::NoContinue.to_string(), "{body}");
    assert!(!rig.snapshot().continued);
    assert_eq!(rig.workspace_row().2, BRANCH);
}

// ---- a new workspace ----

#[tokio::test(flavor = "multi_thread")]
async fn creating_a_workspace_shows_the_dialog_and_presses_create_once() {
    let rig = Rig::new(Scene::default());
    let (status, body) = rig.post("/api/workspaces", json!({ "repo": REPO })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["workspaceId"], NEW_WORKSPACE, "{body}");
    assert_eq!(body["workspace"]["id"], NEW_WORKSPACE, "{body}");

    let seen = rig.snapshot();
    let link = create_link(None, Some(REPO_ROOT));
    assert!(
        seen.events.contains(&format!("open {link}")),
        "{:?}",
        seen.events
    );
    assert_eq!(seen.created, 1);
    assert!(!seen.dialog_open);
    let creates = seen
        .events
        .iter()
        .filter(|event| *event == "press Create")
        .count();
    assert_eq!(creates, 1, "{:?}", seen.events);
}

// ---- a locked Mac ----

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_mac_refuses_every_action_and_presses_nothing() {
    let rig = Rig::new(Scene {
        continue_button: true,
        locked: true,
        ..Scene::default()
    });
    let answers = [
        rig.close(IDLE.0, json!({})).await,
        rig.set_status("in-review").await,
        rig.archive(json!({})).await,
        rig.continue_workspace().await,
    ];
    for (status, body) in answers {
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["ok"], false, "{body}");
        let error = body["error"].as_str().expect("an error text");
        assert!(error.starts_with("The Mac is locked"), "{body}");
    }

    let seen = rig.snapshot();
    assert!(seen.events.is_empty(), "{:?}", seen.events);
    assert!(seen.closed.is_empty());
    assert!(!seen.archived);
    assert_eq!(seen.status, None);
    assert!(!seen.continued);
    assert_eq!(rig.visible_sessions(), [IDLE.0, BUSY.0]);
    assert_eq!(
        rig.workspace_row(),
        ("ready".to_owned(), None, BRANCH.to_owned())
    );
}
