//! A split into a new workspace over the fake desktop, a synthetic database and a real git
//! repository in a temporary directory. The fake desktop shows the New workspace dialog for the
//! link, and pressing Create does what Conductor would: it writes the new row and checks the new
//! branch out as a worktree. The fork itself runs the real git; no real link is ever opened.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{SplitDestination, SplitRequest, WriteAnswer, WriteService};
use conductor_remote::reads::extras::commands::{
    CommandError, Commands, Limits, Output, SystemCommands,
};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::fake::{
    add_workspace_ui, conductor_app, FakeDesktop, WindowSpec, WorkspaceUiSpec,
};
use conductor_remote::ui::screen::SessionState;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;

const REPO: &str = "relay";
const REPO_ID: &str = "r-1";
/// The workspace that is forked; `<root>/<REPO>/<SOURCE_DIR>` is its worktree, on `SOURCE_BRANCH`.
const SOURCE: &str = "ws-1";
const SOURCE_DIR: &str = "src-dir";
const SOURCE_BRANCH: &str = "user/src";
/// The source's chat, with four rows: 1 a prompt, 2 prose, 3 a prompt, 4 prose.
const CHAT: &str = "s-1";
/// The workspace the fake link creates; `<root>/<REPO>/<NEW_DIR>` is its worktree, on `NEW_BRANCH`.
const CREATED: &str = "ws-new";
const NEW_DIR: &str = "new-dir";
const NEW_BRANCH: &str = "user/new";
/// The chat Conductor opens in the new workspace.
const NEW_CHAT: &str = "s-new";
/// The file the new worktree gets of its own in `Scene::stray`.
const STRAY: &str = "stray.txt";
/// The file the new worktree gets after the look in `Scene::late`.
const LATE: &str = "late.txt";
/// A branch whose name the source's branch is the beginning of.
const OTHER_BRANCH: &str = "user/src-2";
/// The source's folder in the temporary directory when it is not in its normal place.
const ELSEWHERE: &str = "elsewhere";

const EARLIER: &str = "The linker is missing a flag.";
const LATER: &str = "Done; the build passes.";

const FORK_REFS: &str = "refs/conductor-remote/forks";
const FORK_FAILED: &str = "Workspace new-dir was created, but its code fork failed: ";
const NOT_READY: &str = "was not ready for the code";
const UNCONFIRMED: &str = "the source workspace's folder could not be confirmed: it is not on the \
                           workspace's branch";
const NOT_A_FOLDER: &str = "the repository's name is not a folder name, so the new workspace's \
                            folder cannot be found";
const CHANGED: &str = "the new workspace changed files before the fork snapshot could be installed";
const NOT_CREATED: &str =
    "Conductor didn\u{2019}t create a workspace \u{2014} check it\u{2019}s running and not showing a dialog.";
const LOCKED: &str = "The Mac is locked - the lock screen hides Conductor from the relay, so \
                      nothing can be sent or pressed. Unlock the Mac and try again.";

/// What the test's Conductor is like.
#[derive(Clone, Copy)]
struct Scene {
    /// The repository's row has its checkout path.
    root_path: bool,
    /// The Mac is locked.
    locked: bool,
    /// How far pressing Create gets.
    creates: Creates,
    /// The new workspace gets a chat.
    chat: bool,
    /// The new worktree gets a file of its own before the code arrives.
    stray: bool,
    /// The repository's name; its worktrees live under `<root>/<repo>/`.
    repo: &'static str,
    /// Where the source's worktree is and what it has checked out.
    source: Source,
    /// The new worktree gets a file of its own between the look and the install.
    late: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// At `<root>/<repo>/<SOURCE_DIR>`, on `SOURCE_BRANCH`.
    Normal,
    /// At `ELSEWHERE` in the temporary directory, on `SOURCE_BRANCH`.
    Elsewhere,
    /// At `ELSEWHERE`, on `OTHER_BRANCH`: no worktree has `SOURCE_BRANCH` checked out.
    OtherBranch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Creates {
    /// No row appears.
    Nothing,
    /// The row appears; its folder never does.
    Row,
    /// The row and its worktree.
    Worktree,
}

impl Default for Scene {
    fn default() -> Scene {
        Scene {
            root_path: true,
            locked: false,
            creates: Creates::Worktree,
            chat: true,
            stray: false,
            repo: REPO,
            source: Source::Normal,
            late: false,
        }
    }
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
        create_checks: 5,
    }
}

