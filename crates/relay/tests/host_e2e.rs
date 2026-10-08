//! The host routes end to end: a real server and the real `Host`, `LogBuffer` and `NoSleep` over a
//! fake spawner and a fake `AppControl`, reached over HTTP with the bearer token. Nothing here
//! runs caffeinate or touches Conductor.

#[allow(dead_code)]
#[path = "support/seed_sessions.rs"]
mod seed_sessions;
mod support;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::app;
use conductor_remote::contract::{AppState, ConductorStatus, Services, Token};
use conductor_remote::host::logbuf::{self, LogBuffer, Redactor};
use conductor_remote::host::nosleep::{Child, NoSleep, Spawner};
use conductor_remote::host::restart::{AppControl, RestartTimings};
use conductor_remote::host::service::{Host, HostParts, Supervisor};
use conductor_remote::host::HostService;
use conductor_remote::reads::Reads;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use serde_json::{json, Value};
use support::TestDb;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

const TOKEN: &str = "e2e-host-token-0123456789";
const NOW: i64 = 1_700_000_000_000;
const PID: u32 = 4242;

/// A keep-awake process that runs until it is killed.
struct FakeChild {
    killed: Arc<AtomicBool>,
}

impl Child for FakeChild {
    fn id(&self) -> u32 {
        PID
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.killed.load(Ordering::SeqCst).then_some(0))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Records the arguments of every spawn and whether its child was killed.
#[derive(Default)]
struct FakeSpawner {
    spawns: Mutex<Vec<Vec<String>>>,
    children: Mutex<Vec<Arc<AtomicBool>>>,
}

impl FakeSpawner {
    fn spawns(&self) -> Vec<Vec<String>> {
        self.spawns.lock().unwrap().clone()
    }

    fn killed(&self) -> Vec<bool> {
        self.children
            .lock()
            .unwrap()
            .iter()
            .map(|killed| killed.load(Ordering::SeqCst))
            .collect()
    }
}

impl Spawner for FakeSpawner {
    fn available(&self) -> bool {
        true
    }

    fn spawn(&self, args: &[String]) -> io::Result<Box<dyn Child>> {
        let killed = Arc::new(AtomicBool::new(false));
        self.spawns.lock().unwrap().push(args.to_vec());
        self.children.lock().unwrap().push(killed.clone());
        Ok(Box::new(FakeChild { killed }))
    }
}

/// A Conductor that is running at first, quits when asked, comes back when launched and then shows
/// a window; it records every call.
struct FakeApp {
    state: Mutex<Inner>,
}

struct Inner {
    running: bool,
    calls: Vec<&'static str>,
}

impl FakeApp {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Inner {
                running: true,
                calls: Vec::new(),
            }),
        })
    }

    fn calls(&self) -> Vec<&'static str> {
        self.state.lock().unwrap().calls.clone()
    }

    fn count(&self, name: &str) -> usize {
        self.calls().iter().filter(|call| **call == name).count()
    }
}

impl AppControl for FakeApp {
    fn running(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        state.calls.push("running");
        state.running
    }

    fn terminate(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        state.calls.push("terminate");
        state.running = false;
        true
    }

    fn launch(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        state.calls.push("launch");
        state.running = true;
        Ok(())
    }

    fn has_window(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        state.calls.push("has_window");
        state.running
    }
}

/// A running relay serving the real host over fakes.
struct Relay {
    base: String,
    client: reqwest::Client,
    spawner: Arc<FakeSpawner>,
    app: Arc<FakeApp>,
    redactor: Redactor,
    logs: LogBuffer,
    _test: TestDb,
    _stop: oneshot::Sender<()>,
    _server: tokio::task::JoinHandle<io::Result<()>>,
}

impl Relay {
    /// The server over a database seeded with the invented chats of `seed_sessions`, two of which
    /// are mid-turn. The log directory is empty.
    async fn start() -> Self {
        let test = TestDb::new();
        seed_sessions::seed(&test.conn());
        let reads = Arc::new(Reads::new(test.db(), test.root()));

        let logs = LogBuffer::new();
        let redactor = Redactor::new();
        redactor.set_token(TOKEN);
        let spawner = Arc::new(FakeSpawner::default());
        let app = FakeApp::new();
        let log_dir = test.dir().join("Logs");
        std::fs::create_dir(&log_dir).unwrap();

        let host = Host::new(HostParts {
            logs: logs.clone(),
            redactor: redactor.clone(),
            log_dir,
            nosleep: NoSleep::new(spawner.clone(), false, Arc::new(|| NOW)),
            app: app.clone(),
            reads: Some(reads),
            screen: Arc::new(|| Some(false)),
            managed: false,
            trusted: Arc::new(|| true),
            supervisor: Supervisor::None,
            port: 0,
            restart_timings: RestartTimings {
                quit_wait: Duration::from_secs(5),
                window_wait: Duration::from_secs(5),
                poll: Duration::from_millis(5),
            },
            ui: None,
        });

        let state = AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
            assets: Arc::new(MemoryAssets::default()),
            reads: None,
            writes: None,
            notify: None,
            services: Services {
                host: Some(Arc::new(host) as Arc<dyn HostService>),
                ..Default::default()
            },
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
            spawner,
            app,
            redactor,
            logs,
            _test: test,
            _stop: stop,
            _server: server,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }

    /// Sends the request and returns the status and the body text.
    async fn send(&self, request: reqwest::RequestBuilder) -> (u16, String) {
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.text().await.unwrap())
    }

    /// Sends the request and returns the status and the body as JSON.
    async fn send_json(&self, request: reqwest::RequestBuilder) -> (u16, Value) {
        let (status, text) = self.send(request).await;
        (status, serde_json::from_str(&text).expect(&text))
    }

    async fn status(&self) -> Value {
        let (status, body) = self
            .send_json(self.request(reqwest::Method::GET, "/api/host/status"))
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn settings(&self) -> Value {
        let (status, body) = self
            .send_json(self.request(reqwest::Method::GET, "/api/settings"))
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }
}

