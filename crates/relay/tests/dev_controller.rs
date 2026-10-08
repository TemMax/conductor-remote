//! The dev-server service over a scripted `Commands`, a stub driver and a synthetic database.
//!
//! The scripted commands answer `ps`, `lsof` and the `tailscale` subcommands from a small model
//! in memory; the stub plays Conductor's Run strip by starting a listener on a loopback port.
//! Nothing here runs a real program, reaches Conductor or leaves the temporary directories.

mod support;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use conductor_remote::contract::Priority;
use conductor_remote::delivery::WriteAnswer;
use conductor_remote::dev::bridge::bridge_matches;
use conductor_remote::dev::controller::{DevDeps, DevServers, DevTimings};
use conductor_remote::dev::ports::tcp_open;
use conductor_remote::dev::store::{self, Forward};
use conductor_remote::dev::tailscale::{preferred_serve_port, SERVE_PORTS};
use conductor_remote::dev::DevServerService;
use conductor_remote::reads::extras::commands::{CommandError, Commands, Limits, Output};
use conductor_remote::reads::Reads;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::{RunOutcome, Target, UiDriver, UiError, ViewReport};
use rusqlite::params;
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BIN: &str = "/opt/homebrew/bin/tailscale";
const HOST: &str = "mac.example.ts.net";
const REPO: &str = "relay";
const BRANCH: &str = "user/feature-x";
const WS: &str = "ws-1";
const OTHER: &str = "ws-2";
const NOT_CONNECTED: &str = "Tailscale is not connected on this Mac";

/// A stand-in for a dev server: answers every request with the `Host` it was asked for.
struct Listener {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// Listens on `127.0.0.1:0`. A plain thread serves it: the UI thread, which starts it when
    /// the stub presses Run, has no tokio runtime.
    fn start() -> Listener {
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
                        Ok((stream, _)) => answer_request(stream),
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

/// Answers one request with `dev:<its Host header>`; a connection that sends none (a probe) is
/// dropped.
fn answer_request(mut stream: std::net::TcpStream) {
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
    let host = head
        .lines()
        .find_map(|line| line.strip_prefix("Host: "))
        .unwrap_or("none");
    let body = format!("dev:{host}");
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
    port: u16,
    _socket: tokio::net::TcpSocket,
}

/// Binds `127.0.0.1:<port>` (0 is any free port) without listening.
fn hold(port: u16) -> std::io::Result<Closed> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(std::net::SocketAddr::from(([127, 0, 0, 1], port)))?;
    Ok(Closed {
        port: socket.local_addr()?.port(),
        _socket: socket,
    })
}

fn closed_port() -> Closed {
    hold(0).expect("a loopback port")
}

/// A running Run task: its wrapper process and the ports it listens on.
struct Task {
    pid: u32,
    worktree: String,
    ports: Vec<u16>,
}

/// What the stub does besides answering when a Run button is pressed.
#[derive(Clone, Copy, Debug)]
enum Play {
    Nothing,
    /// The task comes up: a listener starts and `lsof` names its port.
    Listen,
    /// The task goes away: its listener closes.
    Close,
}

/// One press of a Run button, as the stub was asked for it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Press {
    workspace_id: String,
    task: Option<String>,
    start: bool,
}

fn press(workspace_id: &str, task: Option<&str>, start: bool) -> Press {
    Press {
        workspace_id: workspace_id.to_owned(),
        task: task.map(str::to_owned),
        start,
    }
}

/// This Mac as the service sees it: the processes, the serve mappings and Conductor's Run strip.
#[derive(Default)]
struct Model {
    /// Workspace id → its worktree.
    worktrees: HashMap<String, String>,
    tasks: Vec<Task>,
    /// Workspace id → the listener playing its dev server.
    listeners: HashMap<String, Listener>,
    /// The ports of the dev servers that went away, kept closed.
    closed: Vec<Closed>,
    /// Serve port → proxy target.
    mappings: BTreeMap<u16, String>,
    serve_status_fails: bool,
    /// `serve` refuses a port that already has a mapping.
    refuse_repoint: bool,
    /// `serve` fails for every port.
    serve_fails: bool,
    /// How long `serve status` takes.
    status_takes: Option<Duration>,
    /// Every program run, with its arguments.
    calls: Vec<(String, Vec<String>)>,
    steps: VecDeque<(Result<RunOutcome, UiError>, Play)>,
    presses: Vec<Press>,
    targets: Vec<Target>,
}

impl Model {
    /// Brings the workspace's dev server up and returns its port.
    fn listen(&mut self, workspace_id: &str) -> u16 {
        let listener = Listener::start();
        let port = listener.port;
        let pid = 4000 + u32::try_from(self.tasks.len()).expect("a few tasks");
        self.tasks.push(Task {
            pid,
            worktree: self.worktrees[workspace_id].clone(),
            ports: vec![port],
        });
        self.listeners.insert(workspace_id.to_owned(), listener);
        port
    }

    /// Takes the workspace's dev server down.
    fn close(&mut self, workspace_id: &str) {
        let worktree = self.worktrees[workspace_id].clone();
        self.tasks.retain(|task| task.worktree != worktree);
        if let Some(listener) = self.listeners.remove(workspace_id) {
            let port = listener.port;
            drop(listener);
            // Nobody else may listen there while the test still looks at the port.
            self.closed.extend(hold(port).ok());
        }
    }
}

type Shared = Arc<Mutex<Model>>;

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

