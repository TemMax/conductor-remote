//! Parking and notifications end to end: real HTTP requests through the router, the write
//! service, the parked queue and its pump, the notifier with its ticker, the store, the UI thread
//! and the UI actions, over a synthetic database. Three things are fake: the window (a
//! `FakeDesktop` built inside the `UiActor::spawn` factory, whose reactions write into the
//! database what Conductor would), the lock of the Mac (a flag the test flips) and the push
//! service (a sender that records every request). The test reads a push the way a browser does:
//! it holds the subscription's private key and decrypts the recorded body. Nothing here reaches
//! the Mac or the network.

mod support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use conductor_remote::contract::{AppState, ConductorStatus, Token};
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{
    Deliverer, Notice, ParkedQueue, ParkedTimings, MAX_ATTEMPTS, PARKED_ERROR,
};
use conductor_remote::delivery::service::{WriteTimings, Writes};
use conductor_remote::http::router;
use conductor_remote::notify::sender::{PushRequest, PushResult, PushSender};
use conductor_remote::notify::service::{Notifier, NotifyConfig, DEFAULT_SUBJECT, TTL_SECS};
use conductor_remote::notify::{device_id, NotifyService};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::{ParkedRow, Store};
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::UiActor;
use conductor_remote::ui::desktop::Desktop;
use conductor_remote::ui::fake::{
    conductor_app, main_pane, show_workspace, FakeDesktop, FakeNode, WindowSpec,
};
use conductor_remote::ui::keys::{Key, Modifiers};
use conductor_remote::ui::screen::SessionState;
use hkdf::Hkdf;
use http_body_util::BodyExt;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{FieldBytes, PublicKey, SecretKey};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::Sha256;
use support::TestDb;
use tower::ServiceExt;

const TOKEN: &str = "secret-token";
const WORKSPACE: &str = "ws-1";
const REPO: &str = "relay";
const BRANCH: &str = "user/feature-x";
/// The workspace's name in the database, and so the title of a parked-prompt push.
const WORKSPACE_NAME: &str = "beta";
/// The open chats of the workspace, in tab order: (id, title).
const CHATS: [(&str, &str); 2] = [("s-idle", "Idle"), ("s-busy", "Busy")];
const IDLE: &str = "s-idle";
const PROMPT: &str = "fix the tests";
const ENDPOINT: &str = "https://push.example/phone";
const WAIT: Duration = Duration::from_secs(10);

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

// ---------------------------------------------------------------------------------------------
// The fake push service.

/// Records every request and answers `ok`.
#[derive(Default)]
struct FakeSender {
    requests: Mutex<Vec<PushRequest>>,
}

