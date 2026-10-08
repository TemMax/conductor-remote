//! The three writes end to end: real HTTP requests through the router, the write service, the UI
//! thread and the UI actions, over a fake window and a synthetic database. The fake desktop lives
//! on the UI thread, built inside the `UiActor::spawn` factory; the test sees only what its
//! reactions push into a shared log, and what they write into the database. Nothing here reaches
//! the Mac.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Token};
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings, PARKED_ERROR};
use conductor_remote::delivery::service::{WriteTimings, Writes};
use conductor_remote::http::router;
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::UiActor;
use conductor_remote::ui::fake::{
    conductor_app, main_pane, show_workspace, FakeDesktop, WindowSpec,
};
use conductor_remote::ui::keys::{Key, Modifiers};
use conductor_remote::ui::screen::SessionState;
use http_body_util::BodyExt;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;
use tower::ServiceExt;

const TOKEN: &str = "secret-token";
const WORKSPACE: &str = "ws-1";
const REPO: &str = "relay";
const BRANCH: &str = "user/feature-x";
/// The open chats of the workspace, in tab order: (id, title).
const CHATS: [(&str, &str); 2] = [("s-idle", "Idle"), ("s-busy", "Busy")];
/// The chat that is working when the database is seeded.
const WORKING: &str = "s-busy";
/// The chat that is idle when the database is seeded.
const IDLE: &str = "s-idle";
const PROMPT: &str = "fix the tests";
/// What the web app sends as `x-client-timeout-ms` for a send and for an action.
const CLIENT_TIMEOUT_MS: &str = "75000";

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

fn command_shift() -> Modifiers {
    Modifiers {
        command: true,
        shift: true,
        ..Modifiers::default()
    }
}

/// One live workspace with the open chats of `CHATS` (the second one working).
fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', ?1)", [REPO])
        .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state) \
         VALUES (?1, ?1, 'r-1', ?2, 'beta', 'ready')",
        [WORKSPACE, BRANCH],
    )
    .unwrap();
    let chats = [
        (CHATS[0].0, CHATS[0].1, "idle", "2026-09-01 10:00:00"),
        (CHATS[1].0, CHATS[1].1, "working", "2026-09-01 10:01:00"),
    ];
    for (id, title, status, created_at) in chats {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
             updated_at) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
            params![id, WORKSPACE, title, status, created_at],
        )
        .unwrap();
    }
}

/// The window before the deep link: another branch of the same repo in the header, the first
/// chat selected.
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