/// `Commands` over the model.
struct Scripted {
    home: PathBuf,
    model: Shared,
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
        model.calls.push((
            program.to_owned(),
            args.iter().map(|arg| (*arg).to_owned()).collect(),
        ));
        match (program, args) {
            ("ps", _) => {
                let mut listing = String::from("    1     0 /sbin/launchd\n");
                for task in &model.tasks {
                    listing.push_str(&format!(
                        "{:>5}     1 /bin/zsh {}/.conductor/projects/{}/run-1.sh\n",
                        task.pid,
                        self.home.display(),
                        task.worktree.replace('/', "--"),
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
                    for port in &task.ports {
                        table.push_str(&format!(
                            "node {} user 23u IPv4 0x1 0t0 TCP 127.0.0.1:{port} (LISTEN)\n",
                            task.pid
                        ));
                        rows += 1;
                    }
                }
                Ok(output(i32::from(rows == 0), &table))
            }
            (BIN, ["status", "--json"]) => Ok(output(
                0,
                &json!({ "Self": { "DNSName": format!("{HOST}.") } }).to_string(),
            )),
            (BIN, ["serve", "status", "--json"]) => {
                let answer = if model.serve_status_fails {
                    output(1, "")
                } else {
                    output(0, &serve_status_json(&model.mappings))
                };
                let takes = model.status_takes;
                drop(model);
                if let Some(takes) = takes {
                    std::thread::sleep(takes);
                }
                Ok(answer)
            }
            (BIN, ["serve", "--bg", "--yes", https, target]) => {
                let port = https_port(https);
                if model.serve_fails {
                    return Ok(output(1, ""));
                }
                if model.refuse_repoint && model.mappings.contains_key(&port) {
                    return Ok(output(1, ""));
                }
                model.mappings.insert(port, (*target).to_owned());
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

/// Conductor's Run strip: every press is recorded and answered from the script.
struct Stub {
    model: Shared,
}

impl UiDriver for Stub {
    fn trusted(&self) -> bool {
        true
    }

    fn send_prompt(&mut self, _: &Target, _: &str, _: bool) -> Result<u32, UiError> {
        Err(UiError::NoComposer)
    }

    fn stop_turn(&mut self, _: &Target) -> Result<(), UiError> {
        Err(UiError::NoComposer)
    }

    fn new_chat(&mut self, _: &Target) -> Result<(), UiError> {
        Err(UiError::NoComposer)
    }

    fn locate(&mut self) -> Result<ViewReport, UiError> {
        Ok(ViewReport::default())
    }

    fn open_link(&mut self, _: &str) -> Result<(), UiError> {
        Ok(())
    }

    fn run_task(
        &mut self,
        target: &Target,
        task: Option<&str>,
        start: bool,
    ) -> Result<RunOutcome, UiError> {
        let mut model = self.model.lock().unwrap();
        model.presses.push(press(&target.workspace_id, task, start));
        model.targets.push(target.clone());
        // A press no test scripted fails like a window without Run controls.
        let (result, play) = model
            .steps
            .pop_front()
            .unwrap_or((Err(UiError::NoRunStrip), Play::Nothing));
        match play {
            Play::Nothing => {}
            Play::Listen => {
                model.listen(&target.workspace_id);
            }
            Play::Close => model.close(&target.workspace_id),
        }
        result
    }
}

fn ran(changed: bool, task: Option<&str>) -> Result<RunOutcome, UiError> {
    Ok(RunOutcome {
        changed,
        task: task.map(str::to_owned),
    })
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

struct Rig {
    _test: TestDb,
    home: TempDir,
    state: TempDir,
    repo: TempDir,
    model: Shared,
    ui: UiHandle,
    reads: Arc<Reads>,
}

impl Rig {
    /// Two live workspaces of one repository, each with a worktree, and one Run config (`dev`).
    fn new() -> Rig {
        let test = TestDb::new();
        let home = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let model = Shared::default();

        let conn = test.conn();
        conn.execute(
            "INSERT INTO repos (id, name, root_path) VALUES ('r-1', ?1, ?2)",
            params![REPO, repo.path().to_string_lossy()],
        )
        .unwrap();
        for (id, directory, name) in [(WS, "attic", "beta"), (OTHER, "cellar", "gamma")] {
            // `list_workspaces` takes the directory as the worktree only when it holds `.git`.
            let worktree = test.root().join(REPO).join(directory);
            std::fs::create_dir_all(worktree.join(".git")).unwrap();
            conn.execute(
                "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, \
                 workspace_name, state) VALUES (?1, ?1, 'r-1', ?2, ?3, ?4, 'ready')",
                params![id, directory, BRANCH, name],
            )
            .unwrap();
            model
                .lock()
                .unwrap()
                .worktrees
                .insert(id.to_owned(), worktree.to_string_lossy().into_owned());
        }

        let ui = {
            let model = Arc::clone(&model);
            UiActor::spawn(move || Box::new(Stub { model }))
        };
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let rig = Rig {
            _test: test,
            home,
            state,
            repo,
            model,
            ui,
            reads,
        };
        rig.configs(&["dev"]);
        rig
    }

    /// Writes the repository's Run configs: each id runs `npm run <id>`.
    fn configs(&self, ids: &[&str]) {
        let directory = self.repo.path().join(".conductor");
        std::fs::create_dir_all(&directory).unwrap();
        let settings: String = ids
            .iter()
            .map(|id| format!("[scripts.run.{id}]\ncommand = \"npm run {id}\"\n\n"))
            .collect();
        std::fs::write(directory.join("settings.toml"), settings).unwrap();
    }

    /// A service over this Mac, as the relay makes one when it starts.
    fn servers(&self) -> Arc<DevServers> {
        self.servers_with(Some(PathBuf::from(BIN)))
    }

    fn servers_with(&self, tailscale: Option<PathBuf>) -> Arc<DevServers> {
        let deps = DevDeps {
            reads: Arc::clone(&self.reads),
            ui: self.ui.clone(),
            commands: Arc::new(Scripted {
                home: self.home.path().to_path_buf(),
                model: Arc::clone(&self.model),
            }),
            home: self.home.path().to_path_buf(),
            state_dir: self.state.path().to_path_buf(),
            tailscale,
        };
        let timings = DevTimings {
            ready_wait: ms(200),
            stop_wait: ms(400),
            look: ms(10),
        };
        DevServers::with_timings(deps, timings)
    }

    fn model(&self) -> std::sync::MutexGuard<'_, Model> {
        self.model.lock().unwrap()
    }

    /// Scripts the next press of a Run button.
    fn on_press(&self, result: Result<RunOutcome, UiError>, play: Play) {
        self.model().steps.push_back((result, play));
    }

    fn presses(&self) -> Vec<Press> {
        self.model().presses.clone()
    }

    /// The arguments of every `tailscale` call so far.
    fn tailscale_calls(&self) -> Vec<Vec<String>> {
        self.model()
            .calls
            .iter()
            .filter(|(program, _)| program == BIN)
            .map(|(_, args)| args.clone())
            .collect()
    }

    /// How many times `tailscale serve status` was run so far.
    fn serve_status_calls(&self) -> usize {
        self.tailscale_calls()
            .iter()
            .filter(|args| args.as_slice() == ["serve", "status", "--json"])
            .count()
    }

    /// How many times `ps` was run so far.
    fn ps_calls(&self) -> usize {
        self.model()
            .calls
            .iter()
            .filter(|(program, _)| program == "ps")
            .count()
    }

    /// The serve ports `tailscale serve … off` was run for.
    fn offs(&self) -> Vec<u16> {
        self.tailscale_calls()
            .iter()
            .filter(|args| args.last().is_some_and(|last| last == "off"))
            .map(|args| https_port(&args[2]))
            .collect()
    }

    fn mappings(&self) -> BTreeMap<u16, String> {
        self.model().mappings.clone()
    }

    fn stored(&self) -> Vec<Forward> {
        store::load(self.state.path())
    }
}

async fn state(dev: &DevServers, workspace_id: &str) -> WriteAnswer {
    dev.state(workspace_id.to_owned()).await
}

async fn start(dev: &DevServers, workspace_id: &str, run_config_id: Option<&str>) -> WriteAnswer {
    dev.start(
        workspace_id.to_owned(),
        run_config_id.map(str::to_owned),
        Priority::Interactive,
    )
    .await
}

async fn stop(dev: &DevServers, workspace_id: &str) -> WriteAnswer {
    dev.stop(workspace_id.to_owned(), Priority::Interactive)
        .await
}

fn proxy(bridge_port: u16) -> String {
    format!("http://127.0.0.1:{bridge_port}")
}

/// The serve ports from `port` upwards, wrapping round once.
fn serve_ports_from(port: u16) -> impl Iterator<Item = u16> {
    (port..=*SERVE_PORTS.end()).chain(*SERVE_PORTS.start()..port)
}

/// The dev server's own block: `target` and the nine ports after it.
fn own_block(target: u16) -> std::ops::RangeInclusive<u16> {
    target..=target.saturating_add(9)
}

fn dev_config() -> Value {
    json!({ "id": "dev", "name": "Dev", "command": "npm run dev" })
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

// ---- the store ----

#[test]
fn the_store_keeps_forwards_in_a_private_file() {
    let dir = TempDir::new().unwrap();
    assert_eq!(store::load(dir.path()), Vec::new());

    let forwards = vec![Forward {
        workspace_id: WS.to_owned(),
        target_port: 5173,
        serve_port: 8443,
        bridge_port: 50123,
        host: HOST.to_owned(),
        bridge_token: "abc".to_owned(),
    }];
    store::save(dir.path(), &forwards).unwrap();
    assert_eq!(store::load(dir.path()), forwards);

    let file = dir.path().join("dev-forwards.json");
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let written: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(
        written,
        json!([{
            "workspaceId": WS,
            "targetPort": 5173,
            "servePort": 8443,
            "bridgePort": 50123,
            "host": HOST,
            "bridgeToken": "abc",
        }])
    );
    // Only the file is left: the temporary one was renamed over it.
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("dev-forwards.json")]);

    std::fs::write(&file, "not json").unwrap();
    assert_eq!(store::load(dir.path()), Vec::new());
}

// ---- state ----

#[tokio::test]
async fn state_with_nothing_running() {
    let rig = Rig::new();
    let dev = rig.servers();

    let answer = state(&dev, WS).await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.body,
        json!({
            "available": true,
            "running": false,
            "forwarded": false,
            "port": null,
            "url": null,
            "forwards": [],
            "runConfigs": [dev_config()],
        })
    );
    assert_eq!(rig.presses(), Vec::new());
}

#[tokio::test]
async fn state_with_a_running_unforwarded_port() {
    let rig = Rig::new();
    let dev = rig.servers();
    let port = rig.model().listen(WS);

    let answer = state(&dev, WS).await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.body,
        json!({
            "available": true,
            "running": true,
            "forwarded": false,
            "port": port,
            "url": null,
            "forwards": [{
                "name": format!("Port {port}"),
                "port": port,
                "running": true,
                "forwarded": false,
                "url": null,
            }],
            "runConfigs": [dev_config()],
        })
    );
    // The other workspace's task is not this one's.
    assert_eq!(state(&dev, OTHER).await.body["forwards"], json!([]));
}

#[tokio::test]
async fn two_polls_in_a_row_run_ps_once_and_a_start_runs_it_again() {
    let rig = Rig::new();
    let dev = rig.servers();

    assert_eq!(state(&dev, WS).await.body["forwards"], json!([]));
    assert_eq!(state(&dev, WS).await.body["forwards"], json!([]));
    assert_eq!(rig.ps_calls(), 1);

    // The poll before it kept a listing without the task the start presses.
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(rig.ps_calls() > 1);
    let port = rig.model().tasks[0].ports[0];
    assert_eq!(answer.body["port"], port);
    assert_eq!(answer.body["forwarded"], true);
}

#[tokio::test]
async fn two_polls_of_a_forwarded_workspace_run_serve_status_once() {
    let rig = Rig::new();
    let first = rig.servers();
    rig.model().listen(WS);
    assert_eq!(start(&first, WS, None).await.status, 200);

    // A service that starts with nothing kept, over the forward the first one stored.
    let dev = rig.servers();
    let before = rig.serve_status_calls();
    for _ in 0..2 {
        let answer = state(&dev, WS).await;
        assert_eq!(answer.body["forwarded"], true, "{}", answer.body);
    }
    assert_eq!(rig.serve_status_calls(), before + 1);
}

#[tokio::test]
async fn a_stop_makes_the_next_poll_read_the_serve_status_again() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.model().listen(WS);
    rig.model().listen(OTHER);
    assert_eq!(start(&dev, WS, None).await.status, 200);
    assert_eq!(start(&dev, OTHER, None).await.status, 200);

    // The poll reads the status once and then uses it.
    let before = rig.serve_status_calls();
    assert_eq!(state(&dev, OTHER).await.body["forwarded"], true);
    assert_eq!(state(&dev, OTHER).await.body["forwarded"], true);
    assert_eq!(rig.serve_status_calls(), before + 1);

    // The stop turns a mapping off: what was kept is gone.
    rig.on_press(ran(true, Some("Dev")), Play::Close);
    assert_eq!(stop(&dev, WS).await.status, 200);
    assert_eq!(rig.offs().len(), 1);
    let after_stop = rig.serve_status_calls();
    assert_eq!(state(&dev, OTHER).await.body["forwarded"], true);
    assert_eq!(rig.serve_status_calls(), after_stop + 1);
    assert_eq!(state(&dev, OTHER).await.body["forwarded"], true);
    assert_eq!(rig.serve_status_calls(), after_stop + 1);
}

#[tokio::test]
async fn a_start_reads_a_fresh_serve_status_before_it_chooses_a_serve_port() {
    let rig = Rig::new();
    let dev = rig.servers();
    let first = rig.model().listen(WS);
    rig.model().listen(OTHER);
    assert_eq!(start(&dev, OTHER, None).await.status, 200);
    // The poll leaves a status that is still young.
    assert_eq!(state(&dev, OTHER).await.body["forwarded"], true);

    // Someone else maps the first serve ports of the target after that reading.
    let foreign: BTreeMap<u16, String> = serve_ports_from(preferred_serve_port(first))
        .take(10)
        .map(|port| (port, proxy(9)))
        .collect();
    rig.model().mappings.extend(foreign.clone());
    let before = rig.serve_status_calls();

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(rig.serve_status_calls() > before);
    let stored = rig.stored();
    let forward = stored
        .iter()
        .find(|forward| forward.workspace_id == WS)
        .expect("a forward of the workspace");
    assert!(!foreign.contains_key(&forward.serve_port));
}

#[tokio::test]
async fn a_first_start_reads_the_serve_status_twice() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    let before = rig.serve_status_calls();

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["forwarded"], true);
    assert!(answer.body["url"].is_string(), "{}", answer.body);
    // The availability check and the reading the serve port is chosen with; the final state
    // uses the second.
    assert_eq!(rig.serve_status_calls(), before + 2);
}

