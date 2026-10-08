//! The agent-settings writes over a stub driver and a synthetic database: patch, new chat for a
//! model, retries, and the model list. The stub plays Conductor by inserting rows; nothing here
//! reaches the Mac.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::agent::{AgentPatch, Effort};
use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteTimings, Writes};
use conductor_remote::delivery::{WriteAnswer, WriteService};
use conductor_remote::reads::{HostPaths, Reads};
use conductor_remote::state::store::Store;
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
/// The chat the writes target.
const CHAT: &str = "s-2";
const HIDDEN: &str = "s-hidden";
const SEVERAL: &str =
    "more than one new chat appeared; refusing to guess which one this request opened";
const NO_NEW_CHAT: &str =
    "Conductor did not confirm the new chat for the model. Check the workspace before trying again.";

/// What the stub does for one `set_agent` call.
#[derive(Clone, Debug)]
struct Step {
    /// The error, when the call fails.
    error: Option<UiError>,
    /// The model item that opens a new chat was pressed.
    new_chat: bool,
    /// How many chats Conductor opens.
    opens: usize,
    /// Write the patch's effort, plan and fast into the chat's row, as Conductor does.
    follows: bool,
}

impl Step {
    fn ok() -> Step {
        Step {
            error: None,
            new_chat: false,
            opens: 0,
            follows: false,
        }
    }

    fn opening(opens: usize) -> Step {
        Step {
            new_chat: true,
            opens,
            ..Step::ok()
        }
    }

    fn failing(error: UiError) -> Step {
        Step {
            error: Some(error),
            ..Step::ok()
        }
    }
}

#[derive(Default)]
struct Script {
    steps: VecDeque<Step>,
    calls: Vec<(Target, AgentPatch)>,
    models: Option<Result<Vec<String>, UiError>>,
    listed: Vec<Target>,
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

    fn list_models(&mut self, target: &Target) -> Result<Vec<String>, UiError> {
        let mut script = self.script.lock().unwrap();
        script.listed.push(target.clone());
        script.models.clone().expect("a scripted model list")
    }

    fn set_agent(
        &mut self,
        target: &Target,
        patch: &AgentPatch,
    ) -> Result<AgentOutcome, AgentFailure> {
        let mut script = self.script.lock().unwrap();
        script.calls.push((target.clone(), patch.clone()));
        let step = script.steps.pop_front().unwrap_or_else(Step::ok);
        for _ in 0..step.opens {
            script.opened += 1;
            self.conn
                .execute(
                    "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, \
                     agent_type, created_at, updated_at) \
                     VALUES (?1, ?2, 'Untitled', 'idle', 0, 'codex', ?3, ?3)",
                    params![
                        format!("s-new-{}", script.opened),
                        WORKSPACE,
                        format!("2026-09-02 10:00:0{}", script.opened)
                    ],
                )
                .unwrap();
        }
        if step.follows {
            let id = target.session_id.as_deref().unwrap();
            if let Some(effort) = patch.effort {
                self.conn
                    .execute(
                        "UPDATE sessions SET claude_effort_level = ?1 WHERE id = ?2",
                        params![effort.as_str(), id],
                    )
                    .unwrap();
            }
            if let Some(plan) = patch.plan {
                let mode = if plan { "plan" } else { "default" };
                self.conn
                    .execute(
                        "UPDATE sessions SET permission_mode = ?1 WHERE id = ?2",
                        params![mode, id],
                    )
                    .unwrap();
            }
            if let Some(fast) = patch.fast {
                self.conn
                    .execute(
                        "UPDATE sessions SET fast_mode = ?1 WHERE id = ?2",
                        params![i64::from(fast), id],
                    )
                    .unwrap();
            }
        }
        match step.error {
            Some(error) => Err(AgentFailure {
                error,
                new_chat: step.new_chat,
            }),
            None => Ok(AgentOutcome {
                new_chat: step.new_chat,
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

/// One live workspace with the open chats of `CHATS` (Claude chats) and one closed chat.
fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', 'relay')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state) \
         VALUES (?1, ?1, 'r-1', 'user/feature-x', 'beta', 'ready')",
        [WORKSPACE],
    )
    .unwrap();
    let chats = [
        (CHATS[0].0, CHATS[0].1, 0, "2026-09-01 10:00:00"),
        (CHATS[1].0, CHATS[1].1, 0, "2026-09-01 10:01:00"),
        (HIDDEN, "Closed", 1, "2026-09-01 10:02:00"),
    ];
    for (id, title, hidden, created_at) in chats {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, agent_type, \
             created_at, updated_at) VALUES (?1, ?2, ?3, 'idle', ?4, 'claude', ?5, ?5)",
            params![id, WORKSPACE, title, hidden, created_at],
        )
        .unwrap();
    }
}

struct Rig {
    _test: TestDb,
    _home: TempDir,
    state: TempDir,
    _ui: UiHandle,
    script: Shared,
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
        let writes = Writes::new(reads, ui.clone(), Arc::new(|| true), timings(), parked);
        Rig {
            _test: test,
            _home: home,
            state,
            _ui: ui,
            script,
            writes,
        }
    }

