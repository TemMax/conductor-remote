//! Workspace creation over the fake desktop, a synthetic database, an in-memory store and a
//! temporary state directory. The fake desktop shows the New workspace dialog for the link, and
//! pressing Create writes into the database the row Conductor would; no real link is ever opened.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{CreateRequest, WriteAnswer, WriteService};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::testing::FakeCommands;
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::fake::{
    add_workspace_ui, conductor_app, FakeDesktop, WindowSpec, WorkspaceUiSpec,
};
use conductor_remote::ui::screen::SessionState;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;

/// A repository with a checkout path.
const REPO: &str = "relay";
const REPO_ID: &str = "r-1";
const REPO_ROOT: &str = "/Users/me/code/relay";
/// A repository whose name and checkout path hold a space.
const SPACED: &str = "my repo";
const SPACED_ID: &str = "r-2";
const SPACED_ROOT: &str = "/Users/me/code/my repo";
/// A repository without a checkout path.
const NO_ROOT: &str = "loose";
/// A repository whose checkout path is empty.
const EMPTY_ROOT: &str = "blank";
/// The live workspace that exists before any creation.
const EXISTING: &str = "ws-1";
/// The workspace the fake link creates.
const CREATED: &str = "ws-new";

const NOT_CREATED: &str =
    "Conductor didn\u{2019}t create a workspace \u{2014} check it\u{2019}s running and not showing a dialog.";
const LOCKED: &str = "The Mac is locked - the lock screen hides Conductor from the relay, so \
                      nothing can be sent or pressed. Unlock the Mac and try again.";

/// How the fake Conductor reacts to a creation link.
#[derive(Clone, Copy)]
struct Fake {
    /// The repository the link's new live workspace lands in; `None`: no workspace appears.
    creates_in: Option<&'static str>,
    /// The Mac is locked.
    locked: bool,
}

