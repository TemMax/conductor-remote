mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use conductor_remote::notify::sender::{PushRequest, PushResult, PushSender};
use conductor_remote::notify::service::{
    Notifier, NotifyConfig, DEFAULT_SUBJECT, MAX_FAILURES, TTL_SECS,
};
use conductor_remote::notify::webpush::MAX_PAYLOAD_BYTES;
use conductor_remote::notify::{device_id, DeviceInfo, NotifyService, PushMessage, Subscription};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::{ParkedRow, ParkedStatus, Store};
use hkdf::Hkdf;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{FieldBytes, PublicKey, SecretKey};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::Sha256;
use support::TestDb;

const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// The fake push service.

fn ok() -> PushResult {
    PushResult {
        ok: true,
        status: 201,
        error: None,
        gone: false,
    }
}

fn failed(status: u16, error: &str) -> PushResult {
    PushResult {
        ok: false,
        status,
        error: Some(error.to_owned()),
        gone: matches!(status, 404 | 410),
    }
}

/// Records every request and answers the next scripted result, else `ok`. With a gate, `send`
/// blocks until the gate opens.
#[derive(Default)]
struct FakeSender {
    requests: Mutex<Vec<PushRequest>>,
    script: Mutex<VecDeque<PushResult>>,
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
}

impl FakeSender {
    fn new() -> Arc<FakeSender> {
        Arc::new(FakeSender::default())
    }

    fn gated(gate: Arc<(Mutex<bool>, Condvar)>) -> Arc<FakeSender> {
        Arc::new(FakeSender {
            gate: Some(gate),
            ..FakeSender::default()
        })
    }

    fn answer(&self, result: PushResult) {
        self.script.lock().unwrap().push_back(result);
    }