#[tokio::test]
async fn a_start_of_two_ports_reads_the_serve_status_twice() {
    let rig = Rig::new();
    let dev = rig.servers();
    let first = rig.model().listen(WS);
    let second = Listener::start();
    rig.model().tasks[0].ports.push(second.port);
    // The ports next to each target are someone else's, so both would take the same fallback
    // port from one reading that did not learn of the first mapping.
    let foreign: BTreeMap<u16, String> = [first, second.port]
        .into_iter()
        .flat_map(|port| port..=port.saturating_add(9))
        .map(|port| (port, proxy(9)))
        .collect();
    rig.model().mappings = foreign.clone();
    let before = rig.serve_status_calls();

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(rig.presses(), Vec::new());
    let forwards = answer.body["forwards"].as_array().expect("the forwards");
    for port in [first, second.port] {
        let forward = forwards
            .iter()
            .find(|forward| forward["port"] == port)
            .expect("the port");
        assert_eq!(forward["forwarded"], true, "{}", answer.body);
    }

    let stored = rig.stored();
    assert_eq!(stored.len(), 2);
    assert_ne!(stored[0].serve_port, stored[1].serve_port);
    let mut expected = foreign;
    for forward in &stored {
        expected.insert(forward.serve_port, proxy(forward.bridge_port));
    }
    assert_eq!(rig.mappings(), expected);
    assert_eq!(rig.serve_status_calls(), before + 2);
}

