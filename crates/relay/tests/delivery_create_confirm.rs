//! A creation opens Conductor's New workspace dialog with the link and presses Create in it. A
//! stub driver plays Conductor and records every call; nothing here reaches the Mac.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{CreateRequest, WriteAnswer, WriteService};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::testing::FakeCommands;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::{Target, UiDriver, UiError, ViewReport};
use rusqlite::{params, Connection};
use serde_json::json;
use support::TestDb;
use tempfile::TempDir;

const REPO: &str = "relay";
const REPO_ID: &str = "r-1";
const REPO_ROOT: &str = "/Users/me/code/relay";
/// The workspace the stub inserts.
const CREATED: &str = "ws-new";
const PROMPT: &str = "build the thing";
const NO_DIALOG: &str = "Conductor did not show its New workspace dialog";

/// What the stub was asked, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    OpenLink(String),
    ConfirmCreate,
}

#[derive(Default)]
struct Script {
    /// `confirm_create` fails with `NoCreateDialog`.
    no_dialog: bool,
    /// The row is inserted when the link opens, not when Create is pressed.
    row_on_open: bool,
    calls: Vec<Call>,
}

type Shared = Arc<Mutex<Script>>;

struct Stub {
    script: Shared,
    conn: Connection,
}

impl Stub {
    fn insert_row(&self) {
        self.conn
            .execute(
                "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
                 directory_name, state) \
                 VALUES (?1, ?1, ?2, 'user/new', 'new', 'new-dir', 'setting_up')",
                params![CREATED, REPO_ID],
            )
            .unwrap();
    }
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

    fn open_link(&mut self, url: &str) -> Result<(), UiError> {
        let mut script = self.script.lock().unwrap();
        script.calls.push(Call::OpenLink(url.to_owned()));
        if script.row_on_open {
            self.insert_row();
        }
        Ok(())
    }

    fn confirm_create(&mut self) -> Result<(), UiError> {
        let mut script = self.script.lock().unwrap();
        script.calls.push(Call::ConfirmCreate);
        if script.no_dialog {
            return Err(UiError::NoCreateDialog);
        }
        if !script.row_on_open {
            self.insert_row();
        }
        Ok(())
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

struct Rig {
    _test: TestDb,
    _state: TempDir,
    _ui: UiHandle,
    script: Shared,
    writes: Writes,
}

impl Rig {
    fn new(script: Script) -> Rig {
        let test = TestDb::new();
        test.conn()
            .execute(
                "INSERT INTO repos (id, name, root_path) VALUES (?1, ?2, ?3)",
                params![REPO_ID, REPO, REPO_ROOT],
            )
            .unwrap();
        let state_dir = TempDir::new().unwrap();
        let script = Arc::new(Mutex::new(script));
        let ui = {
            let script = Arc::clone(&script);
            let db = test.path().to_path_buf();
            UiActor::spawn(move || {
                Box::new(Stub {
                    script,
                    conn: Connection::open(db).expect("open the test database for writing"),
                })
            })
        };
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let store = Arc::new(Store::open_in_memory().expect("an in-memory store"));
        let parked = ParkedQueue::new(
            Arc::clone(&store),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let writes = Writes::new(reads, ui.clone(), Arc::new(|| true), timings(), parked)
            .configure(WriteDeps {
                state_dir: state_dir.path().to_path_buf(),
                store,
                commands: Arc::new(FakeCommands::default()),
                locked: Arc::new(|| Some(false)),
            });
        Rig {
            _test: test,
            _state: state_dir,
            _ui: ui,
            script,
            writes,
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.script.lock().unwrap().calls.clone()
    }

    async fn create(&self, prompt: Option<&str>) -> WriteAnswer {
        self.writes
            .create_workspace(
                CreateRequest {
                    repo: Some(REPO.to_owned()),
                    prompt: prompt.map(str::to_owned),
                    send_immediately: true,
                    attachment_ids: Vec::new(),
                    agent: None,
                },
                Priority::Interactive,
            )
            .await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_create_opens_a_link_without_a_prompt_and_then_presses_create() {
    let rig = Rig::new(Script::default());
    let answer = rig.create(Some(PROMPT)).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["workspaceId"], CREATED);

    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    let Call::OpenLink(link) = &calls[0] else {
        panic!("the link opens first: {calls:?}");
    };
    assert!(link.starts_with("conductor://path="), "{link}");
    assert!(link.contains("path=%2FUsers%2Fme%2Fcode%2Frelay"), "{link}");
    assert!(!link.contains("prompt="), "{link}");
    assert_eq!(calls[1], Call::ConfirmCreate);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_dialog_fails_the_create_with_its_text_and_looks_for_no_row() {
    // The row is there already after the link, so a look for it would find it.
    let rig = Rig::new(Script {
        no_dialog: true,
        row_on_open: true,
        ..Script::default()
    });
    let answer = rig.create(Some(PROMPT)).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "deeplink", "error": NO_DIALOG })
    );
    assert_eq!(rig.calls().len(), 2, "{:?}", rig.calls());
    assert_eq!(rig.calls()[1], Call::ConfirmCreate);
    assert!(rig.writes.pending_prompts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_create_with_a_prompt_still_queues_the_first_prompt() {
    let rig = Rig::new(Script::default());
    let answer = rig.create(Some(PROMPT)).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["pendingPrompt"], PROMPT);

    let pending = rig.writes.pending_prompts();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["workspaceId"], CREATED);
    assert_eq!(pending[0]["text"], PROMPT);
}