    fn requests(&self) -> Vec<PushRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl PushSender for FakeSender {
    fn send(&self, request: &PushRequest) -> PushResult {
        self.requests.lock().unwrap().push(request.clone());
        if let Some(gate) = &self.gate {
            let (open, changed) = &**gate;
            let mut open = open.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
        }
        self.script.lock().unwrap().pop_front().unwrap_or_else(ok)
    }
}

fn open_gate(gate: &(Mutex<bool>, Condvar)) {
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
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

    fn subscription(&self, endpoint: &str) -> Subscription {
        Subscription {
            endpoint: endpoint.to_owned(),
            p256dh: URL_SAFE_NO_PAD.encode(self.public()),
            auth: URL_SAFE_NO_PAD.encode(self.auth),
        }
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
// Set-up.

fn config() -> NotifyConfig {
    NotifyConfig {
        enabled: true,
        subject: DEFAULT_SUBJECT.to_owned(),
        tick: Duration::from_secs(2),
        fresh: Duration::from_secs(10),
    }
}

struct Rig {
    test: TestDb,
    store: Arc<Store>,
    reads: Arc<Reads>,
    sender: Arc<FakeSender>,
    notifier: Arc<Notifier>,
}

fn rig_with(sender: Arc<FakeSender>) -> Rig {
    let test = TestDb::new();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    let notifier = Notifier::new(store.clone(), reads.clone(), sender.clone(), config()).unwrap();
    Rig {
        test,
        store,
        reads,
        sender,
        notifier,
    }
}

fn rig() -> Rig {
    rig_with(FakeSender::new())
}

const PHONE: &str = "https://push.example/phone";
const TABLET: &str = "https://push.example/tablet";

impl Rig {
    fn subscribe(&self, ua: &Ua, endpoint: &str, label: &str) -> String {
        self.notifier
            .subscribe(ua.subscription(endpoint), Some(label.to_owned()))
            .unwrap()
            .0
    }

    fn device(&self, id: &str) -> Option<DeviceInfo> {
        self.notifier
            .config()
            .unwrap()
            .devices
            .into_iter()
            .find(|device| device.id == id)
    }
}

fn add_workspace(conn: &Connection, id: &str, name: &str, repo: Option<&str>) {
    if let Some(repo) = repo {
        conn.execute(
            "INSERT INTO repos (id, name) VALUES (?1, ?1)",
            params![repo],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, state, workspace_name) \
         VALUES (?1, ?1, ?2, 'ready', ?3)",
        params![id, repo, name],
    )
    .unwrap();
}

fn add_chat(conn: &Connection, id: &str, workspace: &str, status: &str) {
    conn.execute(
        "INSERT INTO sessions (id, status, title, workspace_id) VALUES (?1, ?2, 'Chat', ?3)",
        params![id, status, workspace],
    )
    .unwrap();
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

/// Polls until `done` holds; panics after `WAIT`.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn message(body: &str) -> PushMessage {
    PushMessage {
        title: "Title".into(),
        body: body.into(),
        tag: "tag".into(),
        url: "/".into(),
        kind: "done".into(),
        ts: 1,
    }
}

/// Workspace `w1` ("Anvil", repo "anvil") with chat `s1` working.
fn one_working_chat(test: &TestDb) -> Connection {
    let conn = test.conn();
    add_workspace(&conn, "w1", "Anvil", Some("anvil"));
    add_chat(&conn, "s1", "w1", "working");
    conn
}

fn parked(workspace_id: &str, session_id: &str, text: &str) -> ParkedRow {
    ParkedRow {
        id: 1,
        workspace_id: workspace_id.into(),
        session_id: session_id.into(),
        text: text.into(),
        queue: false,
        status: ParkedStatus::Waiting,
        attempts: 0,
        created_at_ms: 0,
        reason: "locked".into(),
        error: None,
        cursor_rowid: None,
        cursor_outbox: Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Keys and devices.

#[tokio::test]
async fn the_key_is_made_once_and_survives_a_new_notifier() {
    let rig = rig();
    let stored = rig.store.meta("vapid_private_key").unwrap().unwrap();
    assert_eq!(stored.len(), 64);
    assert!(stored.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    let public_key = rig.notifier.config().unwrap().public_key;
    let point = URL_SAFE_NO_PAD.decode(&public_key).unwrap();
    assert_eq!(point.len(), 65);
    assert_eq!(point[0], 0x04);

    let again = Notifier::new(
        rig.store.clone(),
        rig.reads.clone(),
        rig.sender.clone(),
        config(),
    )
    .unwrap();
    assert_eq!(again.config().unwrap().public_key, public_key);
    assert_eq!(
        rig.store.meta("vapid_private_key").unwrap().unwrap(),
        stored
    );

    // A different store makes a different key.
    let other = Notifier::new(
        Arc::new(Store::open_in_memory().unwrap()),
        rig.reads.clone(),
        rig.sender.clone(),
        config(),
    )
    .unwrap();
    assert_ne!(other.config().unwrap().public_key, public_key);
}

#[tokio::test]
async fn config_reports_enabled_the_key_and_the_safe_half_of_each_device() {
    let rig = rig();
    let empty = rig.notifier.config().unwrap();
    assert!(empty.enabled);
    assert!(empty.devices.is_empty());

    let id = rig.subscribe(&Ua::new(3), PHONE, "Pixel");
    let push = rig.notifier.config().unwrap();
    assert_eq!(push.devices.len(), 1);
    let device = &push.devices[0];
    assert_eq!(device.id, id);
    assert_eq!(device.label, "Pixel");
    assert_eq!(device.failures, 0);
    assert_eq!(device.last_ok_at, None);
    assert_eq!(device.last_error, None);
    assert!(device.created_at > 0);

    let disabled = Notifier::new(
        rig.store.clone(),
        rig.reads.clone(),
        rig.sender.clone(),
        NotifyConfig {
            enabled: false,
            ..config()
        },
    )
    .unwrap();
    assert!(!disabled.config().unwrap().enabled);
}

#[tokio::test]
async fn subscribe_trims_clips_and_defaults_the_label() {
    let rig = rig();
    let ua = Ua::new(3);

    let (id, devices) = rig
        .notifier
        .subscribe(ua.subscription(PHONE), Some("  My Pixel \n".into()))
        .unwrap();
    assert_eq!(id, device_id(PHONE));
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].label, "My Pixel");

    // Re-subscribing without a label keeps the one the device has.
    let (_, devices) = rig
        .notifier
        .subscribe(ua.subscription(PHONE), Some("   ".into()))
        .unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].label, "My Pixel");

    // A new device without a label is a "phone".
    let (tablet, _) = rig
        .notifier
        .subscribe(ua.subscription(TABLET), None)
        .unwrap();
    assert_eq!(rig.device(&tablet).unwrap().label, "phone");

    // At most 64 characters (chars, not bytes).
    let long = "é".repeat(70);
    let (third, _) = rig
        .notifier
        .subscribe(
            ua.subscription("https://push.example/third"),
            Some(long.clone()),
        )
        .unwrap();
    let label = rig.device(&third).unwrap().label;
    assert_eq!(label.chars().count(), 64);
    assert!(long.starts_with(&label));
}

#[tokio::test]
async fn unsubscribe_removes_by_endpoint() {
    let rig = rig();
    let ua = Ua::new(3);
    rig.subscribe(&ua, PHONE, "Phone");
    let tablet = rig.subscribe(&ua, TABLET, "Tablet");

    let (removed, left) = rig.notifier.unsubscribe(PHONE).unwrap();
    assert!(removed);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, tablet);

    let (removed, left) = rig.notifier.unsubscribe(PHONE).unwrap();
    assert!(!removed);
    assert_eq!(left.len(), 1);
}

// ---------------------------------------------------------------------------------------------
// The ticker.

#[tokio::test]
async fn a_turn_that_ends_pushes_once_after_two_ticks() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    let conn = one_working_chat(&rig.test);