#[tokio::test]
async fn a_start_after_a_released_forward_reads_the_serve_status_again() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    assert_eq!(start(&dev, WS, None).await.status, 200);
    let old = rig.stored().remove(0);

    // The mapping went away: the stored forward is no longer valid.
    rig.model().mappings.clear();
    let before = rig.serve_status_calls();

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["forwarded"], true);
    // The old forward is released under the table lock and the status is read again after that.
    assert!(rig.serve_status_calls() >= before + 3);
    assert!(!bridge_matches(old.bridge_port, &old.bridge_token).await);

    let stored = rig.stored();
    assert_eq!(stored.len(), 1);
    let forward = &stored[0];
    assert_eq!(forward.target_port, old.target_port);
    assert_eq!(
        rig.mappings().get(&forward.serve_port),
        Some(&proxy(forward.bridge_port))
    );
    assert!(bridge_matches(forward.bridge_port, &forward.bridge_token).await);
}

#[tokio::test]
async fn state_without_tailscale_says_so() {
    let rig = Rig::new();
    let dev = rig.servers_with(None);

    let answer = state(&dev, WS).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["available"], false);
    assert_eq!(answer.body["error"], NOT_CONNECTED);
    assert_eq!(rig.tailscale_calls(), Vec::<Vec<String>>::new());
}

#[tokio::test]
async fn an_unknown_workspace_is_404() {
    let rig = Rig::new();
    let dev = rig.servers();
    let not_found = WriteAnswer::error(404, "workspace not found");

    assert_eq!(state(&dev, "ws-nowhere").await, not_found);
    assert_eq!(start(&dev, "ws-nowhere", None).await, not_found);
    assert_eq!(stop(&dev, "ws-nowhere").await, not_found);
    assert_eq!(rig.presses(), Vec::new());
}

