//! The agent settings of a new workspace's first chat: kept beside a first prompt and applied just
//! before it is sent, or applied at once when the workspace is created without a prompt. A stub
//! driver plays Conductor (the link creates the rows, `send_prompt` lands the user row) and records
//! every `set_agent`; nothing here reaches the Mac.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use conductor_remote::agent::{AgentPatch, Effort};
use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{CreateRequest, WriteAnswer, WriteService};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::testing::FakeCommands;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::{
    AgentFailure, AgentOutcome, Target, UiDriver, UiError, ViewReport,
};
use rusqlite::{params, Connection};
use serde_json::Value;
use support::TestDb;
use tempfile::TempDir;

const REPO: &str = "relay";
const REPO_ID: &str = "r-1";
const REPO_ROOT: &str = "/Users/me/code/relay";
/// The workspace the stub's link creates, and its chat.
const CREATED: &str = "ws-new";
const NEW_CHAT: &str = "chat-new";
/// The chat Conductor opens for a model of another provider.
const MODEL_CHAT: &str = "chat-model";
const PROMPT: &str = "build the thing";
const NO_CHAT_YET: &str =
    "the new workspace has no chat yet, so its agent settings were not applied";
const LOCKED: &str = "The Mac is locked - the lock screen hides Conductor from the relay, so \
                      nothing can be sent or pressed. Unlock the Mac and try again.";
/// How long a test waits for the pump before it gives up.
const WAIT: Duration = Duration::from_secs(40);

/// What the stub does with the next `set_agent`.
#[derive(Clone, Debug)]
enum Step {
    Ok,
    Fail(UiError),
}

#[derive(Default)]
struct Script {
    /// The answers of the next `set_agent` calls; then `fallback`.
    steps: VecDeque<Step>,
    fallback: Option<Step>,
    /// The link also opens a chat in the new workspace.
    chat_on_open: bool,
    /// `set_agent` opens a new chat and reports it.
    opens_chat: bool,
    /// `send_prompt` fails without landing anything.
    send_fails: bool,
    /// The state of the new workspace.
    state: &'static str,
    calls: Vec<(Target, AgentPatch)>,
    /// `set_agent` and `send_prompt`, in the order they were called.
    events: Vec<&'static str>,
    sent: Vec<String>,
}

type Shared = Arc<Mutex<Script>>;

struct Stub {
    script: Shared,
    conn: Connection,
}

impl UiDriver for Stub {
    fn trusted(&self) -> bool {
        true
    }

    fn send_prompt(&mut self, target: &Target, text: &str, _: bool) -> Result<u32, UiError> {
        let mut script = self.script.lock().unwrap();
        script.events.push("send_prompt");
        script.sent.push(text.to_owned());
        if script.send_fails {
            return Err(UiError::NoComposer);
        }
        self.conn
            .execute(
                "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                 VALUES ('m-landed', ?1, 'user', ?2, 'turn-1')",
                params![target.session_id.as_deref().unwrap(), text],
            )
            .unwrap();
        Ok(1)
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

    fn confirm_create(&mut self) -> Result<(), UiError> {
        Ok(())
    }

    fn open_link(&mut self, _: &str) -> Result<(), UiError> {
        let script = self.script.lock().unwrap();
        self.conn
            .execute(
                "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
                 directory_name, state) VALUES (?1, ?1, ?2, 'user/new', 'new', 'new-dir', ?3)",
                params![CREATED, REPO_ID, script.state],
            )
            .unwrap();
        if script.chat_on_open {
            self.conn
                .execute(
                    "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, \
                     agent_type, created_at, updated_at) \
                     VALUES (?1, ?2, 'Untitled', 'idle', 0, 'claude', \
                     '2026-10-01 10:00:00', '2026-10-01 10:00:00')",
                    params![NEW_CHAT, CREATED],
                )
                .unwrap();
        }
        Ok(())
    }