impl PushSender for FakeSender {
    fn send(&self, request: &PushRequest) -> PushResult {
        self.requests.lock().unwrap().push(request.clone());
        PushResult {
            ok: true,
            status: 201,
            error: None,
            gone: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A browser's key pair, to read what was sent.

struct Ua {
    secret: SecretKey,
    auth: [u8; 16],
}

impl Ua {
    fn new(seed: u8) -> Ua {
        Ua {
            secret: SecretKey::from_bytes(FieldBytes::from_slice(&[seed; 32])).unwrap(),
            auth: [seed.wrapping_add(1); 16],
        }
    }

    fn public(&self) -> Vec<u8> {
        self.secret
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec()
    }

    /// The `subscription` of `POST /api/push/subscribe`, as `PushManager.subscribe` yields it.
    fn subscription(&self) -> Value {
        json!({
            "endpoint": ENDPOINT,
            "keys": {
                "p256dh": URL_SAFE_NO_PAD.encode(self.public()),
                "auth": URL_SAFE_NO_PAD.encode(self.auth),
            },
        })
    }

    /// RFC 8291 from the receiving side; returns the payload JSON.
    fn decrypt(&self, body: &[u8]) -> Value {
        let salt = &body[..16];
        let id_len = usize::from(body[20]);
        let as_public_bytes = &body[21..21 + id_len];
        let as_public = PublicKey::from_sec1_bytes(as_public_bytes).unwrap();
        let shared =
            p256::ecdh::diffie_hellman(self.secret.to_nonzero_scalar(), as_public.as_affine());
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(&self.public());
        key_info.extend_from_slice(as_public_bytes);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&self.auth), shared.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .unwrap();
        let content = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        content
            .expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        let mut nonce = [0u8; 12];
        content
            .expand(b"Content-Encoding: nonce\0", &mut nonce)
            .unwrap();
        let mut plain = Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(Nonce::from_slice(&nonce), &body[21 + id_len..])
            .unwrap();
        assert_eq!(plain.pop(), Some(0x02));
        serde_json::from_slice(&plain).unwrap()
    }
}

// ---------------------------------------------------------------------------------------------
// The fake window, and a lock the test can switch.

/// What the fake desktop saw on the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Url(String),
    Key(Key, Modifiers),
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// A `FakeDesktop` whose lock is a flag: everything is delegated except `session()`, which reads
/// the flag. The parked queue's lock probe reads the same flag.
struct SwitchDesktop {
    inner: FakeDesktop,
    locked: Arc<AtomicBool>,
}

impl Desktop for SwitchDesktop {
    type Node = FakeNode;

    fn trusted(&self) -> bool {
        self.inner.trusted()
    }

    fn session(&self) -> Option<SessionState> {
        Some(SessionState {
            locked: self.locked.load(Ordering::SeqCst),
            on_console: true,
        })
    }

    fn conductor_pid(&self) -> Option<i32> {
        self.inner.conductor_pid()
    }

    fn application(&self, pid: i32) -> FakeNode {
        self.inner.application(pid)
    }

    fn open_url(&self, url: &str) -> bool {
        self.inner.open_url(url)
    }

    fn frontmost_pid(&self) -> Option<i32> {
        self.inner.frontmost_pid()
    }

    fn activate(&self, pid: i32) -> bool {
        self.inner.activate(pid)
    }

    fn post_key(&self, pid: i32, key: Key, modifiers: Modifiers) -> Result<(), String> {
        self.inner.post_key(pid, key, modifiers)
    }

    fn pause(&self, duration: Duration) {
        self.inner.pause(duration)
    }
}

/// The window before the deep link: another branch of the same repo in the header, the first
/// chat selected.
fn spec() -> WindowSpec {
    WindowSpec {
        repo: REPO.to_owned(),
        branch: "main".to_owned(),
        sidebar: vec!["alpha".to_owned(), WORKSPACE_NAME.to_owned()],
        chats: CHATS.iter().map(|(_, title)| (*title).to_owned()).collect(),
        selected: 0,
        composer_value: None,
    }
}

/// The driver of the UI thread, made on it. The deep link shows the target workspace while
/// `shows` is set; Return writes the composer's text as a user row of the selected chat.
fn fake_ui(
    db: PathBuf,
    locked: Arc<AtomicBool>,
    shows: Arc<AtomicBool>,
    log: Log,
) -> conductor_remote::ui::actor::UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&spec());
        let desktop = FakeDesktop::new(app.clone());
        let pane = main_pane(&app);
        let area = pane.find_role("AXTextArea").expect("composer");
        let conn = Connection::open(&db).expect("open the test database for writing");

        let shown = app.clone();
        let urls = Arc::clone(&log);
        desktop.on_open_url(move |url| {
            urls.lock().unwrap().push(Seen::Url(url.to_owned()));
            if shows.load(Ordering::SeqCst)
                && url.starts_with(&format!("conductor://workspace?id={WORKSPACE}"))
            {
                show_workspace(&shown, REPO, BRANCH);
            }
        });

        let mut rows = 0;
        desktop.on_key(move |key, modifiers| {
            log.lock().unwrap().push(Seen::Key(key, modifiers));
            if key != Key::Return {
                return;
            }
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
            conn.execute(
                "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                 VALUES (?1, ?2, 'user', ?3, 'turn-1')",
                params![format!("m-{rows}"), selected, text],
            )
            .unwrap();
        });
        Box::new(Driver::new(SwitchDesktop {
            inner: desktop,
            locked,
        }))
    })
}

// ---------------------------------------------------------------------------------------------
// The database.

/// One live workspace with the open chats of `CHATS`, all idle.
fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', ?1)", [REPO])
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