// ---- start ----

#[tokio::test]
async fn start_with_one_config_presses_run_and_forwards_the_port() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(rig.presses(), vec![press(WS, Some("Dev"), true)]);
    {
        let model = rig.model();
        assert_eq!(model.targets[0].repo.as_deref(), Some(REPO));
        assert_eq!(model.targets[0].branch, BRANCH);
        assert_eq!(model.targets[0].workspace_name.as_deref(), Some("beta"));
        assert_eq!(model.targets[0].session_id, None);
    }

    let port = rig.model().tasks[0].ports[0];
    let stored = rig.stored();
    assert_eq!(stored.len(), 1);
    let forward = &stored[0];
    assert_eq!(forward.workspace_id, WS);
    assert_eq!(forward.target_port, port);
    assert_eq!(forward.host, HOST);
    assert_eq!(
        rig.mappings(),
        BTreeMap::from([(forward.serve_port, proxy(forward.bridge_port))])
    );

    let url = format!("https://{HOST}:{}/", forward.serve_port);
    assert_eq!(
        answer.body,
        json!({
            "ok": true,
            "available": true,
            "running": true,
            "forwarded": true,
            "port": port,
            "url": url,
            "forwards": [{
                "name": format!("Port {port}"),
                "port": port,
                "running": true,
                "forwarded": true,
                "url": url,
            }],
            "runConfigs": [dev_config()],
            "task": "Dev",
            "changed": true,
        })
    );

    // A request for the tailnet name reaches the dev server as a local one.
    let page = through_bridge(forward.bridge_port).await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.ends_with(&format!("dev:127.0.0.1:{port}")), "{page}");
    assert!(bridge_matches(forward.bridge_port, &forward.bridge_token).await);

    let mode = std::fs::metadata(rig.state.path().join("dev-forwards.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // The state says the same afterwards, and a second start keeps the forward.
    let again = state(&dev, WS).await;
    assert_eq!(again.body["url"], url);
    assert_eq!(again.body["forwarded"], true);
    let second = start(&dev, WS, None).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.body["changed"], false);
    assert_eq!(rig.stored(), stored);
    assert_eq!(rig.presses().len(), 1);
}

#[tokio::test]
async fn start_when_already_listening_presses_nothing() {
    let rig = Rig::new();
    let dev = rig.servers();
    // Two configs: a task that already runs is forwarded whatever their number.
    rig.configs(&["dev", "storybook"]);
    let port = rig.model().listen(WS);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["changed"], false);
    assert_eq!(answer.body["forwarded"], true);
    assert_eq!(answer.body["port"], port);
    assert!(answer.body.get("task").is_none());
    assert_eq!(rig.presses(), Vec::new());

    let stored = rig.stored();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].target_port, port);
    assert_eq!(
        answer.body["url"],
        format!("https://{HOST}:{}/", stored[0].serve_port)
    );
}

#[tokio::test]
async fn an_unknown_run_config_is_refused() {
    let rig = Rig::new();
    let dev = rig.servers();

    let answer = start(&dev, WS, Some("storybook")).await;
    assert_eq!(answer.status, 502);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(
        answer.body["error"],
        "Run config storybook is not available in this workspace"
    );
    assert_eq!(answer.body["runConfigs"], json!([dev_config()]));
    assert!(answer.body.get("changed").is_none());
    assert_eq!(rig.presses(), Vec::new());
    assert_eq!(rig.mappings(), BTreeMap::new());
}

#[tokio::test]
async fn two_configs_need_a_choice() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.configs(&["dev", "story-book"]);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 502);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(answer.body["error"], "Choose which Run config to start");
    assert_eq!(rig.presses(), Vec::new());

    // The chosen one is pressed by its display name.
    rig.on_press(ran(true, Some("Story book")), Play::Listen);
    let chosen = start(&dev, WS, Some("story-book")).await;
    assert_eq!(chosen.status, 200, "{}", chosen.body);
    assert_eq!(chosen.body["task"], "Story book");
    assert_eq!(rig.presses(), vec![press(WS, Some("Story book"), true)]);
}

#[tokio::test]
async fn no_configs_press_the_current_task() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.configs(&[]);
    rig.on_press(ran(true, None), Play::Listen);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["runConfigs"], json!([]));
    assert!(answer.body.get("task").is_none());
    assert_eq!(rig.presses(), vec![press(WS, None, true)]);
}

#[tokio::test]
async fn start_without_tailscale_is_409() {
    let rig = Rig::new();
    let dev = rig.servers_with(None);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 409);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(answer.body["available"], false);
    assert_eq!(answer.body["error"], NOT_CONNECTED);
    assert_eq!(rig.presses(), Vec::new());
}

#[tokio::test]
async fn start_without_tailscale_serve_is_502() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.model().serve_status_fails = true;

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 502);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(
        answer.body["error"],
        "Tailscale Serve is not available on this Mac"
    );
    assert_eq!(rig.presses(), Vec::new());
}

#[tokio::test]
async fn a_failed_press_is_502_with_its_text() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(Err(UiError::NoRunStrip), Play::Nothing);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 502);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(
        answer.body["error"],
        "couldn't find the Run controls of this workspace"
    );
    assert_eq!(answer.body["runConfigs"], json!([dev_config()]));
    assert_eq!(rig.presses(), vec![press(WS, Some("Dev"), true)]);
}

#[tokio::test]
async fn a_task_that_never_listens_is_stopped_again() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Nothing);
    rig.on_press(ran(true, Some("Dev")), Play::Nothing);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 502);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(answer.body["task"], "Dev");
    assert_eq!(answer.body["changed"], true);
    assert_eq!(
        answer.body["error"],
        "Dev started, but nothing listened on a port; it was stopped again"
    );
    assert_eq!(
        rig.presses(),
        vec![press(WS, Some("Dev"), true), press(WS, None, false)]
    );
    assert_eq!(rig.mappings(), BTreeMap::new());
    assert_eq!(rig.stored(), Vec::new());
}

