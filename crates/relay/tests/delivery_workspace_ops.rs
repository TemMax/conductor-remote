//! Close chat, workspace status, archive and Continue over a stub driver and a synthetic
//! database. The stub plays Conductor by updating rows; nothing here reaches the Mac.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteTimings, Writes};
use conductor_remote::delivery::{WriteAnswer, WriteService};
use conductor_remote::reads::{HostPaths, Reads};
use conductor_remote::state::store::Store;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::{Target, UiDriver, UiError, ViewReport};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;

const WORKSPACE: &str = "ws-1";
/// The open chats of the workspace, in tab order: (id, title).
const CHATS: [(&str, &str); 2] = [("s-1", "One"), ("s-2", "Two")];
const HIDDEN: &str = "s-hidden";
const BRANCH: &str = "user/feature-x";
const NOT_A_TAB: &str = "chat is no longer one of the workspace\u{2019}s tabs";
const AGENT_RUNNING: &str = "The agent is still working in this chat. Confirm closing it anyway.";
const STILL_OPEN: &str =
    "Conductor took the close but the chat tab is still open. Try again, or close it on your Mac.";
const STILL_IN_SIDEBAR: &str = "Conductor took the archive but the workspace is still in the sidebar. Try again, or archive it on your Mac.";
const NO_NEW_BRANCH: &str =
    "Conductor did not record a new branch within 30 seconds. Check it on your Mac before retrying.";
const NOT_RECORDED: &str =
    "Conductor didn\u{2019}t record the change \u{2014} it may have been asleep. Try again.";

/// What the stub does for one call: fail, or play Conductor with a bit of SQL, or both.
#[derive(Clone, Debug, Default)]
struct Step {
    error: Option<UiError>,
    sql: Option<&'static str>,
}

impl Step {
    fn does(sql: &'static str) -> Step {
        Step {
            sql: Some(sql),
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

/// A command the stub was given.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Close(Target, bool),
    Status(Target, String, String),
    Archive(Target, bool),
    Continue(Target),
}

#[derive(Default)]
struct Script {
    close: VecDeque<Step>,
    status: VecDeque<Step>,
    archive: VecDeque<Step>,
    cont: VecDeque<Step>,
    calls: Vec<Call>,
}

type Shared = Arc<Mutex<Script>>;

struct Stub {
    script: Shared,
    conn: Connection,
}

impl Stub {
    fn play(&self, step: Option<Step>) -> Result<(), UiError> {
        let step = step.unwrap_or_default();
        if let Some(sql) = step.sql {
            self.conn.execute_batch(sql).unwrap();
        }
        step.error.map_or(Ok(()), Err)
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

    fn open_link(&mut self, _: &str) -> Result<(), UiError> {
        Ok(())
    }

    fn close_chat(&mut self, target: &Target, confirm: bool) -> Result<(), UiError> {
        let step = {
            let mut script = self.script.lock().unwrap();
            script.calls.push(Call::Close(target.clone(), confirm));
            script.close.pop_front()
        };
        self.play(step)
    }

    fn set_status(&mut self, target: &Target, row: &str, label: &str) -> Result<(), UiError> {
        let step = {
            let mut script = self.script.lock().unwrap();
            script.calls.push(Call::Status(
                target.clone(),
                row.to_owned(),
                label.to_owned(),
            ));
            script.status.pop_front()
        };
        self.play(step)
    }

    fn archive(&mut self, target: &Target, confirm: bool) -> Result<(), UiError> {
        let step = {
            let mut script = self.script.lock().unwrap();
            script.calls.push(Call::Archive(target.clone(), confirm));
            script.archive.pop_front()
        };
        self.play(step)
    }

    fn press_continue(&mut self, target: &Target) -> Result<(), UiError> {
        let step = {
            let mut script = self.script.lock().unwrap();
            script.calls.push(Call::Continue(target.clone()));
            script.cont.pop_front()
        };
        self.play(step)
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
        stop_poll: ms(2),
        stop_checks: 5,
        chat_poll: ms(2),
        chat_checks: 3,
        send_budget: Some(ms(400)),
        restore_poll: ms(10),
        restore_checks: 3,
        create_poll: ms(10),
        create_checks: 3,
    }
}

/// One live workspace on `BRANCH` with the open chats of `CHATS`, `s-2` active, and one closed
/// chat.
fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', 'relay')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state, \
         active_session_id) VALUES (?1, ?1, 'r-1', ?2, 'beta', 'ready', 's-2')",
        params![WORKSPACE, BRANCH],
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
    test: TestDb,
    _home: TempDir,
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
            test,
            _home: home,
            _ui: ui,
            script,
            writes,
        }
    }