    fn script(&self, steps: impl IntoIterator<Item = Step>) {
        self.script.lock().unwrap().steps.extend(steps);
    }

    fn calls(&self) -> Vec<(Target, AgentPatch)> {
        self.script.lock().unwrap().calls.clone()
    }

    async fn set(&self, session_id: &str, patch: AgentPatch) -> WriteAnswer {
        self.writes
            .set_agent(session_id.to_owned(), None, patch, Priority::Interactive)
            .await
    }

    async fn models(&self, session_id: &str) -> WriteAnswer {
        self.writes
            .list_models(session_id.to_owned(), None, Priority::Interactive)
            .await
    }
}

fn model(name: &str) -> AgentPatch {
    AgentPatch {
        model: Some(name.to_owned()),
        ..AgentPatch::default()
    }
}

fn model_and_effort(name: &str, effort: Effort) -> AgentPatch {
    AgentPatch {
        effort: Some(effort),
        ..model(name)
    }
}

fn effort(effort: Effort) -> AgentPatch {
    AgentPatch {
        effort: Some(effort),
        ..AgentPatch::default()
    }
}

fn failure_body(error: &str) -> Value {
    json!({ "ok": false, "strategy": "accessibility", "error": error })
}

// ---- locating the chat ----

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_chat_is_404() {
    let rig = Rig::new();
    let answer = rig.set("nope", effort(Effort::High)).await;
    assert_eq!(answer.status, 404, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "workspace for session not found" })
    );
    assert!(rig.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hidden_chat_is_409() {
    let rig = Rig::new();
    let answer = rig.set(HIDDEN, effort(Effort::High)).await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "chat is no longer one of the workspace\u{2019}s tabs" })
    );
    assert!(rig.calls().is_empty());
}

