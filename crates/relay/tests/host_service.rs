//! The wired host over fakes: the log ring and the log files, the settings, keep-awake, and the
//! restart's refusals. Nothing here spawns caffeinate or touches Conductor.

mod support;

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::contract::Priority;
use conductor_remote::delivery::WriteAnswer;
use conductor_remote::host::logbuf::{self, LogBuffer, Redactor};
use conductor_remote::host::nosleep::{Child, NoSleep, Spawner};
use conductor_remote::host::restart::{AppControl, RestartTimings};
use conductor_remote::host::service::{Host, HostParts, Supervisor};
use conductor_remote::host::HostService;
use conductor_remote::reads::Reads;
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle, UiRunError, MAX_WAITING};
use conductor_remote::ui::fake::{conductor_app, FakeDesktop, WindowSpec};
use serde_json::{json, Value};
use support::TestDb;
use tracing_subscriber::layer::SubscriberExt;

const TOKEN: &str = "tok-0123456789abcdef";
const NOW: i64 = 1_700_000_000_000;

/// A keep-awake process that runs until killed.
struct FakeChild;

impl Child for FakeChild {
    fn id(&self) -> u32 {
        4242
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(None)
    }

    fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Starts a [`FakeChild`], or fails when `fails`.
struct FakeSpawner {
    fails: bool,
}

impl Spawner for FakeSpawner {
    fn available(&self) -> bool {
        true
    }

    fn spawn(&self, _args: &[String]) -> io::Result<Box<dyn Child>> {
        if self.fails {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no caffeinate here",
            ))
        } else {
            Ok(Box::new(FakeChild))
        }
    }
}

/// A Conductor that records every call; the refusals tested here must make none. It is running
/// until asked to quit, and shows a window as soon as it is looked at.
#[derive(Default)]
struct FakeApp {
    calls: Mutex<Vec<&'static str>>,
    quit: AtomicBool,
}

impl FakeApp {
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    /// Writes `what` into the list of calls, to place something other than a call in it.
    fn note(&self, what: &'static str) {
        self.calls.lock().unwrap().push(what);
    }
}

impl AppControl for FakeApp {
    fn running(&self) -> bool {
        self.calls.lock().unwrap().push("running");
        !self.quit.load(Ordering::SeqCst)
    }

    fn terminate(&self) -> bool {
        self.calls.lock().unwrap().push("terminate");
        self.quit.store(true, Ordering::SeqCst);
        true
    }

    fn launch(&self) -> Result<(), String> {
        self.calls.lock().unwrap().push("launch");
        self.quit.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn has_window(&self) -> bool {
        self.calls.lock().unwrap().push("has_window");
        true
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    log_dir: std::path::PathBuf,
    logs: LogBuffer,
    redactor: Redactor,
    app: Arc<FakeApp>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let log_dir = dir.path().join("Logs");
    std::fs::create_dir(&log_dir).unwrap();
    let redactor = Redactor::new();
    redactor.set_token(TOKEN);
    Fixture {
        _dir: dir,
        log_dir,
        logs: LogBuffer::new(),
        redactor,
        app: Arc::new(FakeApp::default()),
    }
}

/// The host over the fixture; `spawn_fails` makes caffeinate fail to start.
fn host(fx: &Fixture, spawn_fails: bool, reads: Option<Arc<Reads>>, screen: Option<bool>) -> Host {
    build(fx, spawn_fails, reads, screen, None)
}

fn build(
    fx: &Fixture,
    spawn_fails: bool,
    reads: Option<Arc<Reads>>,
    screen: Option<bool>,
    ui: Option<UiHandle>,
) -> Host {
    Host::new(HostParts {
        logs: fx.logs.clone(),
        redactor: fx.redactor.clone(),
        log_dir: fx.log_dir.clone(),
        nosleep: NoSleep::new(
            Arc::new(FakeSpawner { fails: spawn_fails }),
            false,
            Arc::new(|| NOW),
        ),
        app: fx.app.clone(),
        reads,
        screen: Arc::new(move || screen),
        managed: true,
        trusted: Arc::new(|| true),
        supervisor: Supervisor::Launchd,
        port: 8790,
        restart_timings: RestartTimings::default(),
        ui,
    })
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

fn texts(answer: &WriteAnswer) -> Vec<&str> {
    answer.body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["text"].as_str().unwrap())
        .collect()
}

#[test]
fn live_logs_come_from_the_ring_redacted_by_the_layer() {
    let fx = fixture();
    let subscriber =
        tracing_subscriber::registry().with(logbuf::layer(fx.logs.clone(), fx.redactor.clone()));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("open https://mac.example.ts.net/#token={TOKEN}");
        tracing::warn!("the token is {TOKEN}");
        tracing::error!("it failed");
    });
    let host = host(&fx, false, None, None);