fn set_status(conn: &Connection, id: &str, status: &str) {
    conn.execute(
        "UPDATE sessions SET status = ?2 WHERE id = ?1",
        params![id, status],
    )
    .unwrap();
}

fn say(conn: &Connection, id: &str, session: &str, text: &str) {
    let content = json!({ "type": "assistant", "message": { "content": [
        { "type": "text", "text": text }
    ] } })
    .to_string();
    conn.execute(
        "INSERT INTO session_messages (id, session_id, content) VALUES (?1, ?2, ?3)",
        params![id, session, content],
    )
    .unwrap();
}

// ---------------------------------------------------------------------------------------------
// The rig.

fn write_timings() -> WriteTimings {
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

/// Short, so the pump and the ticker answer within a test's patience.
const TICK: Duration = Duration::from_millis(20);

fn parked_timings() -> ParkedTimings {
    ParkedTimings {
        poll: ms(20),
        retry: ms(10),
    }
}

fn notify_config() -> NotifyConfig {
    NotifyConfig {
        enabled: true,
        subject: DEFAULT_SUBJECT.to_owned(),
        tick: TICK,
        fresh: Duration::from_secs(10),
    }
}

/// The router over everything real except the window, the lock and the push service. Built the
/// way `main` wires the pieces, and started the way `run_server` starts them; must be built inside
/// a tokio runtime.
struct Rig {
    test: TestDb,
    app: Router,
    log: Log,
    /// The Mac's lock, read by the fake window and by the queue's lock probe.
    locked: Arc<AtomicBool>,
    /// Whether the deep link shows the workspace in the fake window.
    shows: Arc<AtomicBool>,
    sender: Arc<FakeSender>,
    ua: Ua,
}

impl Rig {
    fn new() -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        let log = Log::default();
        let locked = Arc::new(AtomicBool::new(false));
        let shows = Arc::new(AtomicBool::new(true));
        let ui = fake_ui(
            test.path().to_path_buf(),
            Arc::clone(&locked),
            Arc::clone(&shows),
            Arc::clone(&log),
        );
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let store = Arc::new(Store::open_in_memory().expect("an in-memory store"));
        let sender = Arc::new(FakeSender::default());
        let notifier = Notifier::new(
            Arc::clone(&store),
            Arc::clone(&reads),
            sender.clone(),
            notify_config(),
        )
        .expect("a notifier");
        let probe = Arc::clone(&locked);
        let parked = ParkedQueue::new(
            store,
            Arc::new(move || Some(probe.load(Ordering::SeqCst))),
            parked_timings(),
        );
        let writes = Arc::new(Writes::new(
            Arc::clone(&reads),
            ui,
            Arc::new(|| true),
            write_timings(),
            Arc::clone(&parked),
        ));
        let app = router(AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
            assets: Arc::new(MemoryAssets::default()),
            reads: Some(reads),
            writes: Some(writes.clone()),
            notify: Some(notifier.clone() as Arc<dyn NotifyService>),
            services: Default::default(),
        });

        notifier.start(Arc::new(|| true));
        let deliverer: Deliverer = {
            let writes = Arc::clone(&writes);
            Arc::new(move |row| writes.deliver_parked(row))
        };
        let notice: Notice = {
            let notifier = Arc::clone(&notifier);
            Arc::new(move |row: &ParkedRow, error: Option<&str>| {
                notifier.notify_parked(row, error);
            })
        };
        parked.start(deliverer, notice, 0);

        Rig {
            test,
            app,
            log,
            locked,
            shows,
            sender,
            ua: Ua::new(7),
        }
    }

    fn lock(&self) {
        self.locked.store(true, Ordering::SeqCst);
    }

    fn unlock(&self) {
        self.locked.store(false, Ordering::SeqCst);
    }

    /// How many times the fake window saw Return.
    fn returns(&self) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| matches!(seen, Seen::Key(Key::Return, _)))
            .count()
    }

    /// The user rows of `session_id` that hold exactly `text`.
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

    /// Every push sent so far, decrypted by the subscribed browser's key.
    fn pushes(&self) -> Vec<Value> {
        self.sender
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| self.ua.decrypt(&request.body))
            .collect()
    }

    fn requests(&self) -> Vec<PushRequest> {
        self.sender.requests.lock().unwrap().clone()
    }

    async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-client-timeout-ms", "75000")
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.call(Method::GET, uri, None).await
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, uri, Some(body)).await
    }

    async fn subscribe(&self) -> String {
        let (status, body) = self
            .post(
                "/api/push/subscribe",
                json!({ "subscription": self.ua.subscription(), "label": "test phone" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true, "{body}");
        body["id"].as_str().expect("device id").to_owned()
    }

    async fn send(&self, session_id: &str, text: &str) -> (StatusCode, Value) {
        self.post(
            &format!("/api/sessions/{session_id}/prompt"),
            json!({ "text": text, "workspaceId": WORKSPACE, "clientId": "bubble-1" }),
        )
        .await
    }

    /// The parked prompts `/api/state` lists under the workspace.
    async fn parked_in_state(&self) -> Vec<Value> {
        let (status, state) = self.get("/api/state").await;
        assert_eq!(status, StatusCode::OK, "{state}");
        let workspaces = state["workspaces"].as_array().expect("workspaces");
        let workspace = workspaces
            .iter()
            .find(|workspace| workspace["id"] == WORKSPACE)
            .unwrap_or_else(|| panic!("the workspace is listed: {state}"));
        workspace["parked_prompts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

/// Polls until `done` holds; panics after `WAIT`.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(ms(10)).await;
    }
}

/// A pause of a few ticks, for the ticker to see the database as it is.
async fn some_ticks() {
    tokio::time::sleep(TICK * 8).await;
}

// ---------------------------------------------------------------------------------------------
// Devices.

#[tokio::test(flavor = "multi_thread")]
async fn a_subscribed_device_is_listed() {
    let rig = Rig::new();
    let (status, before) = rig.get("/api/push").await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(before["enabled"], true);
    assert_eq!(before["devices"], json!([]));

    let id = rig.subscribe().await;
    assert_eq!(id, device_id(ENDPOINT));

    let (status, after) = rig.get("/api/push").await;
    assert_eq!(status, StatusCode::OK, "{after}");
    let devices = after["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 1, "{after}");
    assert_eq!(devices[0]["id"], id.as_str());
    assert_eq!(devices[0]["label"], "test phone");
    assert_eq!(devices[0]["failures"], 0);
    // The list never carries the endpoint or the keys.
    assert!(!after.to_string().contains(ENDPOINT), "{after}");
    assert!(rig.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_test_push_sends_one_push() {
    let rig = Rig::new();
    let id = rig.subscribe().await;
    let (status, body) = rig.post("/api/push/test", json!({ "id": id })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "ok": true }));

    let requests = rig.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].endpoint, ENDPOINT);
    assert_eq!(requests[0].ttl_secs, TTL_SECS);
    let push = rig.ua.decrypt(&requests[0].body);
    assert_eq!(push["title"], "Conductor Remote");
    assert_eq!(push["tag"], "test");
    assert_eq!(push["kind"], "test");

    // The device remembers the delivery.
    let (_, config) = rig.get("/api/push").await;
    assert!(config["devices"][0]["lastOkAt"].is_i64(), "{config}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_test_push_to_an_unknown_device_is_a_502_and_sends_nothing() {
    let rig = Rig::new();
    rig.subscribe().await;
    let (status, body) = rig
        .post("/api/push/test", json!({ "id": "not-a-device" }))
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["ok"], false);
    assert!(rig.requests().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Parking.

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_sent_while_locked_is_parked_then_delivered_and_pushed_after_unlock() {
    let rig = Rig::new();
    rig.subscribe().await;

    // Locked: the send is parked, not typed.
    rig.lock();
    let (status, body) = rig.send(IDLE, PROMPT).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["parked"], true, "{body}");
    assert_eq!(body["queued"]["status"], "waiting", "{body}");
    assert_eq!(body["queued"]["sessionId"], IDLE, "{body}");
    assert_eq!(body["error"], PARKED_ERROR, "{body}");

    // The state lists it under its workspace.
    let listed = rig.parked_in_state().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["workspaceId"], WORKSPACE);
    assert_eq!(listed[0]["sessionId"], IDLE);
    assert_eq!(listed[0]["text"], PROMPT);
    assert_eq!(listed[0]["status"], "waiting");
    assert_eq!(listed[0]["attempts"], 0);

    // Still locked after a few pump polls: nothing typed, nothing pushed.
    some_ticks().await;
    assert_eq!(rig.returns(), 0);
    assert_eq!(rig.rows_with(IDLE, PROMPT), 0);
    assert_eq!(rig.parked_in_state().await.len(), 1);
    assert!(rig.requests().is_empty());

    // Unlocked: the pump delivers it through the real write path, the fake window writes the row.
    rig.unlock();
    eventually("the pump delivered the prompt", || {
        rig.rows_with(IDLE, PROMPT) == 1
    })
    .await;
    eventually("the push of the delivery", || !rig.requests().is_empty()).await;
    assert!(rig.parked_in_state().await.is_empty());
    assert_eq!(rig.returns(), 1);

    // One push, to the one device, readable by its key.
    some_ticks().await;
    let pushes = rig.pushes();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["body"], format!("Sent after unlock: {PROMPT}"));
    assert_eq!(pushes[0]["tag"], format!("parked-{IDLE}"));
    assert_eq!(pushes[0]["kind"], "done");
    assert_eq!(pushes[0]["title"], WORKSPACE_NAME);
    assert_eq!(
        pushes[0]["url"],
        format!("/w/{WORKSPACE}?session={IDLE}"),
        "{pushes:?}"
    );
    assert_eq!(rig.requests()[0].endpoint, ENDPOINT);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_parked_prompt_stays_listed_until_it_is_dismissed() {
    let rig = Rig::new();
    rig.subscribe().await;

    // A window that never shows the workspace: every delivery fails with the Mac unlocked.
    rig.shows.store(false, Ordering::SeqCst);
    rig.lock();
    let (status, body) = rig.send(IDLE, PROMPT).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    rig.unlock();

    eventually("the entry failed", || !rig.requests().is_empty()).await;
    let listed = rig.parked_in_state().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["status"], "failed", "{listed:?}");
    assert_eq!(listed[0]["attempts"], MAX_ATTEMPTS, "{listed:?}");
    assert!(listed[0]["error"].as_str().is_some_and(|e| !e.is_empty()));
    assert_eq!(rig.rows_with(IDLE, PROMPT), 0);
    assert_eq!(rig.returns(), 0);

    // The failure was pushed once, as an error.
    some_ticks().await;
    let pushes = rig.pushes();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["tag"], format!("parked-{IDLE}"));
    assert_eq!(pushes[0]["kind"], "error");
    assert!(
        pushes[0]["body"]
            .as_str()
            .is_some_and(|body| body.starts_with("Parked prompt failed: ")),
        "{pushes:?}"
    );

    // Dismiss: 200, and the state loses it.
    let (status, body) = rig
        .call(
            Method::DELETE,
            &format!("/api/sessions/{IDLE}/prompt"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "ok": true }));
    assert!(rig.parked_in_state().await.is_empty());

    // Nothing left to dismiss.
    let (status, _) = rig
        .call(
            Method::DELETE,
            &format!("/api/sessions/{IDLE}/prompt"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------------------------
// The ticker.

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_that_finishes_a_turn_is_pushed_with_its_last_answer() {
    let rig = Rig::new();
    rig.subscribe().await;
    let conn = rig.test.conn();

    // The ticker baselines the idle chat, then sees it working.
    some_ticks().await;
    set_status(&conn, IDLE, "working");
    some_ticks().await;
    assert!(rig.requests().is_empty());

    // The turn ends: the answer is written, then the chat is idle again.
    say(&conn, "a-1", IDLE, "All tests pass now.");
    set_status(&conn, IDLE, "idle");
    eventually("the push of the finished turn", || {
        !rig.requests().is_empty()
    })
    .await;

    some_ticks().await;
    let pushes = rig.pushes();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["tag"], IDLE);
    assert_eq!(pushes[0]["body"], "All tests pass now.");
    assert_eq!(pushes[0]["kind"], "done");
    assert_eq!(
        pushes[0]["title"],
        format!("{WORKSPACE_NAME} · Idle — {REPO}")
    );
    assert_eq!(pushes[0]["url"], format!("/w/{WORKSPACE}?session={IDLE}"));
}