/// The driver of the UI thread, made on it: a fake desktop whose reactions write into the
/// database at `db` what Conductor would.
fn fake_ui(db: PathBuf, locked: bool, log: Log) -> conductor_remote::ui::actor::UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&spec());
        let desktop = FakeDesktop::new(app.clone());
        if locked {
            desktop.set_session(Some(SessionState {
                locked: true,
                on_console: true,
            }));
        }
        let pane = main_pane(&app);
        let area = pane.find_role("AXTextArea").expect("composer");
        let conn = Connection::open(&db).expect("open the test database for writing");

        // The deep link shows the target workspace.
        let shown = app.clone();
        let urls = Arc::clone(&log);
        desktop.on_open_url(move |url| {
            urls.lock().unwrap().push(Seen::Url(url.to_owned()));
            if url.starts_with(&format!("conductor://workspace?id={WORKSPACE}")) {
                show_workspace(&shown, REPO, BRANCH);
            }
        });

        let mut rows = 0;
        let mut opened = 0;
        desktop.on_key(move |key, modifiers| {
            log.lock().unwrap().push(Seen::Key(key, modifiers));
            let selected = CHATS
                .iter()
                .find(|(_, title)| {
                    pane.find_label(&format!("Close chat {title}"))
                        .is_some_and(|radio| radio.is_selected())
                })
                .map(|(id, _)| *id)
                .expect("a selected chat");
            match key {
                // Return sends the composer's text as a user row of the chat and empties it.
                Key::Return => {
                    let text = area.value_text().unwrap_or_default();
                    area.set_value_text(Some(""));
                    rows += 1;
                    conn.execute(
                        "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                         VALUES (?1, ?2, 'user', ?3, 'turn-1')",
                        params![format!("m-{rows}"), selected, text],
                    )
                    .unwrap();
                }
                // Cmd+Shift+Delete stops the chat.
                Key::Delete if modifiers.command && modifiers.shift => {
                    conn.execute(
                        "UPDATE sessions SET status = 'idle' WHERE id = ?1",
                        [selected],
                    )
                    .unwrap();
                }
                // Cmd+T opens a new visible chat.
                Key::T if modifiers.command => {
                    opened += 1;
                    conn.execute(
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

/// The router over the real service, the UI thread and a synthetic database.
struct Rig {
    test: TestDb,
    app: Router,
    log: Log,
}

impl Rig {
    fn new(locked: bool) -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        let log = Log::default();
        let ui = fake_ui(test.path().to_path_buf(), locked, Arc::clone(&log));
        let reads = Arc::new(Reads::new(test.db(), test.root()));
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
        Rig { test, app, log }
    }

    fn seen(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
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

    fn urls(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Url(url) => Some(url),
                Seen::Key(..) => None,
            })
            .collect()
    }

    fn status(&self, session_id: &str) -> String {
        self.test
            .conn()
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
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

    /// Sends the request as the web app does and returns the status and the parsed body.
    async fn call(&self, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let response = self.app.clone().oneshot(web_post(uri, body)).await.unwrap();
        let status = response.status();
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        (status, json_body(response).await)
    }

    async fn send(&self, session_id: &str, client_id: &str) -> (StatusCode, Value) {
        self.call(
            &format!("/api/sessions/{session_id}/prompt"),
            Some(json!({ "text": PROMPT, "workspaceId": WORKSPACE, "clientId": client_id })),
        )
        .await
    }

    async fn stop(&self, session_id: &str) -> (StatusCode, Value) {
        self.call(
            &format!("/api/sessions/{session_id}/stop"),
            Some(json!({ "workspaceId": WORKSPACE })),
        )
        .await
    }

    async fn new_chat(&self) -> (StatusCode, Value) {
        self.call(&format!("/api/workspaces/{WORKSPACE}/sessions"), None)
            .await
    }
}

/// A POST with the headers and the body shape of the web app's `api()` helper; a new chat has no
/// body.
fn web_post(uri: &str, body: Option<Value>) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-client-timeout-ms", CLIENT_TIMEOUT_MS)
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap()
}

async fn json_body(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn assert_locked(status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["strategy"], "accessibility", "{body}");
    let error = body["error"].as_str().expect("error text");
    assert!(error.starts_with("The Mac is locked"), "{error}");
}

// ---- send ----

#[tokio::test(flavor = "multi_thread")]
async fn send_answers_200_with_a_message_receipt_holding_the_text() {
    let rig = Rig::new(false);
    let (status, body) = rig.send(WORKING, "bubble-1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["strategy"], "accessibility");
    assert_eq!(body["attempts"], 1);
    assert_eq!(
        body["receipt"],
        json!({ "kind": "message", "id": "m-1", "rowid": 1, "turnId": "turn-1" })
    );
    // The receipt names the row Conductor wrote, and the row holds the prompt.
    let id = body["receipt"]["id"].as_str().expect("receipt id");
    assert_eq!(rig.content_of(id), PROMPT);
    assert_eq!(rig.rows_with(WORKING, PROMPT), 1);
    // The deep link was opened for the target chat and Return was pressed once.
    assert_eq!(
        rig.urls(),
        [format!(
            "conductor://workspace?id={WORKSPACE}&session={WORKING}"
        )]
    );
    assert_eq!(rig.returns(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_client_id_answers_the_same_body_and_types_once() {
    let rig = Rig::new(false);
    let (first_status, first) = rig.send(WORKING, "bubble-1").await;
    let (second_status, second) = rig.send(WORKING, "bubble-1").await;
    assert_eq!(first_status, StatusCode::OK, "{first}");
    assert_eq!(second_status, StatusCode::OK, "{second}");
    assert_eq!(second, first);
    // One row in the database, one Return and one deep link on the UI thread.
    assert_eq!(rig.rows_with(WORKING, PROMPT), 1);
    assert_eq!(rig.returns(), 1);
    assert_eq!(rig.urls().len(), 1);
}

// ---- stop ----

#[tokio::test(flavor = "multi_thread")]
async fn stop_on_a_working_chat_answers_200_and_the_chat_is_idle() {
    let rig = Rig::new(false);
    assert_eq!(rig.status(WORKING), "working");
    let (status, body) = rig.stop(WORKING).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["strategy"], "accessibility");
    assert_eq!(body["session"]["id"], WORKING);
    assert_eq!(body["session"]["status"], "idle");
    assert!(body.get("alreadyIdle").is_none());
    assert_eq!(rig.status(WORKING), "idle");
    assert!(rig.keys().contains(&(Key::Delete, command_shift())));
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_on_an_idle_chat_is_already_idle_and_touches_nothing() {
    let rig = Rig::new(false);
    let (status, body) = rig.stop(IDLE).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["alreadyIdle"], true);
    assert_eq!(body["session"]["id"], IDLE);
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
}

// ---- new chat ----

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_answers_200_with_the_new_chats_id() {
    let rig = Rig::new(false);
    let (status, body) = rig.new_chat().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "ok": true, "sessionId": "s-new-1" }));
    assert_eq!(rig.keys(), [(Key::L, command()), (Key::T, command())]);
    assert_eq!(
        rig.urls(),
        [format!("conductor://workspace?id={WORKSPACE}")]
    );
}

// ---- a locked session ----

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_mac_parks_a_send_and_answers_202() {
    let rig = Rig::new(true);
    let (status, body) = rig.send(WORKING, "bubble-1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["parked"], true, "{body}");
    assert_eq!(body["strategy"], "accessibility", "{body}");
    assert_eq!(body["queued"]["status"], "waiting", "{body}");
    assert_eq!(
        body["queued"]["reason"], "Sends when the Mac is unlocked",
        "{body}"
    );
    assert_eq!(body["error"], PARKED_ERROR, "{body}");
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
    assert_eq!(rig.rows_with(WORKING, PROMPT), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_mac_answers_502_for_stop_on_a_working_chat() {
    let rig = Rig::new(true);
    let (status, body) = rig.stop(WORKING).await;
    assert_locked(status, &body);
    assert_eq!(rig.status(WORKING), "working");
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_mac_still_answers_already_idle_for_stop_on_an_idle_chat() {
    let rig = Rig::new(true);
    let (status, body) = rig.stop(IDLE).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["alreadyIdle"], true);
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_mac_answers_502_for_a_new_chat() {
    let rig = Rig::new(true);
    let (status, body) = rig.new_chat().await;
    assert_locked(status, &body);
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
}

// ---- auto ----

#[tokio::test(flavor = "multi_thread")]
async fn auto_answers_503_and_never_reaches_the_ui() {
    let rig = Rig::new(false);
    let (status, body) = rig
        .call(
            &format!("/api/sessions/{WORKING}/prompt"),
            Some(json!({
                "text": PROMPT,
                "workspaceId": WORKSPACE,
                "clientId": "bubble-1",
                "auto": true,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body, json!({ "error": "Auto is unavailable." }));
    assert!(rig.seen().is_empty(), "{:?}", rig.seen());
    assert_eq!(rig.rows_with(WORKING, PROMPT), 0);
}