    let answer = host.logs(None, Some(2));
    assert_eq!(answer.status, 200);
    let body = &answer.body;
    assert_eq!(body["source"], "live");
    assert_eq!(body["managed"], true);
    assert_eq!(body["startedAt"], fx.logs.started_at());
    assert!(body["now"].as_i64().unwrap() >= fx.logs.started_at());
    assert_eq!(body["files"], json!([]));
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["level"], "warn");
    assert_eq!(entries[0]["text"], "the token is [redacted]");
    assert_eq!(entries[1]["level"], "error");
    assert!(entries[1]["t"].is_i64());

    let all = host.logs(None, None);
    assert_eq!(
        texts(&all)[0],
        "open https://mac.example.ts.net/#token=[redacted]"
    );
    assert!(!all.body.to_string().contains(TOKEN));
}

#[test]
fn a_log_file_is_tailed_and_redacted() {
    let fx = fixture();
    write(
        &fx.log_dir.join("relay.log"),
        &format!("first\nphone URL: https://mac/#token={TOKEN}\nkey sk-abcdefghijkl\nlast\n"),
    );
    write(&fx.log_dir.join("relay.err.log"), "panicked\n");
    let host = host(&fx, false, None, None);

    let answer = host.logs(Some("relay.log".into()), Some(3));
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["source"], "relay.log");
    assert_eq!(answer.body["managed"], true);
    assert_eq!(
        answer.body["entries"],
        json!([
            { "t": null, "level": "info", "text": "phone URL: https://mac/#token=[redacted]" },
            { "t": null, "level": "info", "text": "key [redacted]" },
            { "t": null, "level": "info", "text": "last" },
        ])
    );

    let errors = host.logs(Some("relay.err.log".into()), Some(10));
    assert_eq!(errors.body["source"], "relay.err.log");
    assert_eq!(
        errors.body["entries"],
        json!([{ "t": null, "level": "error", "text": "panicked" }])
    );
}

#[test]
fn a_missing_log_file_has_no_entries() {
    let fx = fixture();
    let answer = host(&fx, false, None, None).logs(Some("relay.err.log".into()), Some(10));
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["entries"], json!([]));
}

