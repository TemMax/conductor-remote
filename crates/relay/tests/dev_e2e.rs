//! The dev server end to end: real HTTP requests through the router, the dev-server service, the
//! UI thread and the UI action over a fake window with a Run strip, a real bridge and a real
//! listener. `ps`, `lsof` and `tailscale` are scripted. The fake desktop lives on the UI thread,
//! built inside the `UiActor::spawn` factory; the test sees what a wrapper driver copies into a
//! shared snapshot after each command. Nothing here reaches the Mac.

mod support;

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, Services, Token};
use conductor_remote::dev::controller::{DevDeps, DevServers, DevTimings};
use conductor_remote::http::router;
use conductor_remote::reads::extras::commands::{CommandError, Commands, Limits, Output};
use conductor_remote::reads::Reads;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::{
    RunOutcome, Target, UiDriver as DriverCommands, UiError, ViewReport,
};
use conductor_remote::ui::fake::{
    add_run_strip, conductor_app, FakeDesktop, FakeEvent, RunStrip, RunStripSpec, WindowSpec,
};
use conductor_remote::ui::screen::SessionState;
use http_body_util::BodyExt;
use rusqlite::params;
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const TOKEN: &str = "secret-token";
const BIN: &str = "/opt/homebrew/bin/tailscale";
const HOST: &str = "mac.example.ts.net";
const REPO: &str = "relay";
const BRANCH: &str = "user/feature-x";
const WORKSPACE: &str = "ws-1";
const WORKSPACE_NAME: &str = "beta";
const CLIENT_TIMEOUT_MS: &str = "75000";
const WEB: &str = "Web";
const API_DOCS: &str = "Api docs";
/// The mapping the Mac already had before the relay forwarded anything.
const OTHER_PORT: u16 = 443;
const OTHER_PROXY: &str = "http://127.0.0.1:3000";