/// Runs git for the fixture itself, apart from the code under test; the trailing line end of its
/// output is dropped.
fn run_git(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.org",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim_end().to_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, out) = run_git(dir, args);
    assert!(ok, "git {args:?} failed in {dir:?}");
    out
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).expect("write");
}

fn status(worktree: &Path) -> String {
    git(worktree, &["status", "--porcelain=v1"])
}

fn head(worktree: &Path) -> String {
    git(worktree, &["rev-parse", "HEAD"])
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 path")
}

/// JavaScript's `encodeURIComponent`, as Conductor's link carries a path.
fn encoded(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// A repository on `main` with one commit and `.context/` in its `info/exclude`, as Conductor
/// leaves one; and the source worktree on `branch` with one commit of its own, a staged change,
/// an unstaged change and an untracked file.
fn make_repo(repo: &Path, source: &Path, branch: &str) {
    std::fs::create_dir(repo).expect("the repository directory");
    git(repo, &["init", "-q", "-b", "main"]);
    write(repo, "tracked.txt", "tracked base\n");
    write(repo, "staged.txt", "staged base\n");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "base"]);
    let info = repo.join(".git").join("info");
    std::fs::create_dir_all(&info).expect("the info directory");
    write(&info, "exclude", ".context/\n");

    git(
        repo,
        &["worktree", "add", "-q", "-b", branch, path_str(source)],
    );
    write(source, "own.txt", "the source's own commit\n");
    git(source, &["add", "own.txt"]);
    git(source, &["commit", "-q", "-m", "source work"]);
    write(source, "staged.txt", "staged in the source\n");
    git(source, &["add", "staged.txt"]);
    write(source, "tracked.txt", "changed in the source\n");
    write(source, "untracked.txt", "new in the source\n");
}

fn insert_row(conn: &Connection, id: &str, content: &str) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, content, created_at, sent_at) \
         VALUES (?1, ?2, ?3, '2026-09-01T10:05:00.000Z', '2026-09-01T10:05:00.000Z')",
        params![id, CHAT, content],
    )
    .unwrap();
}

fn prose(text: &str) -> String {
    json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": text }] } })
        .to_string()
}

/// The repository `repo` (with `root_path`, when given), the live workspace `SOURCE` and its chat
/// `CHAT`.
fn seed(conn: &Connection, repo: &str, root_path: Option<&str>) {
    conn.execute(
        "INSERT INTO repos (id, name, root_path) VALUES (?1, ?2, ?3)",
        params![REPO_ID, repo, root_path],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
         directory_name, state) VALUES (?1, ?1, ?2, ?3, 'src', ?4, 'ready')",
        params![SOURCE, REPO_ID, SOURCE_BRANCH, SOURCE_DIR],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
         updated_at) VALUES (?1, ?2, 'Build', 'idle', 0, '2026-09-01 10:00:00', \
         '2026-09-01 10:00:00')",
        params![CHAT, SOURCE],
    )
    .unwrap();
    insert_row(conn, "m-1", "Why does the build fail?");
    insert_row(conn, "m-2", &prose(EARLIER));
    insert_row(conn, "m-3", "Add it then.");
    insert_row(conn, "m-4", &prose(LATER));
}

/// The window Conductor shows; a fork never reads it.
fn spec() -> WindowSpec {
    WindowSpec {
        repo: REPO.to_owned(),
        branch: SOURCE_BRANCH.to_owned(),
        sidebar: vec!["src".to_owned()],
        chats: vec!["Build".to_owned()],
        selected: 0,
        composer_value: None,
    }
}

type Urls = Arc<Mutex<Vec<String>>>;

/// The real programs. When `late`, `LATE` is dropped into the new worktree just before the second
/// `symbolic-ref HEAD` addressed to it runs: the first is the look's, the second the install's.
struct Programs {
    new_worktree: PathBuf,
    late: bool,
    /// How many `symbolic-ref HEAD` calls were addressed to the new worktree.
    asked: AtomicUsize,
}