    rig.notifier.tick_once().await; // baseline
    set_status(&conn, "s1", "idle");
    say(&conn, "m1", "s1", "All tests\n\npass now.");
    rig.notifier.tick_once().await; // armed
    rig.notifier.tick_once().await; // confirmed
    eventually("the push is delivered", || {
        rig.device(&phone).unwrap().last_ok_at.is_some()
    })
    .await;

    let requests = rig.sender.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.endpoint, PHONE);
    assert_eq!(request.ttl_secs, TTL_SECS);
    let public_key = rig.notifier.config().unwrap().public_key;
    assert!(request.authorization.starts_with("vapid t="));
    assert!(request
        .authorization
        .ends_with(&format!(", k={public_key}")));

    let payload = ua.decrypt(&request.body);
    assert_eq!(payload["title"], "Anvil — anvil");
    assert_eq!(payload["body"], "All tests pass now.");
    assert_eq!(payload["tag"], "s1");
    assert_eq!(payload["url"], "/w/w1?session=s1");
    assert_eq!(payload["kind"], "done");
    assert!(payload["ts"].as_i64().unwrap() > 0);

    // Nothing more on the following ticks.
    rig.notifier.tick_once().await;
    rig.notifier.tick_once().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.sender.count(), 1);
}

#[tokio::test]
async fn a_device_reading_the_chat_gets_no_push() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    let tablet = rig.subscribe(&ua, TABLET, "Tablet");
    let conn = one_working_chat(&rig.test);

    rig.notifier.tick_once().await;
    set_status(&conn, "s1", "idle");
    rig.notifier.tick_once().await;
    rig.notifier.note_viewing(&tablet, "s1");
    rig.notifier.tick_once().await;
    eventually("the phone's push is delivered", || {
        rig.device(&phone).unwrap().last_ok_at.is_some()
    })
    .await;

    let requests = rig.sender.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].endpoint, PHONE);
    assert_eq!(ua.decrypt(&requests[0].body)["body"], "Finished its turn.");
    let tablet = rig.device(&tablet).unwrap();
    assert_eq!(tablet.last_ok_at, None);
    assert_eq!(tablet.failures, 0);
}

/// Replaces the database file: the relay's handle then opens a new connection on its next use,
/// which raises `generation`.
fn replace_database(test: &TestDb) {
    let fresh = TestDb::new();
    std::fs::rename(fresh.path(), test.path()).unwrap();
}