    fn set_agent(
        &mut self,
        target: &Target,
        patch: &AgentPatch,
    ) -> Result<AgentOutcome, AgentFailure> {
        let mut script = self.script.lock().unwrap();
        script.events.push("set_agent");
        script.calls.push((target.clone(), patch.clone()));
        let step = script
            .steps
            .pop_front()
            .or_else(|| script.fallback.clone())
            .unwrap_or(Step::Ok);
        match step {
            Step::Ok if script.opens_chat => {
                self.conn
                    .execute(
                        "INSERT OR IGNORE INTO sessions (id, workspace_id, title, status, \
                         is_hidden, agent_type, created_at, updated_at) \
                         VALUES (?1, ?2, 'Untitled', 'idle', 0, 'codex', \
                         '2026-10-01 10:00:01', '2026-10-01 10:00:01')",
                        params![MODEL_CHAT, CREATED],
                    )
                    .unwrap();
                Ok(AgentOutcome { new_chat: true })
            }
            Step::Ok => Ok(AgentOutcome::default()),
            Step::Fail(error) => Err(error.into()),
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

fn seed(conn: &Connection) {
    conn.execute(
        "INSERT INTO repos (id, name, root_path) VALUES (?1, ?2, ?3)",
        params![REPO_ID, REPO, REPO_ROOT],
    )
    .unwrap();
}

struct Rig {
    _test: TestDb,
    _state: TempDir,
    _ui: UiHandle,
    script: Shared,
    store: Arc<Store>,
    writes: Writes,
}

impl Rig {
    /// A new workspace in `state` that gets a chat with the link when `chat_on_open`.
    fn new(state: &'static str, chat_on_open: bool) -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        let state_dir = TempDir::new().unwrap();
        let script = Shared::default();
        {
            let mut script = script.lock().unwrap();
            script.state = state;
            script.chat_on_open = chat_on_open;
        }
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
                store: Arc::clone(&store),
                commands: Arc::new(FakeCommands::default()),
                locked: Arc::new(|| Some(false)),
            });
        Rig {
            _test: test,
            _state: state_dir,
            _ui: ui,
            script,
            store,
            writes,
        }
    }

    fn fallback(&self, step: Step) {
        self.script.lock().unwrap().fallback = Some(step);
    }

    fn calls(&self) -> Vec<(Target, AgentPatch)> {
        self.script.lock().unwrap().calls.clone()
    }

    fn events(&self) -> Vec<&'static str> {
        self.script.lock().unwrap().events.clone()
    }

    fn sent(&self) -> Vec<String> {
        self.script.lock().unwrap().sent.clone()
    }

    fn stored_agent(&self) -> Option<String> {
        self.store.first_prompt_agent(CREATED).unwrap()
    }

    async fn create(&self, prompt: Option<&str>, agent: Option<AgentPatch>) -> WriteAnswer {
        self.writes
            .create_workspace(
                CreateRequest {
                    repo: Some(REPO.to_owned()),
                    prompt: prompt.map(str::to_owned),
                    send_immediately: true,
                    attachment_ids: Vec::new(),
                    agent,
                },
                Priority::Interactive,
            )
            .await
    }

    /// The phone's entry of the new workspace's first prompt.
    fn entry(&self) -> Option<Value> {
        self.writes
            .pending_prompts()
            .into_iter()
            .find(|entry| entry["workspaceId"] == CREATED)
    }

    async fn wait(&self, what: &str, done: impl Fn(&Rig) -> bool) {
        let started = Instant::now();
        while !done(self) {
            assert!(started.elapsed() < WAIT, "gave up waiting for {what}");
            tokio::time::sleep(ms(20)).await;
        }
    }
}

fn effort_high() -> AgentPatch {
    AgentPatch {
        effort: Some(Effort::High),
        ..AgentPatch::default()
    }
}

// ---- a first prompt keeps the agent ----

#[tokio::test(flavor = "multi_thread")]
async fn create_with_a_prompt_and_an_agent_stores_the_agent() {
    let rig = Rig::new("setting_up", true);
    let answer = rig.create(Some(PROMPT), Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["configured"], false);
    assert_eq!(rig.stored_agent(), Some(effort_high().to_json()));
    assert!(rig.entry().is_some());
    assert!(rig.calls().is_empty(), "nothing is applied before the send");
}

#[tokio::test(flavor = "multi_thread")]
async fn create_with_a_prompt_and_no_agent_clears_a_stored_one() {
    let rig = Rig::new("setting_up", true);
    rig.store
        .set_first_prompt_agent(CREATED, Some(&effort_high().to_json()))
        .unwrap();
    let answer = rig.create(Some(PROMPT), None).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(rig.stored_agent(), None);
    assert!(rig.calls().is_empty());
}

// ---- the send applies it ----