/// A stand-in for a dev server: answers every request with `hello` and notes the `Host` it got.
struct Listener {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// Listens on `127.0.0.1:0`. A plain thread serves it: the UI thread, which starts it when
    /// the strip's Run is pressed, has no tokio runtime.
    fn start(hosts: Arc<Mutex<Vec<String>>>) -> Listener {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
        let port = listener
            .local_addr()
            .expect("the listener's address")
            .port();
        listener.set_nonblocking(true).expect("a polling listener");
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => answer_request(stream, &hosts),
                        Err(_) => std::thread::sleep(Duration::from_millis(2)),
                    }
                }
            })
        };
        Listener {
            port,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Listener {
    /// Stops accepting: once the thread is gone the port is closed.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answers one request with `hello`, noting its `Host`; a connection that sends no request (a
/// probe) is dropped.
fn answer_request(mut stream: std::net::TcpStream, hosts: &Mutex<Vec<String>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    if let Some(host) = head.lines().find_map(|line| line.strip_prefix("Host: ")) {
        hosts.lock().unwrap().push(host.to_owned());
    }
    let body = "hello";
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
}

/// A loopback port that refuses connections for as long as the socket lives: it is bound and
/// never listens, so no other socket of the test run can take the port either.
struct Closed {
    _socket: tokio::net::TcpSocket,
}

fn hold(port: u16) -> std::io::Result<Closed> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(std::net::SocketAddr::from(([127, 0, 0, 1], port)))?;
    Ok(Closed { _socket: socket })
}

/// A running Run task: its wrapper process and the port it listens on.
struct Task {
    pid: u32,
    name: String,
    port: u16,
}

/// This Mac as the scripted commands see it: the processes and the serve mappings; and the
/// listeners the strip's reactions start.
struct Model {
    worktree: String,
    tasks: Vec<Task>,
    listeners: HashMap<String, Listener>,
    /// The ports of the dev servers that went away, kept closed.
    closed: Vec<Closed>,
    /// Every request head's `Host` the listeners got.
    hosts: Arc<Mutex<Vec<String>>>,
    next_pid: u32,
    /// Serve port → proxy target.
    mappings: BTreeMap<u16, String>,
}

impl Model {
    /// Brings the task's dev server up, as the strip's Run does.
    fn listen(&mut self, name: &str) {
        let listener = Listener::start(Arc::clone(&self.hosts));
        self.tasks.push(Task {
            pid: self.next_pid,
            name: name.to_owned(),
            port: listener.port,
        });
        self.next_pid += 1;
        self.listeners.insert(name.to_owned(), listener);
    }

    /// Takes the task's dev server down, as the strip's Stop does.
    fn close(&mut self, name: &str) {
        self.tasks.retain(|task| task.name != name);
        if let Some(listener) = self.listeners.remove(name) {
            let port = listener.port;
            drop(listener);
            self.closed.extend(hold(port).ok());
        }
    }
}

fn output(code: i32, stdout: &str) -> Output {
    Output {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn serve_status_json(mappings: &BTreeMap<u16, String>) -> String {
    let mut tcp = serde_json::Map::new();
    let mut web = serde_json::Map::new();
    for (port, proxy) in mappings {
        tcp.insert(port.to_string(), json!({ "HTTPS": true }));
        web.insert(
            format!("{HOST}:{port}"),
            json!({ "Handlers": { "/": { "Proxy": proxy } } }),
        );
    }
    json!({ "TCP": tcp, "Web": web }).to_string()
}

fn https_port(flag: &str) -> u16 {
    flag.strip_prefix("--https=")
        .and_then(|port| port.parse().ok())
        .expect("an --https=<port> flag")
}

/// `Commands` over the model: `ps`, `lsof` and the `tailscale` subcommands.
struct Scripted {
    home: PathBuf,
    model: Arc<Mutex<Model>>,
}

impl Commands for Scripted {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        _cwd: Option<&Path>,
        _limits: Limits,
    ) -> Result<Output, CommandError> {
        let mut model = self.model.lock().unwrap();
        match (program, args) {
            ("ps", _) => {
                let mut listing = String::from("    1     0 /sbin/launchd\n");
                for task in &model.tasks {
                    listing.push_str(&format!(
                        "{:>5}     1 /bin/zsh {}/.conductor/projects/{}/run-1.sh\n",
                        task.pid,
                        self.home.display(),
                        model.worktree.replace('/', "--"),
                    ));
                }
                Ok(output(0, &listing))
            }
            ("/usr/sbin/lsof", [.., pids]) => {
                let pids: Vec<u32> = pids.split(',').filter_map(|pid| pid.parse().ok()).collect();
                let mut table =
                    String::from("COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\n");
                let mut rows = 0;
                for task in model.tasks.iter().filter(|task| pids.contains(&task.pid)) {
                    table.push_str(&format!(
                        "node {} user 23u IPv4 0x1 0t0 TCP 127.0.0.1:{} (LISTEN)\n",
                        task.pid, task.port
                    ));
                    rows += 1;
                }
                Ok(output(i32::from(rows == 0), &table))
            }
            (BIN, ["status", "--json"]) => Ok(output(
                0,
                &json!({ "Self": { "DNSName": format!("{HOST}.") } }).to_string(),
            )),
            (BIN, ["serve", "status", "--json"]) => {
                Ok(output(0, &serve_status_json(&model.mappings)))
            }
            (BIN, ["serve", "--bg", "--yes", https, target]) => {
                model
                    .mappings
                    .insert(https_port(https), (*target).to_owned());
                Ok(output(0, ""))
            }
            (BIN, ["serve", "--yes", https, "off"]) => {
                model.mappings.remove(&https_port(https));
                Ok(output(0, ""))
            }
            _ => Err(CommandError::Spawn {
                program: program.to_owned(),
            }),
        }
    }
}

/// What the test thread sees of the UI thread, as of the end of the last command.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    running: Option<String>,
    starts: usize,
    stops: usize,
    /// How many buttons or menu items anything pressed.
    presses: usize,
}

type Shared = Arc<Mutex<Snapshot>>;

/// The driver of the UI thread: the real commands over the fake desktop, and after each of them a
/// copy of what the test wants to see into `shared`.
struct UiDriver {
    driver: Driver<FakeDesktop>,
    strip: RunStrip,
    shared: Shared,
}

impl UiDriver {
    fn new(driver: Driver<FakeDesktop>, strip: RunStrip, shared: Shared) -> UiDriver {
        let wrapper = UiDriver {
            driver,
            strip,
            shared,
        };
        wrapper.publish();
        wrapper
    }

    fn publish(&self) {
        let presses = self
            .driver
            .desktop()
            .events()
            .iter()
            .filter(|event| matches!(event, FakeEvent::Press(_)))
            .count();
        *self.shared.lock().unwrap() = Snapshot {
            running: self.strip.running(),
            starts: self.strip.starts(),
            stops: self.strip.stops(),
            presses,
        };
    }
}

impl DriverCommands for UiDriver {
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

    // Forwarded explicitly: the trait's provided `run_task` answers `NoWindow`.
    fn run_task(
        &mut self,
        target: &Target,
        task: Option<&str>,
        start: bool,
    ) -> Result<RunOutcome, UiError> {
        let result = self.driver.run_task(target, task, start);
        self.publish();
        result
    }
}

fn window() -> WindowSpec {
    WindowSpec {
        repo: REPO.to_owned(),
        branch: BRANCH.to_owned(),
        sidebar: vec!["alpha".to_owned(), WORKSPACE_NAME.to_owned()],
        chats: vec!["Chat".to_owned()],
        selected: 0,
        composer_value: None,
    }
}

/// The UI thread: a fake window with a Run strip of `Web` and `Api docs`, whose reactions start
/// and drop the dev server of the model.
fn fake_ui(model: Arc<Mutex<Model>>, locked: bool, shared: Shared) -> UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&window());
        let desktop = FakeDesktop::new(app.clone());
        if locked {
            desktop.set_session(Some(SessionState {
                locked: true,
                on_console: true,
            }));
        }
        let strip = add_run_strip(
            &app,
            &RunStripSpec {
                tasks: vec![WEB.to_owned(), API_DOCS.to_owned()],
                selected: 0,
                running: None,
            },
        );
        let starter = Arc::clone(&model);
        strip.on_start(move |name| starter.lock().unwrap().listen(name));
        strip.on_stop(move |name| model.lock().unwrap().close(name));
        Box::new(UiDriver::new(
            Driver::new(desktop),
            strip,
            Arc::clone(&shared),
        ))
    })
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