#[tokio::test]
async fn with_no_devices_a_tick_reads_nothing() {
    // Control: with a device, a tick opens the database.
    let rig_with_device = rig();
    rig_with_device.subscribe(&Ua::new(3), PHONE, "Phone");
    rig_with_device.notifier.tick_once().await;
    replace_database(&rig_with_device.test);
    assert_eq!(
        rig_with_device
            .reads
            .db()
            .data_version()
            .unwrap()
            .generation,
        2,
        "the tick opened a connection on the first file"
    );

    // No device: ticks open nothing, so the first connection is the one opened here.
    let rig = rig();
    let conn = one_working_chat(&rig.test);
    rig.notifier.tick_once().await;
    set_status(&conn, "s1", "idle");
    rig.notifier.tick_once().await;
    rig.notifier.tick_once().await;
    drop(conn);
    replace_database(&rig.test);
    assert_eq!(rig.reads.db().data_version().unwrap().generation, 1);
    assert_eq!(rig.sender.count(), 0);
}

#[tokio::test]
async fn unsubscribing_the_last_device_rebaselines_the_watcher() {
    let rig = rig();
    let ua = Ua::new(3);
    rig.subscribe(&ua, PHONE, "Phone");
    let conn = one_working_chat(&rig.test);

    rig.notifier.tick_once().await; // baseline: working
    rig.notifier.unsubscribe(PHONE).unwrap();
    rig.notifier.tick_once().await; // nobody: reset
    set_status(&conn, "s1", "idle");
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    rig.notifier.tick_once().await; // a new baseline: idle is not news
    rig.notifier.tick_once().await;
    rig.notifier.tick_once().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.sender.count(), 0);
    assert_eq!(rig.device(&phone).unwrap().last_ok_at, None);
}

#[tokio::test]
async fn an_unchanged_data_version_reuses_the_cached_states() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    let conn = one_working_chat(&rig.test);

    rig.notifier.tick_once().await; // baseline: working
    set_status(&conn, "s1", "idle");
    rig.notifier.tick_once().await; // armed: idle, at a new data_version

    // Shadow `sessions` with an empty temporary table on the relay's own connection. That
    // commits nothing to the main database, so `data_version` stays as it was; a fresh read
    // would now see no chat at all, and drop the arm.
    let before = rig.reads.db().data_version().unwrap();
    rig.reads
        .db()
        .read("test.shadow", |conn| {
            conn.execute_batch("CREATE TEMP TABLE sessions AS SELECT * FROM main.sessions WHERE 0")
        })
        .unwrap();
    assert_eq!(rig.reads.db().data_version().unwrap(), before);
    assert!(rig.reads.session_states().unwrap().is_empty());

    // The cached states (idle) confirm the turn.
    rig.notifier.tick_once().await;
    eventually("the push is delivered", || {
        rig.device(&phone).unwrap().last_ok_at.is_some()
    })
    .await;
    assert_eq!(rig.sender.count(), 1);
}

#[tokio::test]
async fn a_changed_data_version_reads_the_states_again() {
    let rig = rig();
    let ua = Ua::new(3);
    rig.subscribe(&ua, PHONE, "Phone");
    let conn = one_working_chat(&rig.test);

    rig.notifier.tick_once().await; // baseline: working
    set_status(&conn, "s1", "idle");
    rig.notifier.tick_once().await; // armed
    rig.reads
        .db()
        .read("test.shadow", |conn| {
            conn.execute_batch("CREATE TEMP TABLE sessions AS SELECT * FROM main.sessions WHERE 0")
        })
        .unwrap();
    // Any commit elsewhere changes `data_version`: the tick reads again, sees no chat and
    // drops the arm.
    conn.execute("INSERT INTO repos (id, name) VALUES ('r2', 'other')", [])
        .unwrap();
    rig.notifier.tick_once().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.sender.count(), 0);
}

