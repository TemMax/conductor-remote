//! The agent settings of a prompt over a stub driver and a synthetic database: applied before
//! typing, a new chat for a model takes the prompt, and a locked Mac parks both together. The
//! stub plays Conductor by inserting rows; nothing here reaches the Mac.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::agent::{AgentPatch, Effort};
use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedOutcome, ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteTimings, Writes};
use conductor_remote::delivery::{SendRequest, WriteAnswer, WriteService};
use conductor_remote::reads::{HostPaths, Reads};
use conductor_remote::state::store::{ParkedRow, Store};
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::{
    AgentFailure, AgentOutcome, Target, UiDriver, UiError, ViewReport,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;

const WORKSPACE: &str = "ws-1";
/// The open chats of the workspace, in tab order: (id, title).
const CHATS: [(&str, &str); 2] = [("s-1", "One"), ("s-2", "Two")];
/// The chat the prompts are sent to.
const CHAT: &str = "s-2";
/// The chat Conductor opens for a model of another provider.
const NEW_CHAT: &str = "s-new-1";
const PROMPT: &str = "fix the tests";
const NO_MODEL: &str = "Conductor's model list has no model named gpt-9";

/// What the stub does for one `set_agent` call.
#[derive(Clone, Debug, Default)]
struct Step {
    /// The error, when the call fails.
    error: Option<UiError>,
    /// The model item that opens a new chat was pressed, and the chat is added.
    opens: bool,
}

impl Step {
    fn ok() -> Step {
        Step::default()
    }

    fn opening() -> Step {
        Step {
            opens: true,
            ..Step::default()
        }
    }

    fn failing(error: UiError) -> Step {
        Step {
            error: Some(error),
            ..Step::default()
        }
    }
}

#[derive(Default)]
struct Script {
    steps: VecDeque<Step>,
    /// `set_agent:<chat>` and `send_prompt:<chat>`, in the order the UI thread ran them.
    events: Vec<String>,
    patches: Vec<AgentPatch>,
    /// Every `send_prompt` fails while set.
    fail_sends: bool,
    typed: usize,
    /// How many new chats the stub has opened.
    opened: usize,
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
        let chat = target.session_id.clone().expect("a chat target");
        script.events.push(format!("send_prompt:{chat}"));
        if script.fail_sends {
            return Err(UiError::NoComposer);
        }
        script.typed += 1;
        self.conn
            .execute(
                "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                 VALUES (?1, ?2, 'user', ?3, ?4)",
                params![
                    format!("m-{}", script.typed),
                    chat,
                    text,
                    format!("turn-{}", script.typed)
                ],
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

    fn open_link(&mut self, _: &str) -> Result<(), UiError> {
        Ok(())
    }

    fn set_agent(
        &mut self,
        target: &Target,
        patch: &AgentPatch,
    ) -> Result<AgentOutcome, AgentFailure> {
        let mut script = self.script.lock().unwrap();
        let chat = target.session_id.clone().expect("a chat target");
        script.events.push(format!("set_agent:{chat}"));
        script.patches.push(patch.clone());
        let step = script.steps.pop_front().unwrap_or_else(Step::ok);
        if step.opens {
            script.opened += 1;
            // The first new chat is NEW_CHAT; a later one gets its own id.
            let id = if script.opened == 1 {
                NEW_CHAT.to_owned()
            } else {
                format!("s-new-{}", script.opened)
            };
            self.conn
                .execute(
                    "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, \
                     agent_type, created_at, updated_at) \
                     VALUES (?1, ?2, 'Untitled', 'idle', 0, 'codex', \
                     '2026-09-02 10:00:01', '2026-09-02 10:00:01')",
                    params![id, WORKSPACE],
                )
                .unwrap();
        }
        match step.error {
            Some(error) => Err(AgentFailure {
                error,
                new_chat: step.opens,
            }),
            None => Ok(AgentOutcome {
                new_chat: step.opens,
            }),
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
        stop_poll: ms(5),
        stop_checks: 5,
        chat_poll: ms(5),
        chat_checks: 3,
        send_budget: Some(ms(400)),
        restore_poll: ms(10),
        restore_checks: 3,
        create_poll: ms(10),
        create_checks: 3,
    }
}

/// One live workspace with the open chats of `CHATS`.
fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', 'relay')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state) \
         VALUES (?1, ?1, 'r-1', 'user/feature-x', 'beta', 'ready')",
        [WORKSPACE],
    )
    .unwrap();
    for (index, (id, title)) in CHATS.iter().enumerate() {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, agent_type, \
             created_at, updated_at) VALUES (?1, ?2, ?3, 'idle', 0, 'claude', ?4, ?4)",
            params![id, WORKSPACE, title, format!("2026-09-01 10:0{index}:00")],
        )
        .unwrap();
    }
}

struct Rig {
    test: TestDb,
    _home: TempDir,
    _state: TempDir,
    _ui: UiHandle,
    script: Shared,
    reads: Arc<Reads>,
    parked: Arc<ParkedQueue>,
    writes: Writes,
}