/// The router over the real dev-server service, the UI thread and a synthetic database.
struct Rig {
    _test: TestDb,
    _home: TempDir,
    _state: TempDir,
    _repo: TempDir,
    app: Router,
    model: Arc<Mutex<Model>>,
    shared: Shared,
}

impl Rig {
    fn new(locked: bool) -> Rig {
        let test = TestDb::new();
        let home = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();

        let settings = repo.path().join(".conductor");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("settings.toml"),
            "[scripts.run.web]\ncommand = \"npm run web\"\n\n\
             [scripts.run.api-docs]\ncommand = \"npm run api-docs\"\n",
        )
        .unwrap();

        // `list_workspaces` takes the directory as the worktree only when it holds `.git`.
        let worktree = test.root().join(REPO).join("attic");
        std::fs::create_dir_all(worktree.join(".git")).unwrap();
        let conn = test.conn();
        conn.execute(
            "INSERT INTO repos (id, name, root_path) VALUES ('r-1', ?1, ?2)",
            params![REPO, repo.path().to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, \
             workspace_name, state) VALUES (?1, ?1, 'r-1', 'attic', ?2, ?3, 'ready')",
            params![WORKSPACE, BRANCH, WORKSPACE_NAME],
        )
        .unwrap();

        let model = Arc::new(Mutex::new(Model {
            worktree: worktree.to_string_lossy().into_owned(),
            tasks: Vec::new(),
            listeners: HashMap::new(),
            closed: Vec::new(),
            hosts: Arc::default(),
            next_pid: 4000,
            mappings: BTreeMap::from([(OTHER_PORT, OTHER_PROXY.to_owned())]),
        }));
        let shared = Shared::default();
        let ui = fake_ui(Arc::clone(&model), locked, Arc::clone(&shared));
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let dev = DevServers::with_timings(
            DevDeps {
                reads: Arc::clone(&reads),
                ui,
                commands: Arc::new(Scripted {
                    home: home.path().to_path_buf(),
                    model: Arc::clone(&model),
                }),
                home: home.path().to_path_buf(),
                state_dir: state.path().to_path_buf(),
                tailscale: Some(PathBuf::from(BIN)),
            },
            DevTimings {
                ready_wait: ms(2000),
                stop_wait: ms(2000),
                look: ms(10),
            },
        );
        let app = router(AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
            assets: Arc::new(MemoryAssets::default()),
            reads: Some(reads),
            writes: None,
            notify: None,
            services: Services {
                dev: Some(dev),
                ..Services::default()
            },
        });
        Rig {
            _test: test,
            _home: home,
            _state: state,
            _repo: repo,
            app,
            model,
            shared,
        }
    }

    fn snapshot(&self) -> Snapshot {
        self.shared.lock().unwrap().clone()
    }

    fn mappings(&self) -> BTreeMap<u16, String> {
        self.model.lock().unwrap().mappings.clone()
    }

    /// The mappings the relay made: serve port → bridge port, the Mac's own left out.
    fn forwarded(&self) -> BTreeMap<u16, u16> {
        self.mappings()
            .into_iter()
            .filter(|(port, _)| *port != OTHER_PORT)
            .map(|(port, proxy)| {
                let bridge = proxy
                    .strip_prefix("http://127.0.0.1:")
                    .and_then(|bridge| bridge.parse().ok())
                    .unwrap_or_else(|| panic!("a mapping to a bridge, got {proxy}"));
                (port, bridge)
            })
            .collect()
    }

    /// The `Host` of every request the listeners got.
    fn hosts(&self) -> Vec<String> {
        let hosts = Arc::clone(&self.model.lock().unwrap().hosts);
        let hosts = hosts.lock().unwrap().clone();
        hosts
    }

    /// The port the task's listener has.
    fn listener_port(&self, name: &str) -> u16 {
        self.model.lock().unwrap().listeners[name].port
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

    async fn get(&self, workspace_id: &str) -> (StatusCode, Value) {
        self.request(Method::GET, &dev_server(workspace_id), None)
            .await
    }

    async fn start(&self, body: Value) -> (StatusCode, Value) {
        self.request(Method::POST, &dev_server(WORKSPACE), Some(body))
            .await
    }

    async fn stop(&self) -> (StatusCode, Value) {
        self.request(Method::DELETE, &dev_server(WORKSPACE), None)
            .await
    }
}