impl Commands for Programs {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        let addressed = args
            .windows(2)
            .any(|pair| pair[0] == "-C" && Path::new(pair[1]) == self.new_worktree);
        if addressed
            && args.ends_with(&["symbolic-ref", "HEAD"])
            && self.asked.fetch_add(1, Ordering::SeqCst) == 1
            && self.late
        {
            write(&self.new_worktree, LATE, "after the look\n");
        }
        SystemCommands.run(program, args, cwd, limits)
    }
}

/// Where the fake Conductor works.
struct Places {
    db: PathBuf,
    repo: PathBuf,
    new_worktree: PathBuf,
}

/// A UI thread whose driver runs over a fake desktop playing Conductor as `scene` says.
fn fake_ui(places: Places, scene: Scene, urls: Urls) -> UiHandle {
    UiActor::spawn(move || {
        let desktop = FakeDesktop::new(conductor_app(&spec()));
        if scene.locked {
            desktop.set_session(Some(SessionState {
                locked: true,
                on_console: true,
            }));
        }
        let dialog = add_workspace_ui(
            &desktop,
            &WorkspaceUiSpec {
                running: vec![],
                continue_button: false,
            },
        );
        let rows = Connection::open(&places.db).expect("open the test database for writing");
        desktop.on_open_url(move |url| urls.lock().unwrap().push(url.to_owned()));
        dialog.on_create(move || {
            if scene.creates == Creates::Nothing {
                return;
            }
            rows.execute(
                "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
                 directory_name, state) VALUES (?1, ?1, ?2, ?3, 'new', ?4, 'setting_up')",
                params![CREATED, REPO_ID, NEW_BRANCH, NEW_DIR],
            )
            .unwrap();
            if scene.creates == Creates::Worktree {
                git(
                    &places.repo,
                    &[
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        NEW_BRANCH,
                        path_str(&places.new_worktree),
                    ],
                );
                if scene.stray {
                    write(&places.new_worktree, STRAY, "the new workspace's own\n");
                }
            }
            if scene.chat {
                rows.execute(
                    "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, \
                     created_at, updated_at) VALUES (?1, ?2, 'Untitled', 'idle', 0, \
                     '2026-09-02 10:00:00', '2026-09-02 10:00:00')",
                    params![NEW_CHAT, CREATED],
                )
                .unwrap();
            }
        });
        Box::new(Driver::new(desktop))
    })
}

struct Rig {
    test: TestDb,
    writes: Writes,
    urls: Urls,
    /// The repository's main checkout.
    repo: PathBuf,
    /// The source's worktree.
    source: PathBuf,
    /// Where the new workspace's worktree goes.
    new_worktree: PathBuf,
    /// What `git status` said of the source before anything ran.
    source_status: String,
}

impl Rig {
    fn new(scene: Scene) -> Rig {
        let test = TestDb::new();
        let dir = std::fs::canonicalize(test.dir()).expect("the canonical temporary directory");
        let repo = dir.join("origin");
        let (source, branch) = match scene.source {
            Source::Normal => (test.root().join(scene.repo).join(SOURCE_DIR), SOURCE_BRANCH),
            Source::Elsewhere => (dir.join(ELSEWHERE), SOURCE_BRANCH),
            Source::OtherBranch => (dir.join(ELSEWHERE), OTHER_BRANCH),
        };
        make_repo(&repo, &source, branch);
        seed(
            &test.conn(),
            scene.repo,
            scene.root_path.then(|| path_str(&repo)),
        );

        let urls = Urls::default();
        let new_worktree = test.root().join(scene.repo).join(NEW_DIR);
        let places = Places {
            db: test.path().to_path_buf(),
            repo: repo.clone(),
            new_worktree: new_worktree.clone(),
        };
        let ui = fake_ui(places, scene, Arc::clone(&urls));
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let store = Arc::new(Store::open_in_memory().expect("an in-memory store"));
        let parked = ParkedQueue::new(
            Arc::clone(&store),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let writes =
            Writes::new(reads, ui, Arc::new(|| true), timings(), parked).configure(WriteDeps {
                state_dir: test.dir().join("state"),
                store,
                commands: Arc::new(Programs {
                    new_worktree: new_worktree.clone(),
                    late: scene.late,
                    asked: AtomicUsize::new(0),
                }),
                locked: Arc::new(|| Some(false)),
            });
        let source_status = status(&source);
        Rig {
            test,
            writes,
            urls,
            repo,
            source,
            new_worktree,
            source_status,
        }
    }

    fn urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }

    fn source(&self) -> PathBuf {
        self.source.clone()
    }

    fn new_worktree(&self) -> PathBuf {
        self.new_worktree.clone()
    }

    /// The one link a fork opens.
    fn link(&self) -> String {
        format!("conductor://path={}", encoded(path_str(&self.repo)))
    }

    /// What `git for-each-ref` prints of the fork snapshots.
    fn fork_refs(&self) -> String {
        git(&self.repo, &["for-each-ref", FORK_REFS])
    }

    fn workspace_rows(&self) -> i64 {
        self.test
            .conn()
            .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
            .unwrap()
    }

    /// The new workspace's `state`: the relay leaves it to Conductor.
    fn created_state(&self) -> String {
        self.test
            .conn()
            .query_row(
                "SELECT state FROM workspaces WHERE id = ?1",
                [CREATED],
                |row| row.get(0),
            )
            .unwrap()
    }

    async fn fork(&self, request: SplitRequest) -> WriteAnswer {
        self.writes
            .split_chat(CHAT.to_owned(), request, Priority::Interactive)
            .await
    }

    /// The transcript a fork wrote, read back from the new worktree.
    fn written(&self, answer: &WriteAnswer) -> String {
        let path = answer.body["attachment"]["path"].as_str().expect("a path");
        std::fs::read_to_string(self.new_worktree().join(path)).expect("the transcript file")
    }
}

/// The phone's defaults for a fork: thinking in, tools out, no cut.
fn request() -> SplitRequest {
    SplitRequest {
        destination: SplitDestination::Workspace,
        workspace_id: Some(SOURCE.to_owned()),
        prompt: None,
        include_thinking: true,
        include_tools: false,
        through_rowid: None,
        only_rowid: None,
    }
}

fn error(answer: &WriteAnswer) -> &str {
    answer.body["error"].as_str().expect("an error text")
}

// ---- forked ----

#[tokio::test]
async fn a_fork_carries_the_code_and_the_transcript_into_a_new_workspace() {
    let rig = Rig::new(Scene::default());
    let answer = rig
        .fork(SplitRequest {
            prompt: Some("  carry on  ".to_owned()),
            ..request()
        })
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let body = &answer.body;
    assert_eq!(body["ok"], true);
    assert_eq!(body["destination"], "workspace");
    assert_eq!(body["workspaceId"], CREATED);
    assert_eq!(body["sessionId"], NEW_CHAT);
    let text = body["text"].as_str().expect("a text");
    assert!(
        text.starts_with("Forked from @\u{27e6}Transcript of "),
        "{text}"
    );
    assert!(text.ends_with("\n\ncarry on"), "{text}");

    let attachment = &body["attachment"];
    assert_eq!(attachment["name"], "Transcript of Build.md");
    assert_eq!(attachment["kept"], 4);
    assert_eq!(
        attachment["elided"],
        json!({ "thinking": 0, "tools": 0, "earlier": 0, "later": 0 })
    );
    let path = attachment["path"].as_str().expect("a path");
    let file = rig.new_worktree().join(path);
    assert!(file.is_file(), "{file:?}");
    let transcript = rig.written(&answer);
    assert_eq!(attachment["bytes"], transcript.len());
    assert!(transcript.contains(EARLIER) && transcript.contains(LATER));

    // The code: the source's commit, staged state and files, on the new workspace's own branch.
    let new = rig.new_worktree();
    assert_eq!(status(&new), rig.source_status);
    assert!(rig.source_status.contains("untracked.txt"));
    assert_eq!(head(&new), head(&rig.source()));
    assert_ne!(head(&new), git(&rig.repo, &["rev-parse", "main"]));
    assert_eq!(git(&new, &["symbolic-ref", "--short", "HEAD"]), NEW_BRANCH);
    assert_eq!(
        std::fs::read_to_string(new.join("untracked.txt")).unwrap(),
        "new in the source\n"
    );

    assert_eq!(rig.urls(), vec![rig.link()]);
    assert_eq!(rig.fork_refs(), "");
    assert_eq!(status(&rig.source()), rig.source_status);
    assert_eq!(rig.created_state(), "setting_up");
}