impl Rig {
    fn new() -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        let home = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let script = Shared::default();
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
        let reads = Arc::new(
            Reads::new(test.db(), test.root()).with_host_paths(HostPaths {
                home: home.path().to_path_buf(),
                state_dir: state.path().to_path_buf(),
            }),
        );
        let parked = ParkedQueue::new(
            Arc::new(Store::open_in_memory().expect("an in-memory store")),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let writes = Writes::new(
            Arc::clone(&reads),
            ui.clone(),
            Arc::new(|| true),
            timings(),
            Arc::clone(&parked),
        );
        Rig {
            test,
            _home: home,
            _state: state,
            _ui: ui,
            script,
            reads,
            parked,
            writes,
        }
    }

    fn script(&self, steps: impl IntoIterator<Item = Step>) {
        self.script.lock().unwrap().steps.extend(steps);
    }

    fn events(&self) -> Vec<String> {
        self.script.lock().unwrap().events.clone()
    }

    fn fail_sends(&self, fail: bool) {
        self.script.lock().unwrap().fail_sends = fail;
    }

    async fn send(&self, agent: Option<AgentPatch>) -> WriteAnswer {
        self.writes
            .send_prompt(SendRequest {
                session_id: CHAT.to_owned(),
                text: PROMPT.to_owned(),
                workspace_id: None,
                client_id: None,
                queue: false,
                client_timeout_ms: None,
                priority: Priority::Interactive,
                agent,
            })
            .await
    }

    /// Parks `PROMPT` for `CHAT` with `agent` and the chat's cursor as it is now.
    fn park(&self, agent: &AgentPatch) -> ParkedRow {
        let cursor = self.reads.delivery_cursor(CHAT).expect("the cursor");
        self.parked
            .park_with_agent(WORKSPACE, CHAT, PROMPT, false, &cursor, 1, Some(agent))
            .expect("parked")
    }

    /// How many user rows of `PROMPT` the chat has.
    fn typed_in(&self, chat: &str) -> i64 {
        self.test
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM session_messages \
                 WHERE session_id = ?1 AND role = 'user' AND content = ?2",
                params![chat, PROMPT],
                |row| row.get(0),
            )
            .unwrap()
    }
}

fn model(name: &str) -> AgentPatch {
    AgentPatch {
        model: Some(name.to_owned()),
        ..AgentPatch::default()
    }
}