impl Default for Fake {
    fn default() -> Fake {
        Fake {
            creates_in: Some(REPO_ID),
            locked: false,
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
        create_checks: 3,
    }
}

/// Four repositories (`REPO`, `SPACED`, `NO_ROOT` with no path, `EMPTY_ROOT` with an empty one)
/// and one live workspace, `EXISTING`, in `REPO`.
fn seed(conn: &Connection) {
    let repos: [(&str, &str, Option<&str>); 4] = [
        (REPO_ID, REPO, Some(REPO_ROOT)),
        (SPACED_ID, SPACED, Some(SPACED_ROOT)),
        ("r-3", NO_ROOT, None),
        ("r-4", EMPTY_ROOT, Some("")),
    ];
    for (id, name, root) in repos {
        conn.execute(
            "INSERT INTO repos (id, name, root_path) VALUES (?1, ?2, ?3)",
            params![id, name, root],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
         directory_name, state) VALUES (?1, ?1, ?2, 'user/old', 'old', 'old-dir', 'ready')",
        params![EXISTING, REPO_ID],
    )
    .unwrap();
}

/// The window Conductor shows; a creation never reads it.
fn spec() -> WindowSpec {
    WindowSpec {
        repo: REPO.to_owned(),
        branch: "user/old".to_owned(),
        sidebar: vec!["old".to_owned()],
        chats: vec!["Untitled".to_owned()],
        selected: 0,
        composer_value: None,
    }
}

type Urls = Arc<Mutex<Vec<String>>>;

/// A UI thread whose driver runs over a fake desktop acting as `fake` on the database at `db`.
fn fake_ui(db: PathBuf, fake: Fake, urls: Urls) -> UiHandle {
    UiActor::spawn(move || {
        let desktop = FakeDesktop::new(conductor_app(&spec()));
        if fake.locked {
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
        let rows = Connection::open(&db).expect("open the test database for writing");
        desktop.on_open_url(move |url| urls.lock().unwrap().push(url.to_owned()));
        dialog.on_create(move || {
            if let Some(repository) = fake.creates_in {
                rows.execute(
                    "INSERT INTO workspaces (local_id, id, repository_id, branch, \
                     workspace_name, directory_name, state) \
                     VALUES (?1, ?1, ?2, 'user/new', 'new', 'new-dir', 'setting_up')",
                    params![CREATED, repository],
                )
                .unwrap();
            }
        });
        Box::new(Driver::new(desktop))
    })
}

struct Rig {
    test: TestDb,
    reads: Arc<Reads>,
    writes: Writes,
    urls: Urls,
}

impl Rig {
    fn new(fake: Fake) -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        let urls = Urls::default();
        let ui = fake_ui(test.path().to_path_buf(), fake, Arc::clone(&urls));
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let store = Arc::new(Store::open_in_memory().expect("an in-memory store"));
        let parked = ParkedQueue::new(
            Arc::clone(&store),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let writes = Writes::new(Arc::clone(&reads), ui, Arc::new(|| true), timings(), parked)
            .configure(WriteDeps {
                state_dir: test.dir().join("state"),
                store,
                commands: Arc::new(FakeCommands::default()),
                locked: Arc::new(|| Some(false)),
            });
        Rig {
            test,
            reads,
            writes,
            urls,
        }
    }

    fn urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }

    async fn create(
        &self,
        repo: Option<&str>,
        prompt: Option<&str>,
        send_immediately: bool,
        attachment_ids: Vec<String>,
    ) -> WriteAnswer {
        self.writes
            .create_workspace(
                CreateRequest {
                    repo: repo.map(str::to_owned),
                    prompt: prompt.map(str::to_owned),
                    send_immediately,
                    attachment_ids,
                    agent: None,
                },
                Priority::Interactive,
            )
            .await
    }

    /// Stages a file as the phone does; its stage id and token.
    async fn stage(&self, name: &str, bytes: &'static [u8]) -> (String, String) {
        let answer = self
            .writes
            .stage_attachment(name.to_owned(), Bytes::from_static(bytes))
            .await;
        assert_eq!(answer.status, 201, "{answer:?}");
        let attachment = &answer.body["attachment"];
        (
            attachment["stageId"].as_str().unwrap().to_owned(),
            attachment["token"].as_str().unwrap().to_owned(),
        )
    }

    /// The live workspace as `list_workspaces` serves it.
    fn live(&self, workspace_id: &str) -> Value {
        let workspace = self
            .reads
            .list_workspaces()
            .unwrap()
            .into_iter()
            .find(|workspace| workspace.id == workspace_id)
            .expect("a live workspace");
        serde_json::to_value(workspace).unwrap()
    }

    fn live_count(&self) -> i64 {
        self.test
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM workspaces WHERE state IN ('ready', 'setting_up')",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }
}

// ---- refusals before the link ----

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_staged_attachment_is_a_conflict() {
    let rig = Rig::new(Fake::default());
    let answer = rig
        .create(Some(REPO), Some("hello"), true, vec!["AbC123".to_owned()])
        .await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "an attached file is no longer available; add it again" })
    );
    assert!(rig.urls().is_empty());
    assert!(rig.writes.pending_prompts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn one_missing_attachment_among_staged_ones_is_a_conflict() {
    let rig = Rig::new(Fake::default());
    let (staged, _) = rig.stage("notes.txt", b"notes").await;
    let answer = rig
        .create(Some(REPO), None, true, vec![staged, "Zz9999".to_owned()])
        .await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "an attached file is no longer available; add it again" })
    );
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn neither_a_repo_nor_a_prompt_is_a_bad_request() {
    let rig = Rig::new(Fake::default());
    for prompt in [None, Some(""), Some("  \n ")] {
        let answer = rig.create(None, prompt, true, Vec::new()).await;
        assert_eq!(answer.status, 400, "{answer:?}");
        assert_eq!(answer.body, json!({ "error": "need a repo or a prompt" }));
    }
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_repo_is_not_found() {
    let rig = Rig::new(Fake::default());
    let answer = rig
        .create(Some("nowhere"), Some("hello"), true, Vec::new())
        .await;
    assert_eq!(answer.status, 404, "{answer:?}");
    assert_eq!(answer.body, json!({ "error": "unknown repo nowhere" }));
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_without_a_checkout_path_is_a_conflict() {
    let rig = Rig::new(Fake::default());
    for name in [NO_ROOT, EMPTY_ROOT] {
        let answer = rig
            .create(Some(name), Some("hello"), true, Vec::new())
            .await;
        assert_eq!(answer.status, 409, "{answer:?}");
        assert_eq!(
            answer.body,
            json!({ "error": format!("{name} has no checkout path") })
        );
    }
    assert!(rig.urls().is_empty());
    assert_eq!(rig.live_count(), 1);
}

// ---- the link ----

#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_does_not_open_is_a_bad_gateway() {
    let rig = Rig::new(Fake {
        locked: true,
        ..Fake::default()
    });
    let answer = rig
        .create(Some(REPO), Some("hello"), true, Vec::new())
        .await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "deeplink", "error": LOCKED })
    );
    assert!(rig.urls().is_empty(), "a locked Mac opens nothing");
    assert!(rig.writes.pending_prompts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_new_workspace_is_a_bad_gateway() {
    let rig = Rig::new(Fake {
        creates_in: None,
        ..Fake::default()
    });
    let answer = rig
        .create(Some(REPO), Some("hello"), true, Vec::new())
        .await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "deeplink", "error": NOT_CREATED })
    );
    assert_eq!(
        rig.urls(),
        vec![format!(
            "conductor://path={}",
            "%2FUsers%2Fme%2Fcode%2Frelay"
        )]
    );
    assert!(rig.writes.pending_prompts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_workspace_of_another_repo_does_not_count() {
    let rig = Rig::new(Fake {
        creates_in: Some(SPACED_ID),
        ..Fake::default()
    });
    let answer = rig
        .create(Some(REPO), Some("hello"), true, Vec::new())
        .await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "deeplink", "error": NOT_CREATED })
    );
    assert!(rig.writes.pending_prompts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_repo_any_new_workspace_counts() {
    let rig = Rig::new(Fake {
        creates_in: Some(SPACED_ID),
        ..Fake::default()
    });
    let answer = rig.create(None, Some("  hello  "), true, Vec::new()).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["workspaceId"], CREATED);
    assert_eq!(answer.body["workspace"]["repo_name"], SPACED);
    assert_eq!(rig.urls(), vec!["conductor://".to_owned()]);
}