#[tokio::test]
async fn a_logged_line_with_the_token_reads_redacted() {
    let relay = Relay::start().await;
    // The layer is scoped to this test; the global default stays untouched. The HTTP client logs
    // on this thread too, so only this test's own events are kept.
    let own = filter_fn(|metadata| metadata.target().starts_with("host_e2e"));
    let subscriber = tracing_subscriber::registry()
        .with(logbuf::layer(relay.logs.clone(), relay.redactor.clone()).with_filter(own));
    let _guard = tracing::subscriber::set_default(subscriber);

    tracing::info!("phone opened with access key {TOKEN} from the lan");
    tracing::warn!("second line without any secret");

    let (status, text) = relay
        .send(relay.request(reqwest::Method::GET, "/api/logs"))
        .await;
    assert_eq!(status, 200, "{text}");
    assert!(!text.contains(TOKEN), "the token leaked: {text}");

    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["source"], "live");
    assert_eq!(body["managed"], false);
    assert_eq!(body["files"], json!([]));
    let entries = body["entries"].as_array().unwrap();
    let texts: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                entry["level"].as_str().unwrap(),
                entry["text"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            (
                "info",
                "phone opened with access key [redacted] from the lan"
            ),
            ("warn", "second line without any secret"),
        ]
    );
}

#[tokio::test]
async fn nosleep_arms_through_the_settings_and_disarms() {
    let relay = Relay::start().await;

    // Off to begin with, and nothing spawned.
    let settings = relay.settings().await;
    assert_eq!(settings["nosleep"]["armed"], false);
    assert_eq!(settings["nosleep"]["available"], true);
    assert_eq!(settings["nosleep"]["until"], Value::Null);
    assert_eq!(settings["nosleep"]["pid"], Value::Null);
    assert_eq!(settings["screenLocked"], false);
    assert!(relay.spawner.spawns().is_empty());

    // POST arms it for an hour; the spawner saw the caffeinate arguments.
    let (status, armed) = relay
        .send_json(
            relay
                .request(reqwest::Method::POST, "/api/nosleep")
                .json(&json!({ "seconds": 3600 })),
        )
        .await;
    assert_eq!(status, 200, "{armed}");
    assert_eq!(armed["ok"], true);
    assert_eq!(armed["state"]["armed"], true);
    assert_eq!(
        relay.spawner.spawns(),
        vec![vec![
            "-i".to_string(),
            "-m".into(),
            "-s".into(),
            "-t".into(),
            "3600".into(),
            "-w".into(),
            std::process::id().to_string(),
        ]]
    );

    // The settings now read it as on, until an hour after the clock.
    let settings = relay.settings().await;
    assert_eq!(settings["nosleep"]["armed"], true);
    assert_eq!(settings["nosleep"]["until"], NOW + 3_600_000);
    assert_eq!(settings["nosleep"]["pid"], PID);
    assert_eq!(relay.spawner.killed(), vec![false]);

    // DELETE ends the window: the child is killed and the settings read off again.
    let (status, disarmed) = relay
        .send_json(relay.request(reqwest::Method::DELETE, "/api/nosleep"))
        .await;
    assert_eq!(status, 200, "{disarmed}");
    assert_eq!(disarmed["ok"], true);
    assert_eq!(disarmed["state"]["armed"], false);
    assert_eq!(relay.spawner.killed(), vec![true]);
    let settings = relay.settings().await;
    assert_eq!(settings["nosleep"]["armed"], false);
    assert_eq!(settings["nosleep"]["until"], Value::Null);
    assert_eq!(relay.spawner.spawns().len(), 1);
}

#[tokio::test]
async fn restart_is_refused_while_chats_work_and_runs_when_agents_may_stop() {
    let relay = Relay::start().await;

    // Without `stopAgents` the two working chats refuse it, and Conductor is left alone.
    let (status, refused) = relay
        .send_json(
            relay
                .request(reqwest::Method::POST, "/api/conductor/restart")
                .json(&json!({})),
        )
        .await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        refused,
        json!({
            "ok": false,
            "agentsRunning": true,
            "working": 2,
            "error": "2 chats are mid-turn. Restarting Conductor ends them.",
        })
    );
    assert!(relay.app.calls().is_empty(), "{:?}", relay.app.calls());

    // With `stopAgents: true` Conductor is quit and started again.
    let (status, restarted) = relay
        .send_json(
            relay
                .request(reqwest::Method::POST, "/api/conductor/restart")
                .json(&json!({ "stopAgents": true })),
        )
        .await;
    assert_eq!(status, 200, "{restarted}");
    assert_eq!(restarted["ok"], true);
    assert!(restarted["ms"].is_u64(), "{restarted}");
    assert_eq!(relay.app.count("terminate"), 1);
    assert_eq!(relay.app.count("launch"), 1);
    assert_eq!(
        relay.app.calls(),
        vec!["running", "terminate", "running", "launch", "has_window"]
    );
}

#[tokio::test]
async fn a_status_request_does_not_count_as_activity() {
    let relay = Relay::start().await;
    relay.settings().await;

    let first = relay.status().await["activity"]["idleMs"].as_u64().unwrap();
    // Long enough that a status request resetting the clock would show.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let second = relay.status().await["activity"]["idleMs"].as_u64().unwrap();
    assert!(second >= first, "{second} < {first}");
    assert!(second >= 30, "{second}");
}