    /// Plays Conductor outside the stub: the database as the test wants it before the write.
    fn sql(&self, sql: &str) {
        self.test.conn().execute_batch(sql).unwrap();
    }

    fn on_close(&self, step: Step) {
        self.script.lock().unwrap().close.push_back(step);
    }

    fn on_status(&self, step: Step) {
        self.script.lock().unwrap().status.push_back(step);
    }

    fn on_archive(&self, step: Step) {
        self.script.lock().unwrap().archive.push_back(step);
    }

    fn on_continue(&self, step: Step) {
        self.script.lock().unwrap().cont.push_back(step);
    }

    fn calls(&self) -> Vec<Call> {
        self.script.lock().unwrap().calls.clone()
    }

    async fn close(
        &self,
        session_id: &str,
        workspace_id: Option<&str>,
        running: bool,
    ) -> WriteAnswer {
        self.writes
            .close_chat(
                session_id.to_owned(),
                workspace_id.map(str::to_owned),
                running,
                Priority::Interactive,
            )
            .await
    }

    async fn status(&self, workspace_id: &str, status: &str) -> WriteAnswer {
        self.writes
            .set_workspace_status(
                workspace_id.to_owned(),
                status.to_owned(),
                Priority::Interactive,
            )
            .await
    }

    async fn archive(&self, workspace_id: &str, stop_agents: bool) -> WriteAnswer {
        self.writes
            .archive_workspace(workspace_id.to_owned(), stop_agents, Priority::Interactive)
            .await
    }

    async fn proceed(&self, workspace_id: &str, session_id: Option<&str>) -> WriteAnswer {
        self.writes
            .continue_workspace(
                workspace_id.to_owned(),
                session_id.map(str::to_owned),
                Priority::Interactive,
            )
            .await
    }
}

fn answer(status: u16, body: Value) -> WriteAnswer {
    WriteAnswer {
        status,
        body,
        retry_after_secs: None,
    }
}

fn error(status: u16, message: &str) -> WriteAnswer {
    answer(status, json!({ "error": message }))
}

fn failed(message: &str) -> WriteAnswer {
    answer(
        502,
        json!({ "ok": false, "strategy": "accessibility", "error": message }),
    )
}

fn locked() -> WriteAnswer {
    failed(&UiError::Locked.to_string())
}

fn running_in_chat() -> WriteAnswer {
    answer(
        409,
        json!({ "ok": false, "agentRunning": true, "error": AGENT_RUNNING }),
    )
}

fn agents_running(message: &str) -> WriteAnswer {
    answer(
        409,
        json!({ "ok": false, "agentsRunning": true, "error": message }),
    )
}

const HIDE_S2: &str = "UPDATE sessions SET is_hidden = 1 WHERE id = 's-2'";
const WORK_S2: &str = "UPDATE sessions SET status = 'working' WHERE id = 's-2'";
const ARCHIVED: &str = "UPDATE workspaces SET state = 'archived' WHERE id = 'ws-1'";
const NEW_BRANCH: &str = "UPDATE workspaces SET branch = 'user/feature-y' WHERE id = 'ws-1'";

fn workspace_target(session_id: Option<&str>, tab: Option<(usize, &str)>) -> Target {
    Target {
        workspace_id: WORKSPACE.to_owned(),
        session_id: session_id.map(str::to_owned),
        repo: Some("relay".to_owned()),
        branch: BRANCH.to_owned(),
        workspace_name: Some("beta".to_owned()),
        tab: tab.map(|(index, title)| conductor_remote::ui::driver::Tab {
            index,
            count: 2,
            title: Some(title.to_owned()),
        }),
    }
}

// ---- close chat ----

#[tokio::test]
async fn close_refuses_an_unknown_chat() {
    let rig = Rig::new();
    assert_eq!(
        rig.close("nope", None, false).await,
        error(404, "chat not found")
    );
}