// ---- set_agent ----

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_patch_answers_200_with_the_requested_chat() {
    let rig = Rig::new();
    let patch = AgentPatch {
        effort: Some(Effort::High),
        plan: Some(true),
        ..AgentPatch::default()
    };
    let answer = rig.set(CHAT, patch.clone()).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["strategy"], "accessibility");
    assert_eq!(answer.body["sessionId"], CHAT);
    assert_eq!(answer.body["session"]["id"], CHAT);

    let calls = rig.calls();
    assert_eq!(calls.len(), 1);
    let (target, seen) = &calls[0];
    assert_eq!(seen, &patch);
    assert_eq!(target.workspace_id, WORKSPACE);
    assert_eq!(target.session_id.as_deref(), Some(CHAT));
    let tab = target.tab.as_ref().expect("the chat's tab");
    assert_eq!(
        (tab.index, tab.count, tab.title.as_deref()),
        (2, 2, Some("Two"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_answer_waits_for_the_database_to_follow() {
    let rig = Rig::new();
    rig.script([Step {
        follows: true,
        ..Step::ok()
    }]);
    let patch = AgentPatch {
        effort: Some(Effort::Max),
        plan: Some(true),
        fast: Some(true),
        ..AgentPatch::default()
    };
    let answer = rig.set(CHAT, patch).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let session = &answer.body["session"];
    assert_eq!(session["claude_effort_level"], "max");
    assert_eq!(session["permission_mode"], "plan");
    assert_eq!(session["fast_mode"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_mac_answers_502_with_the_lock_text() {
    let rig = Rig::new();
    rig.script([Step::failing(UiError::Locked)]);
    let answer = rig.set(CHAT, effort(Effort::Low)).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(answer.body, failure_body(&UiError::Locked.to_string()));
    assert!(answer.body["error"]
        .as_str()
        .unwrap()
        .starts_with("The Mac is locked"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_model_answers_502() {
    let rig = Rig::new();
    rig.script([Step::failing(UiError::NoModel("X".to_owned()))]);
    let answer = rig.set(CHAT, model("X")).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({
            "ok": false,
            "strategy": "accessibility",
            "error": "Conductor's model list has no model named X",
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_chat_for_the_model_answers_its_id() {
    let rig = Rig::new();
    rig.script([Step::opening(1)]);
    let answer = rig.set(CHAT, model("GPT-5")).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["sessionId"], "s-new-1");
    assert_eq!(answer.body["session"]["id"], "s-new-1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_chat_that_never_shows_answers_502() {
    let rig = Rig::new();
    rig.script([Step::opening(0)]);
    let answer = rig.set(CHAT, model("GPT-5")).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(answer.body, failure_body(NO_NEW_CHAT));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_new_chats_answer_502() {
    let rig = Rig::new();
    rig.script([Step::opening(2)]);
    let answer = rig.set(CHAT, model("GPT-5")).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(answer.body, failure_body(SEVERAL));
    // Nothing is remembered: the retry would not know which chat to go on in.
    rig.script([Step::ok()]);
    rig.set(CHAT, model("GPT-5")).await;
    assert_eq!(rig.calls()[1].1.model.as_deref(), Some("GPT-5"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_effort_after_a_new_chat_is_retried_in_that_chat() {
    let rig = Rig::new();
    rig.script([Step {
        error: Some(UiError::EffortNotApplied("high".to_owned())),
        ..Step::opening(1)
    }]);
    let patch = model_and_effort("GPT-5", Effort::High);

    let answer = rig.set(CHAT, patch.clone()).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        failure_body("Conductor did not set the effort to high")
    );

    let answer = rig.set(CHAT, patch).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["sessionId"], "s-new-1");

    let calls = rig.calls();
    assert_eq!(calls.len(), 2);
    let (target, seen) = &calls[1];
    assert_eq!(target.session_id.as_deref(), Some("s-new-1"));
    let tab = target.tab.as_ref().expect("the new chat's tab");
    assert_eq!((tab.index, tab.count), (3, 3));
    assert_eq!(seen.model, None);
    assert_eq!(seen.effort, Some(Effort::High));
    assert_eq!(calls.iter().filter(|(_, p)| p.model.is_some()).count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_model_does_not_open_another_chat() {
    let rig = Rig::new();
    rig.script([Step::opening(1)]);
    let first = rig.set(CHAT, model("GPT-5")).await;
    assert_eq!(first.body["sessionId"], "s-new-1");

    // The model is compared lower-cased.
    let second = rig.set(CHAT, model("gpt-5")).await;
    assert_eq!(second.status, 200, "{second:?}");
    assert_eq!(second.body["sessionId"], "s-new-1");
    assert_eq!(second.body["session"]["id"], "s-new-1");
    assert_eq!(rig.calls().len(), 1, "no second UI job for the model");

    // With more in the patch, the UI job runs in the chat that was opened, without the model.
    let third = rig.set(CHAT, model_and_effort("GPT-5", Effort::Low)).await;
    assert_eq!(third.status, 200, "{third:?}");
    assert_eq!(third.body["sessionId"], "s-new-1");
    let calls = rig.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].0.session_id.as_deref(), Some("s-new-1"));
    assert_eq!(calls[1].1, effort(Effort::Low));
}

// ---- list_models ----

#[tokio::test(flavor = "multi_thread")]
async fn models_are_listed_and_cached() {
    let rig = Rig::new();
    rig.script_models(Ok(vec!["Opus".to_owned(), "Sonnet".to_owned()]));
    let answer = rig.models(CHAT).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": true, "models": ["Opus", "Sonnet"], "defaultModel": "Opus" })
    );

    let listed = rig.script.lock().unwrap().listed.clone();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id.as_deref(), Some(CHAT));
    assert_eq!(listed[0].tab.as_ref().map(|tab| tab.index), Some(2));

    let cache: Value = serde_json::from_str(
        &std::fs::read_to_string(rig.state.path().join("model-cache.json")).unwrap(),
    )
    .unwrap();
    let groups = cache.as_array().expect("a list of groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["agentType"], "claude");
    assert_eq!(groups[0]["models"], json!(["Opus", "Sonnet"]));
    assert_eq!(groups[0]["defaultModel"], "Opus");
}

#[tokio::test(flavor = "multi_thread")]
async fn models_of_an_unknown_chat_are_404_and_of_a_hidden_chat_409() {
    let rig = Rig::new();
    assert_eq!(rig.models("nope").await.status, 404);
    assert_eq!(rig.models(HIDDEN).await.status, 409);
    assert!(rig.script.lock().unwrap().listed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_model_list_answers_502() {
    let rig = Rig::new();
    rig.script_models(Err(UiError::NoModelPicker));
    let answer = rig.models(CHAT).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        failure_body("couldn't find the model picker in Conductor's composer")
    );
    assert!(!rig.state.path().join("model-cache.json").exists());
}

impl Rig {
    fn script_models(&self, models: Result<Vec<String>, UiError>) {
        self.script.lock().unwrap().models = Some(models);
    }
}
