//! The agent options end to end: real HTTP requests through the router, the write service, the UI
//! thread and the UI actions, over a fake window with the agent menus and a synthetic database.
//! The fake desktop and its menus live on the UI thread, built inside the `UiActor::spawn`
//! factory; the test sees what a wrapper driver copies into a shared snapshot after each command,
//! and what the fake's reactions write into the database. Nothing here reaches the Mac.

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
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings, PARKED_ERROR};
use conductor_remote::delivery::service::{WriteTimings, Writes};
use conductor_remote::http::router;
use conductor_remote::reads::{HostPaths, Reads};
use conductor_remote::state::store::Store;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::{
    AgentFailure, AgentOutcome, Target, UiDriver as Commands, UiError, ViewReport,
};
use conductor_remote::ui::fake::{
    add_agent_menus, conductor_app, main_pane, show_workspace, AgentMenuSpec, AgentMenus,
    FakeDesktop, FakeEvent, FakeNode, WindowSpec,
};
use conductor_remote::ui::keys::Key;
use conductor_remote::ui::screen::SessionState;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "secret-token";
const WORKSPACE: &str = "ws-1";
const REPO: &str = "relay";
const BRANCH: &str = "user/feature-x";
/// The open chats of the workspace, in tab order: (id, title).
const CHATS: [(&str, &str); 2] = [("s-idle", "Idle"), ("s-busy", "Busy")];
/// The chat the requests go to; the first tab, selected when the window opens.
const CHAT: &str = "s-idle";
/// The chat Conductor opens for a model of another provider.
const NEW_CHAT: (&str, &str) = ("s-new-1", "Untitled");
const PROMPT: &str = "fix the tests";
const CURRENT_MODEL: &str = "Opus 5.5";
const OTHER_PROVIDER_MODEL: &str = "GPT-6.1 Sol";
const CLIENT_TIMEOUT_MS: &str = "75000";

/// What the test thread sees of the UI thread, as of the end of the last command.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    /// The `Change agent` pop-up's label.
    shown: String,
    plan: Option<bool>,
    menu_open: bool,
    /// The fake desktop's events, pauses left out, as plain text.
    events: Vec<String>,
}

type Shared = Arc<Mutex<Snapshot>>;

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

/// One live workspace with the open chats of `CHATS`, both idle.
fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', ?1)", [REPO])
        .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state) \
         VALUES (?1, ?1, 'r-1', ?2, 'beta', 'ready')",
        [WORKSPACE, BRANCH],
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
        branch: "main".to_owned(),
        sidebar: vec!["alpha".to_owned(), "beta".to_owned()],
        chats: CHATS.iter().map(|(_, title)| (*title).to_owned()).collect(),
        selected: 0,
        composer_value: None,
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn menus_spec() -> AgentMenuSpec {
    AgentMenuSpec {
        models: vec![
            (CURRENT_MODEL.to_owned(), "High".to_owned(), false),
            (OTHER_PROVIDER_MODEL.to_owned(), "High".to_owned(), true),
        ],
        current: 0,
        new_chat_models: names(&[OTHER_PROVIDER_MODEL]),
        efforts: names(&["Low", "Medium", "High", "Extra high", "Max", "Ultracode"]),
        fast_item: true,
        plan: Some(false),
    }
}