#[tokio::test]
async fn close_refuses_a_chat_of_another_workspace() {
    let rig = Rig::new();
    assert_eq!(
        rig.close("s-1", Some("ws-other"), false).await,
        error(409, "chat is not in that workspace")
    );
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn close_refuses_a_chat_whose_workspace_is_not_live() {
    let rig = Rig::new();
    rig.sql(ARCHIVED);
    assert_eq!(
        rig.close("s-1", None, false).await,
        error(404, "workspace for session not found")
    );
}

#[tokio::test]
async fn close_of_a_closed_chat_is_already_closed() {
    let rig = Rig::new();
    assert_eq!(
        rig.close(HIDDEN, Some(WORKSPACE), false).await,
        answer(
            200,
            json!({ "ok": true, "alreadyClosed": true, "activeSessionId": "s-2" })
        )
    );
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn close_names_the_first_chat_when_the_active_one_is_not_open() {
    let rig = Rig::new();
    rig.sql("UPDATE workspaces SET active_session_id = 's-hidden' WHERE id = 'ws-1'");
    assert_eq!(
        rig.close(HIDDEN, None, false).await.body["activeSessionId"],
        "s-1"
    );
}

#[tokio::test]
async fn close_names_null_when_no_chat_is_open() {
    let rig = Rig::new();
    rig.sql("UPDATE sessions SET is_hidden = 1");
    assert_eq!(
        rig.close(HIDDEN, None, false).await,
        answer(
            200,
            json!({ "ok": true, "alreadyClosed": true, "activeSessionId": null })
        )
    );
}

#[tokio::test]
async fn close_asks_to_confirm_a_working_chat() {
    let rig = Rig::new();
    rig.sql(WORK_S2);
    assert_eq!(rig.close("s-2", None, false).await, running_in_chat());
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn close_anyway_presses_confirm_for_a_working_chat() {
    let rig = Rig::new();
    rig.sql(WORK_S2);
    rig.on_close(Step::does(HIDE_S2));
    assert_eq!(
        rig.close("s-2", None, true).await,
        answer(
            200,
            json!({ "ok": true, "strategy": "accessibility", "activeSessionId": "s-1" })
        )
    );
    assert_eq!(
        rig.calls(),
        [Call::Close(
            workspace_target(Some("s-2"), Some((2, "Two"))),
            true
        )]
    );
}

#[tokio::test]
async fn close_turns_the_dialog_into_the_same_409() {
    let rig = Rig::new();
    rig.on_close(Step::failing(UiError::NeedsConfirmation));
    assert_eq!(rig.close("s-1", None, false).await, running_in_chat());
    assert_eq!(
        rig.calls(),
        [Call::Close(
            workspace_target(Some("s-1"), Some((1, "One"))),
            false
        )]
    );
}

#[tokio::test]
async fn close_names_the_active_chat_when_it_is_still_open() {
    let rig = Rig::new();
    rig.on_close(Step::does(
        "UPDATE sessions SET is_hidden = 1 WHERE id = 's-1'",
    ));
    assert_eq!(
        rig.close("s-1", Some(WORKSPACE), false).await,
        answer(
            200,
            json!({ "ok": true, "strategy": "accessibility", "activeSessionId": "s-2" })
        )
    );
}

#[tokio::test]
async fn close_that_never_shows_is_a_502() {
    let rig = Rig::new();
    rig.on_close(Step::default());
    assert_eq!(rig.close("s-1", None, false).await, failed(STILL_OPEN));
}

#[tokio::test]
async fn close_on_a_locked_mac_is_a_502_with_the_lock_text() {
    let rig = Rig::new();
    rig.on_close(Step::failing(UiError::Locked));
    assert_eq!(rig.close("s-1", None, false).await, locked());
}

// ---- workspace status ----

#[tokio::test]
async fn status_refuses_an_unknown_status() {
    let rig = Rig::new();
    assert_eq!(
        rig.status(WORKSPACE, "paused").await,
        error(
            400,
            "status must be one of backlog, in-progress, in-review, done, canceled"
        )
    );
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn status_refuses_an_unknown_workspace() {
    let rig = Rig::new();
    assert_eq!(
        rig.status("ws-none", "done").await,
        error(404, "workspace not found")
    );
}

#[tokio::test]
async fn status_that_is_already_set_skips_the_ui() {
    let rig = Rig::new();
    rig.sql("UPDATE workspaces SET manual_status = 'done' WHERE id = 'ws-1'");
    let answer = rig.status(WORKSPACE, "done").await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["workspace"]["id"], WORKSPACE);
    assert_eq!(answer.body["workspace"]["manual_status"], "done");
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn status_presses_the_menu_item_and_reads_the_workspace_back() {
    let rig = Rig::new();
    rig.on_status(Step::does(
        "UPDATE workspaces SET manual_status = 'in-review' WHERE id = 'ws-1'",
    ));
    let answer = rig.status(WORKSPACE, "in-review").await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["workspace"]["manual_status"], "in-review");
    assert_eq!(answer.body["workspace"]["branch"], BRANCH);
    assert_eq!(
        rig.calls(),
        [Call::Status(
            workspace_target(None, None),
            "beta".to_owned(),
            "In review".to_owned()
        )]
    );
}

#[tokio::test]
async fn status_names_the_menu_label_of_each_status() {
    let rig = Rig::new();
    for (status, label, sql) in [
        (
            "backlog",
            "Backlog",
            "UPDATE workspaces SET manual_status = 'backlog'",
        ),
        (
            "in-progress",
            "In progress",
            "UPDATE workspaces SET manual_status = 'in-progress'",
        ),
        (
            "done",
            "Done",
            "UPDATE workspaces SET manual_status = 'done'",
        ),
        (
            "canceled",
            "Canceled",
            "UPDATE workspaces SET manual_status = 'canceled'",
        ),
    ] {
        rig.sql("UPDATE workspaces SET manual_status = NULL WHERE id = 'ws-1'");
        rig.on_status(Step::does(sql));
        assert_eq!(rig.status(WORKSPACE, status).await.status, 200);
        let calls = rig.calls();
        let Some(Call::Status(_, _, shown)) = calls.last() else {
            panic!("the status was not pressed");
        };
        assert_eq!(shown, label);
    }
}

#[tokio::test]
async fn status_that_records_another_value_names_it() {
    let rig = Rig::new();
    rig.on_status(Step::does(
        "UPDATE workspaces SET manual_status = 'in-progress' WHERE id = 'ws-1'",
    ));
    assert_eq!(
        rig.status(WORKSPACE, "done").await,
        failed("Conductor recorded the status as \u{201c}in-progress\u{201d}, not \u{201c}done\u{201d}.")
    );
}

#[tokio::test]
async fn status_that_records_nothing_is_a_502() {
    let rig = Rig::new();
    rig.on_status(Step::default());
    assert_eq!(rig.status(WORKSPACE, "done").await, failed(NOT_RECORDED));
}

#[tokio::test]
async fn status_on_a_locked_mac_is_a_502_with_the_lock_text() {
    let rig = Rig::new();
    rig.on_status(Step::failing(UiError::Locked));
    assert_eq!(rig.status(WORKSPACE, "done").await, locked());
}

// ---- archive ----

#[tokio::test]
async fn archive_of_an_archived_workspace_is_already_archived() {
    let rig = Rig::new();
    rig.sql(ARCHIVED);
    let answer = rig.archive(WORKSPACE, false).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["alreadyArchived"], true);
    assert_eq!(answer.body["workspace"]["id"], WORKSPACE);
    assert_eq!(answer.body["workspace"]["archived"], true);
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn archive_refuses_an_unknown_workspace() {
    let rig = Rig::new();
    assert_eq!(
        rig.archive("ws-none", false).await,
        error(404, "workspace not found")
    );
}

#[tokio::test]
async fn archive_asks_to_confirm_one_working_agent() {
    let rig = Rig::new();
    rig.sql(WORK_S2);
    assert_eq!(
        rig.archive(WORKSPACE, false).await,
        agents_running("1 agent is still working here. Archiving stops them.")
    );
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn archive_asks_to_confirm_several_working_agents() {
    let rig = Rig::new();
    rig.sql("UPDATE sessions SET status = 'working' WHERE id IN ('s-1', 's-2')");
    assert_eq!(
        rig.archive(WORKSPACE, false).await,
        agents_running("2 agents are still working here. Archiving stops them.")
    );
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn archive_turns_the_dialog_into_a_409() {
    let rig = Rig::new();
    rig.sql("UPDATE sessions SET status = 'waiting' WHERE id = 's-2'");
    rig.on_archive(Step::failing(UiError::NeedsConfirmation));
    assert_eq!(
        rig.archive(WORKSPACE, false).await,
        agents_running("Agents are still working here. Archiving stops them.")
    );
    assert_eq!(
        rig.calls(),
        [Call::Archive(workspace_target(None, None), false)]
    );
}

#[tokio::test]
async fn archive_with_stop_agents_confirms_and_names_the_archived_workspace() {
    let rig = Rig::new();
    rig.sql(WORK_S2);
    rig.on_archive(Step::does(ARCHIVED));
    let answer = rig.archive(WORKSPACE, true).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["strategy"], "accessibility");
    assert_eq!(answer.body["workspace"]["id"], WORKSPACE);
    assert_eq!(answer.body["workspace"]["archived"], true);
    assert_eq!(
        rig.calls(),
        [Call::Archive(workspace_target(None, None), true)]
    );
}

#[tokio::test]
async fn archive_that_never_shows_is_a_502() {
    let rig = Rig::new();
    rig.on_archive(Step::default());
    assert_eq!(
        rig.archive(WORKSPACE, false).await,
        failed(STILL_IN_SIDEBAR)
    );
}

#[tokio::test]
async fn archive_on_a_locked_mac_is_a_502_with_the_lock_text() {
    let rig = Rig::new();
    rig.on_archive(Step::failing(UiError::Locked));
    assert_eq!(rig.archive(WORKSPACE, false).await, locked());
}

// ---- continue ----

#[tokio::test]
async fn continue_refuses_an_unknown_workspace() {
    let rig = Rig::new();
    assert_eq!(
        rig.proceed("ws-none", None).await,
        error(404, "workspace not found")
    );
}

#[tokio::test]
async fn continue_refuses_a_chat_that_is_not_a_tab() {
    let rig = Rig::new();
    assert_eq!(
        rig.proceed(WORKSPACE, Some(HIDDEN)).await,
        error(409, NOT_A_TAB)
    );
    assert!(rig.calls().is_empty());
}

#[tokio::test]
async fn continue_refuses_a_workspace_without_a_branch() {
    for branch in ["NULL", "''"] {
        let rig = Rig::new();
        rig.sql(&format!("UPDATE workspaces SET branch = {branch}"));
        assert_eq!(
            rig.proceed(WORKSPACE, None).await,
            answer(
                409,
                json!({ "ok": false, "error": "workspace has no branch to continue" })
            )
        );
        assert!(rig.calls().is_empty());
    }
}

#[tokio::test]
async fn continue_presses_in_the_given_chat() {
    let rig = Rig::new();
    rig.on_continue(Step::does(NEW_BRANCH));
    assert_eq!(rig.proceed(WORKSPACE, Some("s-1")).await.status, 200);
    assert_eq!(
        rig.calls(),
        [Call::Continue(workspace_target(
            Some("s-1"),
            Some((1, "One"))
        ))]
    );
}

#[tokio::test]
async fn continue_presses_in_the_active_chat_when_none_is_given() {
    let rig = Rig::new();
    rig.on_continue(Step::does(NEW_BRANCH));
    assert_eq!(rig.proceed(WORKSPACE, None).await.status, 200);
    assert_eq!(
        rig.calls(),
        [Call::Continue(workspace_target(
            Some("s-2"),
            Some((2, "Two"))
        ))]
    );
}

#[tokio::test]
async fn continue_presses_on_the_workspace_when_the_active_chat_is_not_open() {
    let rig = Rig::new();
    rig.sql("UPDATE workspaces SET active_session_id = 's-hidden' WHERE id = 'ws-1'");
    rig.on_continue(Step::does(NEW_BRANCH));
    assert_eq!(rig.proceed(WORKSPACE, None).await.status, 200);
    assert_eq!(rig.calls(), [Call::Continue(workspace_target(None, None))]);
}

#[tokio::test]
async fn continue_names_the_previous_branch_and_the_new_workspace() {
    let rig = Rig::new();
    rig.on_continue(Step::does(NEW_BRANCH));
    let answer = rig.proceed(WORKSPACE, None).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["previousBranch"], BRANCH);
    assert_eq!(answer.body["workspace"]["id"], WORKSPACE);
    assert_eq!(answer.body["workspace"]["branch"], "user/feature-y");
}

#[tokio::test]
async fn continue_that_keeps_the_branch_is_a_502() {
    let rig = Rig::new();
    rig.on_continue(Step::default());
    assert_eq!(rig.proceed(WORKSPACE, None).await, failed(NO_NEW_BRANCH));
}

#[tokio::test]
async fn continue_on_a_locked_mac_is_a_502_with_the_lock_text() {
    let rig = Rig::new();
    rig.on_continue(Step::failing(UiError::Locked));
    assert_eq!(rig.proceed(WORKSPACE, None).await, locked());
}