#[test]
fn the_files_list_names_each_existing_log_with_its_size_and_time() {
    let fx = fixture();
    write(&fx.log_dir.join("relay.log"), "12345\n");
    write(&fx.log_dir.join("relay.err.log"), "");
    let answer = host(&fx, false, None, None).logs(None, Some(10));
    let files = answer.body["files"].as_array().unwrap();
    let summary: Vec<(&str, u64)> = files
        .iter()
        .map(|file| {
            (
                file["name"].as_str().unwrap(),
                file["size"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(summary, [("relay.log", 6), ("relay.err.log", 0)]);
    assert!(files
        .iter()
        .all(|file| file["modifiedAt"].as_i64().unwrap() > 0));
}

#[test]
fn any_other_log_name_is_a_404() {
    let fx = fixture();
    write(&fx._dir.path().join("token"), "not a log");
    let host = host(&fx, false, None, None);
    for name in ["other.log", "../token", "", "relay.log/"] {
        let answer = host.logs(Some(name.into()), Some(10));
        assert_eq!(answer.status, 404, "{name}");
        assert_eq!(answer.body, json!({ "error": "no such log" }), "{name}");
    }
}

#[test]
fn settings_carry_the_nosleep_state_and_the_screen() {
    let fx = fixture();
    let answer = host(&fx, false, None, Some(true)).settings();
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.body,
        json!({
            "settings": {},
            "nosleep": {
                "available": true,
                "armed": false,
                "until": null,
                "pid": null,
                "preventsScreenLock": false,
                "maxSeconds": 604800,
            },
            "screenLocked": true,
        })
    );
    let unknown = host(&fx, false, None, None).settings();
    assert_eq!(unknown.body["screenLocked"], Value::Null);
}

#[test]
fn arming_reports_the_window() {
    let fx = fixture();
    let host = host(&fx, false, None, None);
    let armed = host.arm_nosleep(60);
    assert_eq!(armed.status, 200);
    assert_eq!(armed.body["ok"], true);
    assert_eq!(armed.body["state"]["armed"], true);
    assert_eq!(armed.body["state"]["until"], NOW + 60_000);
    assert_eq!(armed.body["state"]["pid"], 4242);
    assert_eq!(host.nosleep().body["armed"], true);

    let disarmed = host.disarm_nosleep();
    assert_eq!(disarmed.status, 200);
    assert_eq!(disarmed.body["ok"], true);
    assert_eq!(disarmed.body["state"]["armed"], false);
}

#[test]
fn an_arm_error_is_a_400_with_the_state() {
    let fx = fixture();
    let failing = host(&fx, true, None, None).arm_nosleep(60);
    assert_eq!(failing.status, 400);
    assert_eq!(failing.body["ok"], false);
    assert_eq!(
        failing.body["error"],
        "could not start caffeinate: no caffeinate here"
    );
    assert_eq!(failing.body["state"]["armed"], false);

    let too_long = host(&fx, false, None, None).arm_nosleep(8 * 24 * 3600);
    assert_eq!(too_long.status, 400);
    assert_eq!(
        too_long.body["error"],
        "seconds must be between 1 and 604800"
    );
}

#[tokio::test]
async fn a_locked_screen_refuses_the_restart() {
    let fx = fixture();
    let answer = host(&fx, false, None, Some(true))
        .restart_conductor(true)
        .await;
    assert_eq!(answer.status, 409);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(
        answer.body["error"],
        "The Mac is locked - unlock it before restarting Conductor."
    );
    assert!(fx.app.calls().is_empty(), "{:?}", fx.app.calls());
}

/// Two live workspaces with three working chats between them, one idle chat, and a working chat
/// in an archived workspace (not counted).
fn reads_with_working_chats() -> (TestDb, Arc<Reads>) {
    let test = TestDb::new();
    let conn = test.conn();
    for (id, state) in [
        ("ws-a", "ready"),
        ("ws-b", "setting_up"),
        ("ws-gone", "archived"),
    ] {
        conn.execute(
            "INSERT INTO workspaces (local_id, id, directory_name, state) VALUES (?1, ?1, ?1, ?2)",
            [id, state],
        )
        .unwrap();
    }
    for (id, workspace, status) in [
        ("chat-1", "ws-a", "working"),
        ("chat-2", "ws-a", "working"),
        ("chat-3", "ws-a", "idle"),
        ("chat-4", "ws-b", "working"),
        ("chat-5", "ws-gone", "working"),
    ] {
        conn.execute(
            "INSERT INTO sessions (id, status, workspace_id, is_hidden, agent_type) \
             VALUES (?1, ?2, ?3, 0, 'claude')",
            [id, status, workspace],
        )
        .unwrap();
    }
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    (test, reads)
}

#[test]
fn the_status_reports_trust_and_activity() {
    let fx = fixture();
    let (_test, reads) = reads_with_working_chats();
    let trusted = Arc::new(AtomicBool::new(false));
    let flag = trusted.clone();
    let host = Host::new(HostParts {
        logs: fx.logs.clone(),
        redactor: fx.redactor.clone(),
        log_dir: fx.log_dir.clone(),
        nosleep: NoSleep::new(
            Arc::new(FakeSpawner { fails: false }),
            false,
            Arc::new(|| NOW),
        ),
        app: fx.app.clone(),
        reads: Some(reads),
        screen: Arc::new(|| Some(false)),
        managed: false,
        trusted: Arc::new(move || flag.load(Ordering::SeqCst)),
        supervisor: Supervisor::App,
        port: 4242,
        restart_timings: RestartTimings::default(),
        ui: None,
    });

    let first = host.status();
    assert_eq!(first.status, 200);
    println!("{}", first.body);
    let mut keys: Vec<&str> = first
        .body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "accessibility",
            "activity",
            "conductor",
            "pid",
            "port",
            "screenLocked",
            "startedAt",
            "supervisor",
            "version"
        ]
    );
    assert_eq!(first.body["version"], conductor_remote::contract::VERSION);
    assert_eq!(first.body["pid"], std::process::id());
    assert_eq!(first.body["startedAt"], fx.logs.started_at());
    assert_eq!(first.body["supervisor"], "app");
    assert_eq!(first.body["port"], 4242);
    assert_eq!(first.body["conductor"], json!({ "running": true }));
    assert_eq!(first.body["screenLocked"], false);
    assert_eq!(first.body["accessibility"], json!({ "trusted": false }));
    assert_eq!(first.body["activity"]["working"], 3);
    assert_eq!(first.body["activity"]["idleMs"], Value::Null);

    trusted.store(true, Ordering::SeqCst);
    host.note_request();
    let second = host.status();
    assert_eq!(second.body["accessibility"], json!({ "trusted": true }));
    let idle = second.body["activity"]["idleMs"].as_u64().unwrap();
    assert!(idle < 5_000, "{idle}");
}

#[tokio::test]
async fn working_chats_refuse_the_restart() {
    let fx = fixture();
    let (_test, reads) = reads_with_working_chats();
    let answer = host(&fx, false, Some(reads), Some(false))
        .restart_conductor(false)
        .await;
    assert_eq!(answer.status, 409);
    assert_eq!(
        answer.body,
        json!({
            "ok": false,
            "agentsRunning": true,
            "working": 3,
            "error": "3 chats are mid-turn. Restarting Conductor ends them.",
        })
    );
    assert!(fx.app.calls().is_empty(), "{:?}", fx.app.calls());
}

/// A UI thread over a fake desktop; the restart's jobs never touch the driver.
fn fake_ui() -> UiHandle {
    UiActor::spawn(|| {
        let spec = WindowSpec {
            repo: "relay".to_owned(),
            branch: "user/feature-x".to_owned(),
            sidebar: Vec::new(),
            chats: Vec::new(),
            selected: 0,
            composer_value: None,
        };
        Box::new(Driver::new(FakeDesktop::new(conductor_app(&spec))))
    })
}

/// Occupies the UI thread with a job that runs until the returned sender is dropped or used, and
/// waits until that job runs.
async fn block(ui: &UiHandle) -> (std::sync::mpsc::Sender<()>, BlockedJob) {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let done = ui.run(Priority::Interactive, move |_driver| {
        let _ = started_tx.send(());
        let _ = release_rx.recv();
    });
    started_rx.await.expect("the blocking job started");
    (release_tx, Box::pin(done))
}

type BlockedJob =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), UiRunError>> + Send + 'static>>;