#[tokio::test]
async fn a_failed_second_stop_is_told() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, None), Play::Nothing);
    rig.on_press(Err(UiError::RunNotChanged), Play::Nothing);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 502);
    assert_eq!(
        answer.body["error"],
        "Run task started, but nothing listened on a port; stopping it again also failed: \
         Conductor did not start or stop the Run task"
    );
    assert_eq!(answer.body["changed"], true);
}

#[tokio::test]
async fn an_unchanged_task_that_never_listens_is_left_alone() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(false, Some("Dev")), Play::Nothing);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 502);
    assert_eq!(
        answer.body["error"],
        "Dev started, but nothing listened on a port"
    );
    assert_eq!(answer.body["changed"], false);
    assert_eq!(rig.presses(), vec![press(WS, Some("Dev"), true)]);
}

#[tokio::test]
async fn a_failed_forward_stops_the_task_again() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    rig.on_press(ran(true, Some("Dev")), Play::Close);
    // Every serve port is taken: the mapping of each belongs to someone else.
    let foreign: BTreeMap<u16, String> = (1..=u16::MAX).map(|port| (port, proxy(9))).collect();
    rig.model().mappings = foreign.clone();

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 502);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(
        answer.body["error"],
        "no free Tailscale Serve port is available; it was stopped again"
    );
    assert_eq!(
        rig.presses(),
        vec![press(WS, Some("Dev"), true), press(WS, None, false)]
    );
    assert_eq!(rig.mappings(), foreign);
    assert_eq!(rig.offs(), Vec::<u16>::new());
    assert_eq!(rig.stored(), Vec::new());
}

#[tokio::test]
async fn a_failed_forward_of_a_running_task_keeps_the_earlier_forwards() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    let first = start(&dev, WS, None).await;
    assert_eq!(first.status, 200, "{}", first.body);
    let port = rig.model().tasks[0].ports[0];
    let stored = rig.stored();
    let mappings = rig.mappings();
    assert_eq!(stored.len(), 1);

    // A second port appears on the running task and `tailscale serve` fails for it.
    let second = Listener::start();
    rig.model().tasks[0].ports.push(second.port);
    rig.model().serve_fails = true;

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 502, "{}", answer.body);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(answer.body["changed"], false);
    assert_eq!(rig.presses(), vec![press(WS, Some("Dev"), true)]);

    assert_eq!(rig.stored(), stored);
    assert_eq!(rig.mappings(), mappings);
    let forwards = answer.body["forwards"].as_array().expect("the forwards");
    let earlier = forwards
        .iter()
        .find(|forward| forward["port"] == port)
        .expect("the first port");
    assert_eq!(earlier["forwarded"], true);
    let later = forwards
        .iter()
        .find(|forward| forward["port"] == second.port)
        .expect("the second port");
    assert_eq!(later["forwarded"], false);
}

// ---- stop ----

#[tokio::test]
async fn stop_presses_stop_and_turns_the_mapping_off() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    assert_eq!(start(&dev, WS, None).await.status, 200);
    let forward = rig.stored().remove(0);

    rig.on_press(ran(true, Some("Dev")), Play::Close);
    let answer = stop(&dev, WS).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        answer.body,
        json!({
            "ok": true,
            "available": true,
            "running": false,
            "forwarded": false,
            "port": null,
            "url": null,
            "forwards": [],
            "runConfigs": [dev_config()],
            "task": "Dev",
            "changed": true,
        })
    );
    assert_eq!(
        rig.presses(),
        vec![press(WS, Some("Dev"), true), press(WS, None, false)]
    );
    assert_eq!(rig.offs(), vec![forward.serve_port]);
    assert_eq!(rig.mappings(), BTreeMap::new());
    assert_eq!(rig.stored(), Vec::new());
    // The bridge went with the forward.
    assert!(!bridge_matches(forward.bridge_port, &forward.bridge_token).await);
}

#[tokio::test]
async fn a_foreign_mapping_on_the_serve_port_is_never_turned_off() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    assert_eq!(start(&dev, WS, None).await.status, 200);
    let forward = rig.stored().remove(0);

    // Someone else took the serve port for a server of their own.
    let foreign = BTreeMap::from([(forward.serve_port, proxy(9))]);
    rig.model().mappings = foreign.clone();
    assert_eq!(state(&dev, WS).await.body["forwarded"], false);

    rig.on_press(ran(true, Some("Dev")), Play::Close);
    let answer = stop(&dev, WS).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(rig.offs(), Vec::<u16>::new());
    assert_eq!(rig.mappings(), foreign);
    assert_eq!(rig.stored(), Vec::new());
}

#[tokio::test]
async fn a_port_that_keeps_listening_after_stop_is_told() {
    let rig = Rig::new();
    let dev = rig.servers();
    let port = rig.model().listen(WS);
    rig.on_press(ran(true, Some("Dev")), Play::Nothing);

    let answer = stop(&dev, WS).await;
    assert_eq!(answer.status, 502);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(
        answer.body["error"],
        format!("Dev stopped, but :{port} is still listening")
    );
    assert_eq!(answer.body["task"], "Dev");
    assert_eq!(answer.body["changed"], true);
    assert_eq!(rig.presses(), vec![press(WS, None, false)]);
}

#[tokio::test]
async fn a_failed_stop_is_502_and_keeps_the_forward() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    assert_eq!(start(&dev, WS, None).await.status, 200);
    let stored = rig.stored();

    rig.on_press(Err(UiError::RunNotChanged), Play::Nothing);
    let answer = stop(&dev, WS).await;
    assert_eq!(answer.status, 502);
    assert_eq!(answer.body["ok"], false);
    assert_eq!(
        answer.body["error"],
        "Conductor did not start or stop the Run task"
    );
    assert_eq!(answer.body["forwarded"], true);
    assert_eq!(rig.offs(), Vec::<u16>::new());
    assert_eq!(rig.stored(), stored);
}