#[tokio::test]
async fn a_fork_without_a_chat_yet_answers_no_session() {
    let rig = Rig::new(Scene {
        chat: false,
        ..Scene::default()
    });
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["destination"], "workspace");
    assert_eq!(answer.body["workspaceId"], CREATED);
    assert_eq!(answer.body["sessionId"], Value::Null);
    assert!(rig.written(&answer).contains(LATER));
    assert_eq!(head(&rig.new_worktree()), head(&rig.source()));
    assert_eq!(rig.fork_refs(), "");
}

#[tokio::test]
async fn a_fork_cuts_the_transcript_where_the_request_says() {
    let rig = Rig::new(Scene::default());
    let answer = rig
        .fork(SplitRequest {
            through_rowid: Some(2),
            ..request()
        })
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let attachment = &answer.body["attachment"];
    assert_eq!(attachment["kept"], 2);
    assert!(
        attachment["elided"]["later"].as_u64().unwrap() > 0,
        "{attachment}"
    );
    let transcript = rig.written(&answer);
    assert!(transcript.contains(EARLIER), "{transcript}");
    assert!(!transcript.contains(LATER), "{transcript}");
    assert_eq!(rig.fork_refs(), "");
}

#[tokio::test]
async fn a_source_found_by_its_branch_is_forked() {
    let rig = Rig::new(Scene {
        source: Source::Elsewhere,
        ..Scene::default()
    });
    assert!(!rig.test.root().join(REPO).join(SOURCE_DIR).exists());
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["workspaceId"], CREATED);

    let new = rig.new_worktree();
    assert_eq!(status(&new), rig.source_status);
    assert!(rig.source_status.contains("untracked.txt"));
    assert_eq!(head(&new), head(&rig.source()));
    assert_ne!(head(&new), git(&rig.repo, &["rev-parse", "main"]));
    assert_eq!(rig.urls(), vec![rig.link()]);
    assert_eq!(rig.fork_refs(), "");
    assert_eq!(status(&rig.source()), rig.source_status);
}

// ---- nothing created ----

#[tokio::test]
async fn a_source_folder_on_another_branch_is_not_forked() {
    let rig = Rig::new(Scene {
        source: Source::OtherBranch,
        ..Scene::default()
    });
    assert!(!rig.test.root().join(REPO).join(SOURCE_DIR).exists());
    assert_eq!(
        git(&rig.source(), &["symbolic-ref", "--short", "HEAD"]),
        OTHER_BRANCH
    );
    let (exists, _) = run_git(
        &rig.repo,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{SOURCE_BRANCH}"),
        ],
    );
    assert!(!exists, "nothing is on the workspace's branch");

    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(answer.body, json!({ "error": UNCONFIRMED }));
    assert!(rig.urls().is_empty(), "nothing was opened");
    assert_eq!(rig.workspace_rows(), 1);
    assert_eq!(rig.fork_refs(), "");
    assert_eq!(status(&rig.source()), rig.source_status);
}

#[tokio::test]
async fn a_repository_name_that_is_not_a_folder_name_is_not_forked() {
    let rig = Rig::new(Scene {
        repo: "nested/repo",
        ..Scene::default()
    });
    assert_eq!(
        rig.source(),
        rig.test.root().join("nested").join("repo").join(SOURCE_DIR)
    );
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(answer.body, json!({ "error": NOT_A_FOLDER }));
    assert!(rig.urls().is_empty(), "nothing was opened");
    assert_eq!(rig.workspace_rows(), 1);
    assert_eq!(rig.fork_refs(), "");
    assert_eq!(status(&rig.source()), rig.source_status);
}

#[tokio::test]
async fn a_source_in_the_middle_of_a_merge_is_not_forked() {
    let rig = Rig::new(Scene::default());
    // A merge needs a clean index; then `main` moves on and is merged without the commit.
    let source = rig.source();
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-q", "-m", "work in progress"]);
    write(&rig.repo, "side.txt", "on main\n");
    git(&rig.repo, &["add", "side.txt"]);
    git(&rig.repo, &["commit", "-q", "-m", "side"]);
    git(&source, &["merge", "-q", "--no-ff", "--no-commit", "main"]);

    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "Could not snapshot the source workspace: the source workspace is in \
                          the middle of a Git merge" })
    );
    assert!(rig.urls().is_empty(), "nothing was opened");
    assert_eq!(rig.workspace_rows(), 1);
    assert_eq!(rig.fork_refs(), "");
}