fn describe(event: &FakeEvent) -> Option<String> {
    match event {
        FakeEvent::OpenUrl(url) => Some(format!("open {url}")),
        FakeEvent::Activate(_) => Some("activate".to_owned()),
        FakeEvent::Key { key, .. } => Some(format!("key {key:?}")),
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
    menus: AgentMenus,
    shared: Shared,
}

impl UiDriver {
    fn new(driver: Driver<FakeDesktop>, menus: AgentMenus, shared: Shared) -> UiDriver {
        let wrapper = UiDriver {
            driver,
            menus,
            shared,
        };
        wrapper.publish();
        wrapper
    }

    fn publish(&self) {
        let snapshot = Snapshot {
            shown: self.menus.shown(),
            plan: self.menus.plan(),
            menu_open: self.menus.menu_open(),
            events: self
                .driver
                .desktop()
                .events()
                .iter()
                .filter_map(describe)
                .collect(),
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
}

/// The UI thread: a fake window with the agent menus, whose reactions write into the database at
/// `db` what Conductor would.
fn fake_ui(db: PathBuf, locked: bool, shared: Shared) -> UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&spec());
        let menus = add_agent_menus(&app, &menus_spec());
        let desktop = FakeDesktop::new(app.clone());
        if locked {
            desktop.set_session(Some(SessionState {
                locked: true,
                on_console: true,
            }));
        }
        let pane = main_pane(&app);
        let area = pane.find_role("AXTextArea").expect("composer");
        let strip = pane
            .find_label(&format!("Close chat {}", CHATS[0].1))
            .and_then(|radio| radio.parent())
            .expect("chat strip");
        let conn = Rc::new(Connection::open(&db).expect("open the test database for writing"));

        // The deep link shows the target workspace.
        let shown = app.clone();
        desktop.on_open_url(move |url| {
            if url.starts_with(&format!("conductor://workspace?id={WORKSPACE}")) {
                show_workspace(&shown, REPO, BRANCH);
            }
        });

        // Return sends the composer's text as a user row of the selected chat and empties it.
        let writer = Rc::clone(&conn);
        let mut rows = 0;
        desktop.on_key(move |key, _| {
            if key != Key::Return {
                return;
            }
            let selected = CHATS
                .iter()
                .chain([&NEW_CHAT])
                .find(|(_, title)| {
                    pane.find_label(&format!("Close chat {title}"))
                        .is_some_and(|radio| radio.is_selected())
                })
                .map(|(id, _)| *id)
                .expect("a selected chat");
            let text = area.value_text().unwrap_or_default();
            area.set_value_text(Some(""));
            rows += 1;
            writer
                .execute(
                    "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                     VALUES (?1, ?2, 'user', ?3, 'turn-1')",
                    params![format!("m-{rows}"), selected, text],
                )
                .unwrap();
        });

        // Picking a model of another provider opens a new chat: a new visible row, and a new
        // selected tab at the end of the strip.
        menus.on_new_chat(move |_| {
            conn.execute(
                "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
                 updated_at) VALUES (?1, ?2, ?3, 'idle', 0, '2026-09-02 10:00:01', \
                 '2026-09-02 10:00:01')",
                params![NEW_CHAT.0, WORKSPACE, NEW_CHAT.1],
            )
            .unwrap();
            for radio in strip.child_nodes() {
                radio.set_selected(false);
            }
            let radio = FakeNode::new("AXRadioButton")
                .with_label(&format!("Close chat {}", NEW_CHAT.1))
                .with_selected(true);
            radio.on_press(|node| {
                if let Some(parent) = node.parent() {
                    for sibling in parent.child_nodes() {
                        sibling.set_selected(false);
                    }
                }
                node.set_selected(true);
            });
            strip.add_child(radio);
        });

        Box::new(UiDriver::new(
            Driver::new(desktop),
            menus,
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
    fn new(locked: bool) -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        let state = tempfile::tempdir().expect("temporary state directory");
        let shared = Shared::default();
        let ui = fake_ui(test.path().to_path_buf(), locked, Arc::clone(&shared));
        let reads = Arc::new(
            Reads::new(test.db(), test.root()).with_host_paths(HostPaths {
                home: state.path().to_path_buf(),
                state_dir: state.path().to_path_buf(),
            }),
        );
        let parked = ParkedQueue::new(
            Arc::new(Store::open_in_memory().expect("an in-memory store")),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let writes = Writes::new(Arc::clone(&reads), ui, Arc::new(|| true), timings(), parked);
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

    fn user_rows(&self) -> i64 {
        self.test
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM session_messages WHERE role = 'user'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

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

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.request(Method::GET, uri, None).await
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.request(Method::POST, uri, Some(body)).await
    }

    /// A prompt to `CHAT` with the agent settings the phone stages.
    async fn send_with(&self, agent: Value) -> (StatusCode, Value) {
        self.post(
            &format!("/api/sessions/{CHAT}/prompt"),
            json!({
                "text": PROMPT,
                "workspaceId": WORKSPACE,
                "clientId": "bubble-1",
                "agent": agent,
            }),
        )
        .await
    }
}

async fn json_body(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn position(events: &[String], wanted: impl Fn(&str) -> bool) -> usize {
    events
        .iter()
        .position(|event| wanted(event))
        .unwrap_or_else(|| panic!("no such event in {events:?}"))
}

// ---- the model list ----

#[tokio::test(flavor = "multi_thread")]
async fn the_model_list_is_read_off_the_menu_and_kept_for_the_catalogue() {
    let rig = Rig::new(false);
    let (status, body) = rig
        .get(&format!(
            "/api/sessions/{CHAT}/models?workspaceId={WORKSPACE}"
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "ok": true,
            "models": [CURRENT_MODEL, OTHER_PROVIDER_MODEL],
            "defaultModel": CURRENT_MODEL,
        })
    );

    let (status, catalog) = rig.get("/api/models").await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    let groups = catalog["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1, "{catalog}");
    let mut models: Vec<&str> = groups[0]["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|model| model.as_str().expect("a model name"))
        .collect();
    models.sort_unstable();
    assert_eq!(models, [OTHER_PROVIDER_MODEL, CURRENT_MODEL], "{catalog}");
    assert_eq!(groups[0]["defaultModel"], CURRENT_MODEL, "{catalog}");
    assert_eq!(catalog["defaultModel"], CURRENT_MODEL, "{catalog}");

    assert!(!rig.snapshot().menu_open);
}

// ---- a prompt with agent settings ----

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_applies_effort_fast_and_plan_before_it_is_typed() {
    let rig = Rig::new(false);
    let (status, body) = rig
        .send_with(json!({ "effort": "xhigh", "fast": true, "plan": true }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");

    let seen = rig.snapshot();
    assert_eq!(
        seen.shown,
        format!("Change agent ({CURRENT_MODEL} · Extra high · Fast)")
    );
    assert_eq!(seen.plan, Some(true));
    assert!(!seen.menu_open);
    assert_eq!(rig.rows_with(CHAT, PROMPT), 1);

    // Every control was pressed before the composer's value was set.
    let typed = position(&seen.events, |event| event == format!("set {PROMPT}"));
    for pressed in ["press Extra high", "press Fast", "press Plan mode ⇧ Tab"] {
        let at = position(&seen.events, |event| event == pressed);
        assert!(
            at < typed,
            "{pressed} came after the typing: {:?}",
            seen.events
        );
    }
    assert!(
        seen.events[typed..]
            .iter()
            .all(|event| !event.starts_with("press ")),
        "{:?}",
        seen.events
    );
}

// ---- a provider switch ----

#[tokio::test(flavor = "multi_thread")]
async fn a_model_of_another_provider_sends_the_prompt_in_the_chat_conductor_opened() {
    let rig = Rig::new(false);
    assert!(menus_spec()
        .new_chat_models
        .contains(&OTHER_PROVIDER_MODEL.to_owned()));
    assert_eq!(rig.visible_sessions(), [CHATS[0].0, CHATS[1].0]);

    let (status, body) = rig
        .send_with(json!({ "model": OTHER_PROVIDER_MODEL }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["sessionId"], NEW_CHAT.0, "{body}");

    // The reaction opened one chat, and the prompt went into it: the fake's Return writes into
    // the chat whose tab is selected.
    assert_eq!(rig.visible_sessions(), [CHATS[0].0, CHATS[1].0, NEW_CHAT.0]);
    assert_eq!(rig.rows_with(NEW_CHAT.0, PROMPT), 1);
    assert_eq!(rig.rows_with(CHAT, PROMPT), 0);
    assert_eq!(rig.user_rows(), 1);
    assert!(
        rig.snapshot()
            .shown
            .starts_with(&format!("Change agent ({OTHER_PROVIDER_MODEL}")),
        "{}",
        rig.snapshot().shown
    );
    assert!(!rig.snapshot().menu_open);
}

// ---- a model Conductor does not list ----

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_model_answers_502_and_types_nothing() {
    let rig = Rig::new(false);
    let (status, body) = rig.send_with(json!({ "model": "Nope" })).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(
        body["error"], "Conductor's model list has no model named Nope",
        "{body}"
    );

    let seen = rig.snapshot();
    assert!(
        seen.events.iter().all(|event| !event.starts_with("set ")),
        "{:?}",
        seen.events
    );
    assert_eq!(rig.user_rows(), 0);
    assert!(!seen.menu_open);
}

// ---- a locked Mac ----

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_mac_parks_the_prompt_with_its_agent_settings() {
    let rig = Rig::new(true);
    let (status, body) = rig.send_with(json!({ "effort": "low" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["parked"], true, "{body}");
    assert_eq!(body["error"], PARKED_ERROR, "{body}");
    assert_eq!(
        body["queued"]["agent"],
        json!({ "effort": "low" }),
        "{body}"
    );

    let seen = rig.snapshot();
    assert!(
        seen.events.iter().all(|event| !event.starts_with("press ")),
        "{:?}",
        seen.events
    );
    assert_eq!(rig.user_rows(), 0);
}

// ---- the agent route ----

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_route_changes_plan_and_refuses_an_empty_patch() {
    let rig = Rig::new(false);
    let (status, body) = rig
        .post(
            &format!("/api/sessions/{CHAT}/agent"),
            json!({ "workspaceId": WORKSPACE, "plan": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["sessionId"], CHAT, "{body}");
    assert_eq!(rig.snapshot().plan, Some(true));
    assert!(!rig.snapshot().menu_open);

    let (status, body) = rig
        .post(&format!("/api/sessions/{CHAT}/agent"), json!({}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body, json!({ "error": "nothing to change" }));
}