fn dev_server(workspace_id: &str) -> String {
    format!("/api/workspaces/{workspace_id}/dev-server")
}

async fn json_body(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// What a request for the tailnet name gets through the bridge on `bridge_port`.
async fn through_bridge(bridge_port: u16) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", bridge_port))
        .await
        .expect("the bridge accepts");
    stream
        .write_all(format!("GET / HTTP/1.1\r\nHost: {HOST}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.unwrap();
    String::from_utf8_lossy(&answer).into_owned()
}

fn run_configs() -> Value {
    json!([
        { "id": "web", "name": WEB, "command": "npm run web" },
        { "id": "api-docs", "name": API_DOCS, "command": "npm run api-docs" },
    ])
}

fn other_mapping() -> BTreeMap<u16, String> {
    BTreeMap::from([(OTHER_PORT, OTHER_PROXY.to_owned())])
}

#[tokio::test(flavor = "multi_thread")]
async fn state_before_anything_runs_lists_the_run_configs() {
    let rig = Rig::new(false);

    let (status, body) = rig.get(WORKSPACE).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["available"], true, "{body}");
    assert_eq!(body["running"], false, "{body}");
    assert_eq!(body["forwards"], json!([]), "{body}");
    assert_eq!(body["runConfigs"], run_configs(), "{body}");
    assert_eq!(rig.snapshot().presses, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_a_config_forwards_its_port_through_a_bridge() {
    let rig = Rig::new(false);

    let (status, body) = rig.start(json!({ "runConfigId": "web" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["changed"], true, "{body}");
    assert_eq!(body["task"], WEB, "{body}");

    let port = rig.listener_port(WEB);
    let forwarded = rig.forwarded();
    assert_eq!(forwarded.len(), 1, "{:?}", rig.mappings());
    let (serve_port, bridge_port) = forwarded.into_iter().next().unwrap();
    assert_eq!(
        body["forwards"],
        json!([{
            "name": format!("Port {port}"),
            "port": port,
            "running": true,
            "forwarded": true,
            "url": format!("https://{HOST}:{serve_port}/"),
        }]),
        "{body}"
    );

    // A request under the tailnet name reaches the dev server as a local one.
    let answer = through_bridge(bridge_port).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("hello"), "{answer}");
    assert_eq!(rig.hosts(), [format!("127.0.0.1:{port}")]);

    let seen = rig.snapshot();
    assert_eq!(seen.running.as_deref(), Some(WEB));
    assert_eq!((seen.starts, seen.stops), (1, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_without_a_choice_among_two_configs_presses_nothing() {
    let rig = Rig::new(false);

    let (status, body) = rig.start(json!({})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["error"], "Choose which Run config to start", "{body}");

    let seen = rig.snapshot();
    assert_eq!(seen.presses, 0);
    assert_eq!((seen.starts, seen.stops), (0, 0));
    assert_eq!(rig.mappings(), other_mapping());
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_another_config_stops_the_running_one_first() {
    let rig = Rig::new(false);
    let (status, body) = rig.start(json!({ "runConfigId": "web" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = rig.start(json!({ "runConfigId": "api-docs" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["task"], API_DOCS, "{body}");

    let seen = rig.snapshot();
    assert_eq!(seen.running.as_deref(), Some(API_DOCS));
    assert_eq!((seen.starts, seen.stops), (2, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_closes_the_forward_and_leaves_the_other_mapping_alone() {
    let rig = Rig::new(false);
    let (status, body) = rig.start(json!({ "runConfigId": "web" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(rig.forwarded().len(), 1);
    assert_eq!(rig.mappings()[&OTHER_PORT], OTHER_PROXY);

    let (status, body) = rig.stop().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["changed"], true, "{body}");

    assert_eq!(rig.mappings(), other_mapping());
    let seen = rig.snapshot();
    assert_eq!(seen.running, None);
    assert_eq!((seen.starts, seen.stops), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_mac_presses_nothing_and_forwards_nothing() {
    let rig = Rig::new(true);

    let (status, body) = rig.start(json!({ "runConfigId": "web" })).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["ok"], false, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(error.starts_with("The Mac is locked"), "{body}");

    let seen = rig.snapshot();
    assert_eq!(seen.presses, 0);
    assert_eq!((seen.starts, seen.stops), (0, 0));
    assert_eq!(seen.running, None);
    assert_eq!(rig.mappings(), other_mapping());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_workspace_is_404() {
    let rig = Rig::new(false);

    let (status, body) = rig.get("ws-nowhere").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}