#[tokio::test]
async fn a_creation_that_makes_nothing_answers_as_a_creation_does() {
    let rig = Rig::new(Scene {
        creates: Creates::Nothing,
        ..Scene::default()
    });
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "deeplink", "error": NOT_CREATED })
    );
    assert_eq!(rig.urls(), vec![rig.link()]);
    assert_eq!(rig.workspace_rows(), 1);
    assert_eq!(rig.fork_refs(), "");
    assert_eq!(status(&rig.source()), rig.source_status);
}

#[tokio::test]
async fn a_locked_mac_answers_as_a_creation_does() {
    let rig = Rig::new(Scene {
        locked: true,
        ..Scene::default()
    });
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "deeplink", "error": LOCKED })
    );
    assert!(rig.urls().is_empty(), "a locked Mac opens nothing");
    assert_eq!(rig.workspace_rows(), 1);
    assert_eq!(rig.fork_refs(), "");
}

#[tokio::test]
async fn a_source_whose_repository_has_no_root_path_is_a_conflict() {
    let rig = Rig::new(Scene {
        root_path: false,
        ..Scene::default()
    });
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "the source workspace has no repository checkout to fork" })
    );
    assert!(rig.urls().is_empty(), "nothing was opened");
    assert_eq!(rig.workspace_rows(), 1);
    assert_eq!(rig.fork_refs(), "");
}

// ---- created, but not forked ----

#[tokio::test]
async fn a_new_worktree_with_a_file_of_its_own_is_left_alone() {
    let rig = Rig::new(Scene {
        stray: true,
        ..Scene::default()
    });
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    let text = error(&answer);
    assert!(text.starts_with(FORK_FAILED), "{text}");
    assert!(text.contains(NOT_READY), "{text}");
    assert_eq!(answer.body, json!({ "error": text }));

    let new = rig.new_worktree();
    assert_eq!(
        std::fs::read_to_string(new.join(STRAY)).unwrap(),
        "the new workspace's own\n"
    );
    assert_eq!(head(&new), git(&rig.repo, &["rev-parse", "main"]));
    assert_ne!(head(&new), head(&rig.source()));
    assert_eq!(status(&new), format!("?? {STRAY}"));
    assert!(!new.join(".context").exists(), "no transcript was written");
    assert_eq!(rig.fork_refs(), "");
    assert_eq!(status(&rig.source()), rig.source_status);
    assert_eq!(rig.created_state(), "setting_up");
}

#[tokio::test]
async fn a_new_workspace_whose_folder_never_appears_is_not_forked() {
    let rig = Rig::new(Scene {
        creates: Creates::Row,
        ..Scene::default()
    });
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    let text = error(&answer);
    assert!(text.starts_with(FORK_FAILED), "{text}");
    assert!(text.contains(NOT_READY), "{text}");
    assert!(!rig.new_worktree().exists());
    assert_eq!(rig.fork_refs(), "");
    assert_eq!(status(&rig.source()), rig.source_status);
    assert_eq!(rig.created_state(), "setting_up");
}

#[tokio::test]
async fn a_new_worktree_that_changes_after_the_look_is_left_alone() {
    let rig = Rig::new(Scene {
        late: true,
        ..Scene::default()
    });
    let answer = rig.fork(request()).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": format!("{FORK_FAILED}{CHANGED}") })
    );

    let new = rig.new_worktree();
    assert_eq!(
        std::fs::read_to_string(new.join(LATE)).unwrap(),
        "after the look\n"
    );
    assert_eq!(head(&new), git(&rig.repo, &["rev-parse", "main"]));
    assert_ne!(head(&new), head(&rig.source()));
    assert_eq!(status(&new), format!("?? {LATE}"));
    assert_eq!(rig.fork_refs(), "");
    assert_eq!(status(&rig.source()), rig.source_status);
}