#[tokio::test]
async fn a_forward_is_never_served_on_the_dev_servers_port() {
    let rig = Rig::new();
    let dev = rig.servers();
    // A dev server inside the range of the serve ports: its own port is the first one looked at.
    let server = SERVE_PORTS
        .rev()
        .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("a loopback port of the range");
    let target = server.local_addr().expect("the listener's address").port();
    assert_eq!(preferred_serve_port(target), target);
    {
        let mut model = rig.model();
        let worktree = model.worktrees[WS].clone();
        model.tasks.push(Task {
            pid: 4000,
            worktree,
            ports: vec![target],
        });
    }

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(rig.presses(), Vec::new());
    let stored = rig.stored();
    assert_eq!(stored.len(), 1);
    let forward = &stored[0];
    assert_eq!(forward.target_port, target);
    assert!(SERVE_PORTS.contains(&forward.serve_port));
    assert!(!own_block(target).contains(&forward.serve_port));
    assert_eq!(
        answer.body["url"],
        format!("https://{HOST}:{}/", forward.serve_port)
    );
    assert_eq!(
        rig.mappings(),
        BTreeMap::from([(forward.serve_port, proxy(forward.bridge_port))])
    );
}

#[tokio::test]
async fn a_serve_port_something_listens_on_is_skipped() {
    let rig = Rig::new();
    let dev = rig.servers();
    let target = rig.model().listen(WS);
    let preferred = preferred_serve_port(target);
    // When the bind fails because the port is taken, something listens there anyway.
    let _listener = std::net::TcpListener::bind(("127.0.0.1", preferred));
    assert!(tcp_open(preferred).await);

    let answer = start(&dev, WS, None).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let stored = rig.stored();
    assert_eq!(stored.len(), 1);
    let forward = &stored[0];
    assert_eq!(forward.target_port, target);
    assert_ne!(forward.serve_port, preferred);
    assert!(SERVE_PORTS.contains(&forward.serve_port));
    assert_eq!(
        rig.mappings(),
        BTreeMap::from([(forward.serve_port, proxy(forward.bridge_port))])
    );
    assert_eq!(
        answer.body["url"],
        format!("https://{HOST}:{}/", forward.serve_port)
    );
}

// ---- several workspaces ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workspaces_forwarded_at_once_keep_separate_serve_ports() {
    let rig = Rig::new();
    let dev = rig.servers();
    let first = rig.model().listen(WS);
    let second = rig.model().listen(OTHER);
    // Both would take the same serve port: every port of the range is someone else's except two
    // neighbouring ones, which are outside both dev servers' own blocks and which neither start
    // looks at first above the other (so both come to the lower one), and the serve status is
    // slow enough for the two starts to overlap. Nothing on this Mac listens on the two.
    let targets = [first, second];
    let mut free = None;
    for low in *SERVE_PORTS.start()..*SERVE_PORTS.end() {
        let high = low + 1;
        let own = targets
            .iter()
            .any(|target| own_block(*target).contains(&low) || own_block(*target).contains(&high));
        if own || targets.map(preferred_serve_port).contains(&high) {
            continue;
        }
        if !tcp_open(low).await && !tcp_open(high).await {
            free = Some([low, high]);
            break;
        }
    }
    let free = free.expect("two neighbouring serve ports nothing listens on");
    let foreign: BTreeMap<u16, String> = SERVE_PORTS
        .filter(|port| !free.contains(port))
        .map(|port| (port, proxy(9)))
        .collect();
    {
        let mut model = rig.model();
        model.mappings = foreign.clone();
        model.status_takes = Some(ms(20));
    }

    let (one, two) = tokio::join!(start(&dev, WS, None), start(&dev, OTHER, None));
    assert_eq!(one.status, 200, "{}", one.body);
    assert_eq!(two.status, 200, "{}", two.body);
    assert_eq!(rig.presses(), Vec::new());

    let stored = rig.stored();
    assert_eq!(stored.len(), 2);
    let of = |workspace_id: &str| {
        stored
            .iter()
            .find(|forward| forward.workspace_id == workspace_id)
            .expect("a forward of the workspace")
            .clone()
    };
    let (a, b) = (of(WS), of(OTHER));
    assert_eq!((a.target_port, b.target_port), (first, second));
    assert_ne!(a.serve_port, b.serve_port);
    assert_ne!(a.bridge_port, b.bridge_port);
    assert!(!foreign.contains_key(&a.serve_port));
    assert!(!foreign.contains_key(&b.serve_port));

    let mut expected = foreign;
    expected.insert(a.serve_port, proxy(a.bridge_port));
    expected.insert(b.serve_port, proxy(b.bridge_port));
    assert_eq!(rig.mappings(), expected);
    assert_eq!(rig.offs(), Vec::<u16>::new());

    assert_eq!(one.body["url"], format!("https://{HOST}:{}/", a.serve_port));
    assert_eq!(two.body["url"], format!("https://{HOST}:{}/", b.serve_port));
    assert!(through_bridge(a.bridge_port)
        .await
        .ends_with(&format!("dev:127.0.0.1:{first}")));
    assert!(through_bridge(b.bridge_port)
        .await
        .ends_with(&format!("dev:127.0.0.1:{second}")));
}

// ---- sweep ----