#[tokio::test(flavor = "multi_thread")]
async fn the_first_prompts_send_applies_the_stored_patch_first_and_clears_it() {
    let rig = Rig::new("setting_up", true);
    rig.writes.start_first_prompts();
    let answer = rig.create(Some(PROMPT), Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");

    rig.wait("the first prompt to be sent", |rig| {
        rig.entry().is_none() && !rig.sent().is_empty()
    })
    .await;
    assert_eq!(rig.events(), ["set_agent", "send_prompt"]);
    assert_eq!(rig.sent(), [PROMPT]);
    let calls = rig.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.session_id.as_deref(), Some(NEW_CHAT));
    assert_eq!(calls[0].1, effort_high());
    assert_eq!(rig.stored_agent(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_chat_for_the_model_keeps_the_agent_stored_when_the_send_fails() {
    let rig = Rig::new("setting_up", true);
    {
        let mut script = rig.script.lock().unwrap();
        script.opens_chat = true;
        script.send_fails = true;
    }
    rig.writes.start_first_prompts();
    let answer = rig.create(Some(PROMPT), Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");

    rig.wait("the send to be tried", |rig| !rig.sent().is_empty())
        .await;
    assert_eq!(rig.calls()[0].0.session_id.as_deref(), Some(NEW_CHAT));
    assert_eq!(rig.stored_agent(), Some(effort_high().to_json()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_patch_fails_the_send_with_its_text_and_nothing_is_typed() {
    // Ready, so every failed send counts; the third one fails the entry.
    let rig = Rig::new("ready", true);
    let error = UiError::NoModel("nope".to_owned());
    rig.fallback(Step::Fail(error.clone()));
    rig.writes.start_first_prompts();
    let answer = rig.create(Some(PROMPT), Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");

    rig.wait("the entry to fail", |rig| {
        rig.entry().is_some_and(|entry| entry["status"] == "failed")
    })
    .await;
    let entry = rig.entry().unwrap();
    assert_eq!(entry["error"], error.to_string());
    assert_eq!(rig.calls().len(), 3);
    assert!(rig.sent().is_empty(), "send_prompt must not be called");
    assert!(!rig.events().contains(&"send_prompt"));
    assert_eq!(rig.stored_agent(), Some(effort_high().to_json()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_patch_leaves_the_entry_waiting_and_the_agent_stored() {
    let rig = Rig::new("setting_up", true);
    rig.fallback(Step::Fail(UiError::Locked));
    rig.writes.start_first_prompts();
    let answer = rig.create(Some(PROMPT), Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");

    // Tried again by the pump, and still locked.
    rig.wait("a second try", |rig| rig.calls().len() >= 2).await;
    let entry = rig.entry().expect("the entry is still listed");
    assert_eq!(entry["status"], "waiting");
    assert_eq!(entry["attempts"], 0);
    assert_eq!(entry["earlyAttempts"], 0);
    assert!(rig.sent().is_empty());
    assert_eq!(rig.stored_agent(), Some(effort_high().to_json()));
}

// ---- without a prompt the agent is applied at once ----

#[tokio::test(flavor = "multi_thread")]
async fn create_without_a_prompt_applies_the_agent_to_the_new_chat() {
    let rig = Rig::new("setting_up", true);
    let answer = rig.create(None, Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["configured"], true);
    assert!(answer.body.get("warning").is_none(), "{answer:?}");
    let calls = rig.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.session_id.as_deref(), Some(NEW_CHAT));
    assert_eq!(calls[0].1, effort_high());
    assert_eq!(rig.stored_agent(), None);
    assert!(rig.entry().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn create_without_a_prompt_and_a_failing_patch_warns_and_still_creates() {
    let rig = Rig::new("setting_up", true);
    let error = UiError::NoModel("nope".to_owned());
    rig.fallback(Step::Fail(error.clone()));
    let answer = rig.create(None, Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["workspaceId"], CREATED);
    assert_eq!(answer.body["configured"], false);
    assert_eq!(answer.body["warning"], error.to_string());
    assert_eq!(rig.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_without_a_prompt_on_a_locked_mac_warns_with_the_lock_text() {
    let rig = Rig::new("setting_up", true);
    rig.fallback(Step::Fail(UiError::Locked));
    let answer = rig.create(None, Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["configured"], false);
    assert_eq!(answer.body["warning"], LOCKED);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_without_a_prompt_and_no_chat_yet_warns() {
    let rig = Rig::new("setting_up", false);
    let answer = rig.create(None, Some(effort_high())).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["workspaceId"], CREATED);
    assert_eq!(answer.body["configured"], false);
    assert_eq!(answer.body["warning"], NO_CHAT_YET);
    assert!(rig.calls().is_empty());
}

// ---- without an agent nothing changes ----

#[tokio::test(flavor = "multi_thread")]
async fn create_without_an_agent_never_calls_set_agent() {
    let rig = Rig::new("setting_up", true);
    let answer = rig.create(None, None).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["configured"], false);
    assert!(answer.body.get("warning").is_none(), "{answer:?}");
    assert!(rig.calls().is_empty());

    // A prompt goes out without any agent step.
    let rig = Rig::new("setting_up", true);
    rig.writes.start_first_prompts();
    let answer = rig.create(Some(PROMPT), None).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    rig.wait("the first prompt to be sent", |rig| {
        rig.entry().is_none() && !rig.sent().is_empty()
    })
    .await;
    assert_eq!(rig.events(), ["send_prompt"]);
    assert!(rig.calls().is_empty());
}