// ---- created ----

#[tokio::test(flavor = "multi_thread")]
async fn a_creation_in_a_repo_with_a_space_answers_the_workspace_and_queues_the_prompt() {
    let rig = Rig::new(Fake {
        creates_in: Some(SPACED_ID),
        ..Fake::default()
    });
    let prompt = "fix it&path=/etc (now)";
    let answer = rig
        .create(
            Some(SPACED),
            Some(&format!("\n {prompt} \n")),
            true,
            Vec::new(),
        )
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        rig.urls(),
        vec!["conductor://path=%2FUsers%2Fme%2Fcode%2Fmy%20repo".to_owned()]
    );
    assert_eq!(
        answer.body,
        json!({
            "ok": true,
            "workspaceId": CREATED,
            "workspace": rig.live(CREATED),
            "pendingPrompt": prompt,
            "sent": false,
            "configured": false,
        })
    );
    assert_eq!(answer.body["workspace"]["repo_name"], SPACED);
    assert_eq!(answer.body["workspace"]["state"], "setting_up");

    let pending = rig.writes.pending_prompts();
    assert_eq!(pending.len(), 1, "{pending:?}");
    let entry = &pending[0];
    assert_eq!(entry["workspaceId"], CREATED);
    assert_eq!(entry["text"], prompt);
    assert_eq!(entry["status"], "waiting");
    assert_eq!(entry["attempts"], 0);
    assert_eq!(entry["sendImmediately"], true);
    assert_eq!(entry["attachmentIds"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_objective_is_the_attachment_tokens_then_the_prompt() {
    let rig = Rig::new(Fake::default());
    let (first, first_token) = rig.stage("plan.md", b"# plan").await;
    let (second, second_token) = rig.stage("shot (1).png", b"\x89PNG").await;
    let answer = rig
        .create(
            Some(REPO),
            Some("  read these  "),
            false,
            vec![first.clone(), second.clone()],
        )
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");

    let objective = format!("{first_token}\n{second_token}\nread these");
    assert_eq!(answer.body["pendingPrompt"], objective.as_str());
    assert_eq!(answer.body["workspaceId"], CREATED);
    assert_eq!(answer.body["workspace"], rig.live(CREATED));
    assert_eq!(answer.body["sent"], false);
    assert_eq!(answer.body["configured"], false);

    let urls = rig.urls();
    assert_eq!(urls.len(), 1, "{urls:?}");
    assert!(!urls[0].contains("prompt="), "{}", urls[0]);
    assert_eq!(urls[0], "conductor://path=%2FUsers%2Fme%2Fcode%2Frelay");

    let pending = rig.writes.pending_prompts();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["workspaceId"], CREATED);
    assert_eq!(pending[0]["text"], objective.as_str());
    assert_eq!(pending[0]["sendImmediately"], false);
    assert_eq!(pending[0]["attachmentIds"], json!([first, second]));
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_alone_are_an_objective() {
    let rig = Rig::new(Fake::default());
    let (staged, token) = rig.stage("log.txt", b"log").await;
    let answer = rig.create(None, None, true, vec![staged.clone()]).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["pendingPrompt"], token.as_str());
    let pending = rig.writes.pending_prompts();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["attachmentIds"], json!([staged]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_without_a_prompt_creates_an_empty_workspace_and_queues_nothing() {
    let rig = Rig::new(Fake::default());
    let answer = rig.create(Some(REPO), Some("   "), true, Vec::new()).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({
            "ok": true,
            "workspaceId": CREATED,
            "workspace": rig.live(CREATED),
            "sent": false,
            "configured": false,
        })
    );
    assert_eq!(
        rig.urls(),
        vec!["conductor://path=%2FUsers%2Fme%2Fcode%2Frelay".to_owned()]
    );
    assert!(rig.writes.pending_prompts().is_empty());
}