fn patch_json(patch: &AgentPatch) -> Value {
    serde_json::to_value(patch).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_without_agent_never_sets_the_agent() {
    let rig = Rig::new();
    let answer = rig.send(None).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(rig.events(), [format!("send_prompt:{CHAT}")]);
    assert!(answer.body.get("sessionId").is_none(), "{answer:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_patch_is_set_before_the_prompt_is_typed() {
    let rig = Rig::new();
    let answer = rig.send(Some(model("claude-opus"))).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], json!(true));
    assert!(answer.body.get("sessionId").is_none(), "{answer:?}");
    assert_eq!(
        rig.events(),
        [format!("set_agent:{CHAT}"), format!("send_prompt:{CHAT}")]
    );
    assert_eq!(rig.typed_in(CHAT), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_patch_answers_its_text_and_types_nothing() {
    let rig = Rig::new();
    rig.script([Step::failing(UiError::NoModel("gpt-9".to_owned()))]);
    let answer = rig.send(Some(model("gpt-9"))).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(answer.body["error"], json!(NO_MODEL));
    assert_eq!(rig.events(), [format!("set_agent:{CHAT}")]);
    assert_eq!(rig.typed_in(CHAT), 0);
    assert!(rig.parked.list().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_chat_for_the_model_takes_the_prompt() {
    let rig = Rig::new();
    rig.script([Step::opening()]);
    let answer = rig.send(Some(model("gpt-5"))).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], json!(true));
    assert_eq!(answer.body["sessionId"], json!(NEW_CHAT));
    assert_eq!(rig.typed_in(NEW_CHAT), 1);
    assert_eq!(rig.typed_in(CHAT), 0);
    assert_eq!(
        rig.events(),
        [
            format!("set_agent:{CHAT}"),
            format!("send_prompt:{NEW_CHAT}")
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delivered_prompt_ends_the_memo_of_its_new_chat() {
    let rig = Rig::new();
    rig.script([Step::opening(), Step::opening()]);
    let first = rig.send(Some(model("gpt-5"))).await;
    assert_eq!(first.body["sessionId"], json!(NEW_CHAT), "{first:?}");

    let second = rig.send(Some(model("gpt-5"))).await;
    assert_eq!(second.status, 200, "{second:?}");
    assert_eq!(second.body["sessionId"], json!("s-new-2"), "{second:?}");
    let patches = rig.script.lock().unwrap().patches.clone();
    assert_eq!(patches.len(), 2);
    assert_eq!(patches[1].model.as_deref(), Some("gpt-5"));
    assert_eq!(
        rig.events(),
        [
            format!("set_agent:{CHAT}"),
            format!("send_prompt:{NEW_CHAT}"),
            format!("set_agent:{CHAT}"),
            "send_prompt:s-new-2".to_owned(),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_delivery_keeps_the_memo_for_the_retry() {
    let rig = Rig::new();
    let patch = AgentPatch {
        effort: Some(Effort::High),
        ..model("gpt-5")
    };
    rig.script([Step::opening()]);
    rig.fail_sends(true);
    let failed = rig.send(Some(patch.clone())).await;
    assert_ne!(failed.status, 200, "{failed:?}");

    rig.fail_sends(false);
    let retry = rig.send(Some(patch.clone())).await;
    assert_eq!(retry.status, 200, "{retry:?}");
    assert_eq!(retry.body["sessionId"], json!(NEW_CHAT));
    let patches = rig.script.lock().unwrap().patches.clone();
    assert_eq!(patches.len(), 2);
    assert_eq!(
        patches[1].model, None,
        "the retry does not ask for the model again"
    );
    assert_eq!(
        rig.events()
            .iter()
            .filter(|e| e.as_str() == format!("set_agent:{CHAT}"))
            .count(),
        1,
        "{:?}",
        rig.events()
    );
    assert!(
        rig.events().contains(&format!("set_agent:{NEW_CHAT}")),
        "{:?}",
        rig.events()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lock_at_the_patch_parks_the_prompt_with_it() {
    let rig = Rig::new();
    rig.script([Step::failing(UiError::Locked)]);
    let patch = model("claude-opus");
    let answer = rig.send(Some(patch.clone())).await;
    assert_eq!(answer.status, 202, "{answer:?}");
    assert_eq!(answer.body["parked"], json!(true));
    assert_eq!(answer.body["queued"]["agent"], patch_json(&patch));
    assert_eq!(answer.body["queued"]["sessionId"], json!(CHAT));

    let listed = rig.writes.parked_prompts();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["agent"], patch_json(&patch));
    assert_eq!(rig.events(), [format!("set_agent:{CHAT}")]);
    assert_eq!(rig.typed_in(CHAT), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_patch_is_set_then_the_prompt_typed_and_the_patch_cleared() {
    let rig = Rig::new();
    rig.script([Step::failing(UiError::Locked)]);
    let patch = model("claude-opus");
    assert_eq!(rig.send(Some(patch.clone())).await.status, 202);
    let row = rig.parked.list().remove(0);
    assert_eq!(rig.parked.agent(row.id), Some(patch.clone()));

    let outcome = rig.writes.deliver_parked(row.clone()).await;
    assert_eq!(outcome, ParkedOutcome::Delivered);
    assert_eq!(
        rig.events(),
        [
            format!("set_agent:{CHAT}"),
            format!("set_agent:{CHAT}"),
            format!("send_prompt:{CHAT}")
        ]
    );
    assert_eq!(rig.typed_in(CHAT), 1);
    assert_eq!(rig.parked.agent(row.id), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_patch_that_opened_a_chat_is_kept_until_the_prompt_lands_there() {
    let rig = Rig::new();
    let patch = model("gpt-5");
    let row = rig.park(&patch);
    rig.script([Step::opening()]);
    rig.fail_sends(true);

    let outcome = rig.writes.deliver_parked(row.clone()).await;
    assert!(matches!(outcome, ParkedOutcome::Failed(_)), "{outcome:?}");
    assert_eq!(rig.parked.agent(row.id), Some(patch.clone()));
    assert_eq!(rig.typed_in(NEW_CHAT), 0);

    rig.fail_sends(false);
    let outcome = rig.writes.deliver_parked(row.clone()).await;
    assert_eq!(outcome, ParkedOutcome::Delivered);
    assert_eq!(rig.typed_in(NEW_CHAT), 1);
    assert_eq!(rig.typed_in(CHAT), 0);
    let sets = rig
        .events()
        .iter()
        .filter(|event| event.starts_with("set_agent"))
        .count();
    assert_eq!(
        sets,
        1,
        "the model is not asked for twice: {:?}",
        rig.events()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_patch_that_fails_fails_the_delivery() {
    let rig = Rig::new();
    let row = rig.park(&model("gpt-9"));
    rig.script([Step::failing(UiError::NoModel("gpt-9".to_owned()))]);
    let outcome = rig.writes.deliver_parked(row).await;
    assert_eq!(outcome, ParkedOutcome::Failed(NO_MODEL.to_owned()));
    assert_eq!(rig.events(), [format!("set_agent:{CHAT}")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_patch_meeting_the_lock_again_waits() {
    let rig = Rig::new();
    let patch = model("claude-opus");
    let row = rig.park(&patch);
    rig.script([Step::failing(UiError::Locked)]);
    let outcome = rig.writes.deliver_parked(row.clone()).await;
    assert_eq!(outcome, ParkedOutcome::Locked);
    assert_eq!(rig.parked.agent(row.id), Some(patch));
    assert_eq!(rig.events(), [format!("set_agent:{CHAT}")]);
    assert_eq!(rig.typed_in(CHAT), 0);
}