/// Waits until `count` jobs wait behind the running one.
async fn until_waiting(ui: &UiHandle, count: usize) {
    for _ in 0..400 {
        if ui.waiting() == count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{} jobs wait, not {count}", ui.waiting());
}

#[tokio::test]
async fn a_restart_waits_for_the_ui_thread_and_holds_it() {
    let fx = fixture();
    let ui = fake_ui();
    let host = build(&fx, false, None, Some(false), Some(ui.clone()));
    let (release, blocked) = block(&ui).await;

    let restart = tokio::spawn(host.restart_conductor(false));
    until_waiting(&ui, 1).await;
    // A job queued after the restart's hold.
    let app = fx.app.clone();
    let later = ui.run(Priority::Interactive, move |_driver| app.note("ui job"));
    until_waiting(&ui, 2).await;

    // The send job is still running: Conductor is not touched.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(fx.app.calls().is_empty(), "{:?}", fx.app.calls());
    assert!(!restart.is_finished());

    drop(release);
    blocked.await.expect("the blocking job ended");
    let answer = restart.await.expect("the restart task");
    assert_eq!(answer.status, 200, "{:?}", answer.body);
    assert_eq!(answer.body["ok"], true);
    later.await.expect("the later job ran");
    assert_eq!(
        fx.app.calls(),
        [
            "running",
            "terminate",
            "running",
            "launch",
            "has_window",
            "ui job"
        ]
    );
}

#[tokio::test]
async fn a_second_restart_while_one_runs_is_refused() {
    let fx = fixture();
    let ui = fake_ui();
    let host = build(&fx, false, None, Some(false), Some(ui.clone()));
    let (release, blocked) = block(&ui).await;

    let first = tokio::spawn(host.restart_conductor(false));
    let second = host.restart_conductor(true).await;
    assert_eq!(second.status, 409);
    assert_eq!(
        second.body,
        json!({ "ok": false, "error": "Conductor is already restarting." })
    );
    assert!(fx.app.calls().is_empty(), "{:?}", fx.app.calls());

    drop(release);
    blocked.await.expect("the blocking job ended");
    assert_eq!(first.await.expect("the first restart").status, 200);

    let calls = fx.app.calls().len();
    assert!(calls > 0);
    let third = host.restart_conductor(false).await;
    assert_eq!(third.status, 200, "{:?}", third.body);
    assert!(fx.app.calls().len() > calls);

    // A restart whose request goes away frees the next one as well.
    drop(host.restart_conductor(false));
    assert_eq!(host.restart_conductor(false).await.status, 200);
}

#[tokio::test]
async fn a_busy_ui_thread_refuses_the_restart() {
    let fx = fixture();
    let ui = fake_ui();
    let host = build(&fx, false, None, Some(false), Some(ui.clone()));
    let (release, blocked) = block(&ui).await;
    let queued: Vec<_> = (0..MAX_WAITING)
        .map(|_| ui.run(Priority::Interactive, |_driver| ()))
        .collect();

    let answer = host.restart_conductor(false).await;
    assert_eq!(answer.status, 503);
    assert_eq!(
        answer.body,
        json!({
            "ok": false,
            "error": UiRunError::Busy { waiting: MAX_WAITING }.to_string(),
        })
    );
    assert!(fx.app.calls().is_empty(), "{:?}", fx.app.calls());

    drop(release);
    blocked.await.expect("the blocking job ended");
    for job in queued {
        job.await.expect("a queued job ran");
    }
    let later = host.restart_conductor(false).await;
    assert_eq!(later.status, 200, "{:?}", later.body);
    assert!(!fx.app.calls().is_empty());
}