#[tokio::test]
async fn a_tick_does_not_wait_for_a_push() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let rig = rig_with(FakeSender::gated(gate.clone()));
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    let conn = one_working_chat(&rig.test);

    rig.notifier.tick_once().await;
    set_status(&conn, "s1", "idle");
    rig.notifier.tick_once().await;
    tokio::time::timeout(WAIT, rig.notifier.tick_once())
        .await
        .expect("the tick returns");
    eventually("the push is under way", || rig.sender.count() == 1).await;
    // The push service has not answered, and the next tick runs anyway.
    tokio::time::timeout(WAIT, rig.notifier.tick_once())
        .await
        .expect("the next tick returns");
    assert_eq!(rig.device(&phone).unwrap().last_ok_at, None);

    open_gate(&gate);
    eventually("the push is delivered", || {
        rig.device(&phone).unwrap().last_ok_at.is_some()
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// The fan-out and its bookkeeping.

#[tokio::test]
async fn ok_records_the_delivery() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    rig.sender.answer(failed(500, "HTTP 500: busy"));
    assert_eq!(rig.notifier.notify_all(message("one"), None).await, 0);
    assert_eq!(rig.device(&phone).unwrap().failures, 1);

    assert_eq!(rig.notifier.notify_all(message("two"), None).await, 1);
    let device = rig.device(&phone).unwrap();
    assert!(device.last_ok_at.is_some());
    assert_eq!(device.failures, 0);
    assert_eq!(device.last_error, None);
}

#[tokio::test]
async fn gone_removes_the_device_and_forgets_its_viewing_stamp() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    let tablet = rig.subscribe(&ua, TABLET, "Tablet");
    rig.notifier.note_viewing(&phone, "s1");

    rig.sender.answer(failed(410, "HTTP 410: gone"));
    rig.sender.answer(failed(410, "HTTP 410: gone"));
    assert_eq!(rig.notifier.notify_all(message("hi"), None).await, 0);
    assert!(rig.device(&phone).is_none());
    assert!(rig.device(&tablet).is_none());

    // Subscribed again, the phone is not "reading s1" any more.
    rig.subscribe(&ua, PHONE, "Phone");
    assert_eq!(
        rig.notifier
            .notify_all(message("again"), Some("s1".into()))
            .await,
        1
    );
}

#[tokio::test]
async fn a_failure_is_counted_and_the_twentieth_removes_the_device() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");

    for n in 1..MAX_FAILURES {
        rig.sender.answer(failed(500, "HTTP 500: boom"));
        assert_eq!(rig.notifier.notify_all(message("hi"), None).await, 0);
        let device = rig.device(&phone).expect("still subscribed");
        assert_eq!(device.failures, n);
        assert_eq!(device.last_error.as_deref(), Some("HTTP 500: boom"));
    }
    rig.sender
        .answer(failed(0, "could not reach the push service"));
    assert_eq!(rig.notifier.notify_all(message("hi"), None).await, 0);
    assert!(rig.device(&phone).is_none());
    assert_eq!(rig.sender.count(), MAX_FAILURES as usize);
}

#[tokio::test]
async fn a_failure_without_text_records_the_status() {
    let rig = rig();
    let phone = rig.subscribe(&Ua::new(3), PHONE, "Phone");
    rig.sender.answer(PushResult {
        ok: false,
        status: 429,
        error: None,
        gone: false,
    });
    rig.notifier.notify_all(message("hi"), None).await;
    assert_eq!(
        rig.device(&phone).unwrap().last_error.as_deref(),
        Some("HTTP 429")
    );
}

#[tokio::test]
async fn every_device_gets_its_own_encryption_and_the_count_is_returned() {
    let rig = rig();
    let phone_ua = Ua::new(3);
    let tablet_ua = Ua::new(5);
    rig.subscribe(&phone_ua, PHONE, "Phone");
    rig.subscribe(&tablet_ua, TABLET, "Tablet");
    assert_eq!(rig.notifier.notify_all(message("both"), None).await, 2);

    for request in rig.sender.requests() {
        let ua = if request.endpoint == PHONE {
            &phone_ua
        } else {
            &tablet_ua
        };
        assert_eq!(ua.decrypt(&request.body)["body"], "both");
    }
}