#[tokio::test]
async fn sweep_releases_a_forward_on_the_second_pass_after_its_listener_closed() {
    let rig = Rig::new();
    let dev = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    assert_eq!(start(&dev, WS, None).await.status, 200);
    let stored = rig.stored();
    let forward = stored[0].clone();
    let mapped = rig.mappings();

    // A server that listens is left alone, however often it is looked at.
    dev.sweep().await;
    dev.sweep().await;
    dev.sweep().await;
    assert_eq!(rig.stored(), stored);
    assert_eq!(rig.offs(), Vec::<u16>::new());

    rig.model().close(WS);
    dev.sweep().await;
    assert_eq!(rig.stored(), stored);
    assert_eq!(rig.mappings(), mapped);
    assert_eq!(rig.offs(), Vec::<u16>::new());
    // The closed port is still shown, as a forward that no longer works.
    let between = state(&dev, WS).await;
    assert_eq!(between.body["port"], forward.target_port);
    assert_eq!(between.body["running"], false);

    dev.sweep().await;
    assert_eq!(rig.stored(), Vec::new());
    assert_eq!(rig.mappings(), BTreeMap::new());
    assert_eq!(rig.offs(), vec![forward.serve_port]);
    assert!(!bridge_matches(forward.bridge_port, &forward.bridge_token).await);
    assert_eq!(state(&dev, WS).await.body["forwards"], json!([]));
}

// ---- restore ----

#[tokio::test]
async fn restore_rebuilds_a_bridge_and_drops_a_record_whose_mapping_is_gone() {
    let rig = Rig::new();
    let before = rig.servers();
    rig.on_press(ran(true, Some("Dev")), Play::Listen);
    assert_eq!(start(&before, WS, None).await.status, 200);
    let old = rig.stored().remove(0);

    // The relay restarts: its bridges die with it, the mapping and the file stay. A second
    // record is on file whose mapping was removed meanwhile.
    drop(before);
    let orphan = Forward {
        workspace_id: OTHER.to_owned(),
        target_port: old.target_port,
        serve_port: 8443,
        bridge_port: 50123,
        host: HOST.to_owned(),
        bridge_token: "gone".to_owned(),
    };
    store::save(rig.state.path(), &[old.clone(), orphan]).unwrap();

    let dev = rig.servers();
    dev.restore().await;

    let stored = rig.stored();
    assert_eq!(stored.len(), 1);
    let new = &stored[0];
    assert_eq!(
        (new.workspace_id.as_str(), new.target_port, new.serve_port),
        (WS, old.target_port, old.serve_port)
    );
    assert_ne!(new.bridge_token, old.bridge_token);
    assert_eq!(
        rig.mappings(),
        BTreeMap::from([(old.serve_port, proxy(new.bridge_port))])
    );
    assert_eq!(rig.offs(), Vec::<u16>::new());
    assert!(bridge_matches(new.bridge_port, &new.bridge_token).await);
    assert!(through_bridge(new.bridge_port)
        .await
        .ends_with(&format!("dev:127.0.0.1:{}", old.target_port)));

    let answer = state(&dev, WS).await;
    assert_eq!(answer.body["forwarded"], true);
    assert_eq!(
        answer.body["url"],
        format!("https://{HOST}:{}/", old.serve_port)
    );
    assert_eq!(rig.presses().len(), 1);
}

#[tokio::test]
async fn restore_frees_a_port_tailscale_refuses_to_repoint() {
    let rig = Rig::new();
    let port = rig.model().listen(WS);
    let old_bridge = closed_port();
    let old = Forward {
        workspace_id: WS.to_owned(),
        target_port: port,
        serve_port: 443,
        bridge_port: old_bridge.port,
        host: HOST.to_owned(),
        bridge_token: "old".to_owned(),
    };
    store::save(rig.state.path(), std::slice::from_ref(&old)).unwrap();
    {
        let mut model = rig.model();
        model.mappings.insert(443, proxy(old_bridge.port));
        model.refuse_repoint = true;
    }

    let dev = rig.servers();
    // Before the restore the record is there but its bridge is not.
    assert_eq!(state(&dev, WS).await.body["forwarded"], false);
    dev.restore().await;

    let stored = rig.stored();
    assert_eq!(stored.len(), 1);
    assert_ne!(stored[0].bridge_port, old_bridge.port);
    assert!(bridge_matches(stored[0].bridge_port, &stored[0].bridge_token).await);
    assert_eq!(
        rig.mappings(),
        BTreeMap::from([(443, proxy(stored[0].bridge_port))])
    );
    assert_eq!(rig.offs(), vec![443]);

    // The default HTTPS port is left out of the address.
    let answer = state(&dev, WS).await;
    assert_eq!(answer.body["forwarded"], true);
    assert_eq!(answer.body["url"], format!("https://{HOST}/"));
    assert_eq!(
        answer.body["forwards"][0]["url"],
        format!("https://{HOST}/")
    );
}

#[tokio::test]
async fn restore_releases_a_forward_whose_server_is_gone() {
    let rig = Rig::new();
    let (target, old_bridge) = (closed_port(), closed_port());
    let old = Forward {
        workspace_id: WS.to_owned(),
        target_port: target.port,
        serve_port: 8443,
        bridge_port: old_bridge.port,
        host: HOST.to_owned(),
        bridge_token: "old".to_owned(),
    };
    store::save(rig.state.path(), std::slice::from_ref(&old)).unwrap();
    // The relay's mapping, and one of someone else that stays.
    rig.model().mappings = BTreeMap::from([(8443, proxy(old_bridge.port)), (9443, proxy(9))]);

    let dev = rig.servers();
    dev.restore().await;

    assert_eq!(rig.stored(), Vec::new());
    assert_eq!(rig.offs(), vec![8443]);
    assert_eq!(rig.mappings(), BTreeMap::from([(9443, proxy(9))]));
}

#[tokio::test]
async fn restore_without_tailscale_does_nothing() {
    let rig = Rig::new();
    let old = Forward {
        workspace_id: WS.to_owned(),
        target_port: 5173,
        serve_port: 8443,
        bridge_port: 50123,
        host: HOST.to_owned(),
        bridge_token: "old".to_owned(),
    };
    store::save(rig.state.path(), std::slice::from_ref(&old)).unwrap();

    let dev = rig.servers_with(None);
    dev.restore().await;

    assert_eq!(rig.stored(), vec![old]);
    assert!(rig.model().calls.is_empty());
}