#[tokio::test]
async fn an_oversized_body_is_clipped_to_fit() {
    let rig = rig();
    let ua = Ua::new(3);
    rig.subscribe(&ua, PHONE, "Phone");
    let long = "word ".repeat(1200);
    assert_eq!(rig.notifier.notify_all(message(&long), None).await, 1);

    let request = &rig.sender.requests()[0];
    let payload = ua.decrypt(&request.body);
    assert!(serde_json::to_vec(&payload).unwrap().len() <= MAX_PAYLOAD_BYTES);
    let body = payload["body"].as_str().unwrap();
    assert!(body.ends_with('…'));
    assert!(body.len() > 3000, "clipped only as much as needed");
    assert_eq!(payload["title"], "Title");
}

#[tokio::test]
async fn a_huge_title_still_fits() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    let conn = rig.test.conn();
    add_workspace(&conn, "w1", &"W".repeat(5000), Some("anvil"));
    add_chat(&conn, "s1", "w1", "working");

    rig.notifier.tick_once().await; // baseline
    set_status(&conn, "s1", "idle");
    say(&conn, "m1", "s1", "All tests pass now.");
    rig.notifier.tick_once().await; // armed
    rig.notifier.tick_once().await; // confirmed
    eventually("the push is delivered", || {
        rig.device(&phone).unwrap().last_ok_at.is_some()
    })
    .await;

    let requests = rig.sender.requests();
    assert_eq!(requests.len(), 1);
    let payload = ua.decrypt(&requests[0].body);
    let size = serde_json::to_vec(&payload).unwrap().len();
    eprintln!("huge title payload: {size} bytes (limit {MAX_PAYLOAD_BYTES})");
    assert!(size <= MAX_PAYLOAD_BYTES);
    let title = payload["title"].as_str().unwrap();
    assert!(!title.is_empty());
    assert!(title.ends_with('…'));
    assert_eq!(payload["tag"], "s1");
    assert_eq!(payload["url"], "/w/w1?session=s1");
    assert_eq!(rig.device(&phone).unwrap().failures, 0);
}

#[tokio::test]
async fn a_short_title_is_untouched_by_the_clipping() {
    let rig = rig();
    let ua = Ua::new(3);
    rig.subscribe(&ua, PHONE, "Phone");
    let long = "word ".repeat(1200);
    assert_eq!(rig.notifier.notify_all(message(&long), None).await, 1);
    let payload = ua.decrypt(&rig.sender.requests()[0].body);
    assert_eq!(payload["title"], "Title");
}

// ---------------------------------------------------------------------------------------------
// The test notification.

#[tokio::test]
async fn test_refuses_an_unknown_device() {
    let rig = rig();
    let result = rig.notifier.test("0123456789abcdef".into()).await;
    assert_eq!(result, Err("this device is not subscribed".to_owned()));
    assert_eq!(rig.sender.count(), 0);
}

#[tokio::test]
async fn test_sends_to_that_device_alone() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    rig.subscribe(&ua, TABLET, "Tablet");

    assert_eq!(rig.notifier.test(phone.clone()).await, Ok(()));
    let requests = rig.sender.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].endpoint, PHONE);
    let payload = ua.decrypt(&requests[0].body);
    assert_eq!(payload["title"], "Conductor Remote");
    assert_eq!(
        payload["body"],
        "Notifications are working. You’ll get one when an agent finishes."
    );
    assert_eq!(payload["tag"], "test");
    assert_eq!(payload["url"], "/");
    assert_eq!(payload["kind"], "test");
    assert!(payload["ts"].as_i64().unwrap() > 0);
    assert!(rig.device(&phone).unwrap().last_ok_at.is_some());

    rig.sender.answer(failed(500, "HTTP 500: nope"));
    assert_eq!(
        rig.notifier.test(phone.clone()).await,
        Err("HTTP 500: nope".to_owned())
    );
    assert_eq!(rig.device(&phone).unwrap().failures, 1);
}

#[tokio::test]
async fn test_and_parked_notices_still_send_when_disabled() {
    let test = TestDb::new();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    let sender = FakeSender::new();
    let notifier = Notifier::new(
        store,
        reads,
        sender.clone(),
        NotifyConfig {
            enabled: false,
            ..config()
        },
    )
    .unwrap();
    let ua = Ua::new(3);
    let (phone, _) = notifier.subscribe(ua.subscription(PHONE), None).unwrap();
    assert_eq!(notifier.test(phone).await, Ok(()));
    notifier.notify_parked(&parked("w1", "s1", "hello"), None);
    eventually("the parked notice is sent", || sender.count() == 2).await;
}

// ---------------------------------------------------------------------------------------------
// The parked-prompt notice.

#[tokio::test]
async fn notify_parked_says_the_prompt_was_sent() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    one_working_chat(&rig.test);

    rig.notifier
        .notify_parked(&parked("w1", "s1", "Run the tests again"), None);
    eventually("the notice is delivered", || {
        rig.device(&phone).unwrap().last_ok_at.is_some()
    })
    .await;
    let requests = rig.sender.requests();
    assert_eq!(requests.len(), 1);
    let payload = ua.decrypt(&requests[0].body);
    assert_eq!(payload["title"], "Anvil");
    assert_eq!(payload["body"], "Sent after unlock: Run the tests again");
    assert_eq!(payload["tag"], "parked-s1");
    assert_eq!(payload["url"], "/w/w1?session=s1");
    assert_eq!(payload["kind"], "done");
}

#[tokio::test]
async fn notify_parked_says_the_prompt_failed() {
    let rig = rig();
    let ua = Ua::new(3);
    let phone = rig.subscribe(&ua, PHONE, "Phone");
    // A reader of this chat still gets it: the notice is not about what is on screen.
    rig.notifier.note_viewing(&phone, "s1");

    rig.notifier.notify_parked(
        &parked("w-gone", "s1", "Run the tests again"),
        Some("Conductor is not running"),
    );
    eventually("the notice is delivered", || {
        rig.device(&phone).unwrap().last_ok_at.is_some()
    })
    .await;
    let payload = ua.decrypt(&rig.sender.requests()[0].body);
    assert_eq!(payload["title"], "Conductor");
    assert_eq!(
        payload["body"],
        "Parked prompt failed: Conductor is not running"
    );
    assert_eq!(payload["tag"], "parked-s1");
    assert_eq!(payload["url"], "/w/w-gone?session=s1");
    assert_eq!(payload["kind"], "error");
}

#[tokio::test]
async fn notify_parked_returns_before_the_push() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let rig = rig_with(FakeSender::gated(gate.clone()));
    let phone = rig.subscribe(&Ua::new(3), PHONE, "Phone");

    rig.notifier
        .notify_parked(&parked("w1", "s1", "hello"), None);
    eventually("the push is under way", || rig.sender.count() == 1).await;
    assert_eq!(rig.device(&phone).unwrap().last_ok_at, None);
    open_gate(&gate);
    eventually("the notice is delivered", || {
        rig.device(&phone).unwrap().last_ok_at.is_some()
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// The environment. The only test that touches these variables.

#[test]
fn from_env_reads_push_notify_and_push_subject() {
    std::env::remove_var("PUSH_NOTIFY");
    std::env::remove_var("PUSH_SUBJECT");
    let default = NotifyConfig::from_env();
    assert!(default.enabled);
    assert_eq!(default.subject, DEFAULT_SUBJECT);
    assert_eq!(default.tick, Duration::from_secs(2));
    assert_eq!(default.fresh, Duration::from_secs(10));

    for off in ["off", "OFF", " Off\n", "false", "FALSE", "0", " 0 "] {
        std::env::set_var("PUSH_NOTIFY", off);
        assert!(!NotifyConfig::from_env().enabled, "{off:?} turns it off");
    }
    for on in ["", "on", "1", "true", "yes", "offline", "00"] {
        std::env::set_var("PUSH_NOTIFY", on);
        assert!(NotifyConfig::from_env().enabled, "{on:?} leaves it on");
    }
    std::env::remove_var("PUSH_NOTIFY");

    std::env::set_var("PUSH_SUBJECT", "mailto:relay@example.invalid");
    assert_eq!(
        NotifyConfig::from_env().subject,
        "mailto:relay@example.invalid"
    );
    std::env::set_var("PUSH_SUBJECT", "");
    assert_eq!(NotifyConfig::from_env().subject, DEFAULT_SUBJECT);
    std::env::remove_var("PUSH_SUBJECT");
}
