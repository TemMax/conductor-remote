//! The write service over the fake desktop and a synthetic database: send, stop and new chat,
//! with every answer they give. The fake desktop lives on the UI thread; it writes into the
//! database what Conductor would, and the test sees only what its reactions report. Nothing here
//! reaches the Mac.

mod support;

use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{
    ParkedOutcome, ParkedQueue, ParkedTimings, PARKED_ERROR, PARKED_REASON,
};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{SendRequest, WriteAnswer, WriteService};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::{NewFirstPrompt, ParkedRow, Store};
use conductor_remote::testing::FakeCommands;
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::UiDriver;
use conductor_remote::ui::fake::{conductor_app, main_pane, FakeDesktop, WindowSpec};
use conductor_remote::ui::keys::{Key, Modifiers};
use conductor_remote::ui::screen::SessionState;
use rusqlite::{params, Connection};
use serde_json::json;
use support::TestDb;

const WORKSPACE: &str = "ws-1";
/// The open chats of the workspace, in tab order: (id, title).
const CHATS: [(&str, &str); 2] = [("s-1", "One"), ("s-2", "Two")];
/// The chat the writes target.
const CHAT: &str = "s-2";
/// A closed chat of the same workspace.
const HIDDEN: &str = "s-hidden";
const PROMPT: &str = "fix the tests";
const NOT_A_TAB: &str = "chat is no longer one of the workspace\u{2019}s tabs";

/// What the fake desktop saw, as its reactions report it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Url(String),
    Key(Key, Modifiers),
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// How the fake Conductor answers the keys.
#[derive(Clone, Copy)]
struct Fake {
    /// Return writes the composer's text as a user row of the selected chat.
    lands: bool,
    /// Cmd+Shift+Delete sets the selected chat's status to "idle".
    stops: bool,
    /// How many visible chats Cmd+T opens.
    opens: usize,
    /// The Mac is locked.
    locked: bool,
    /// Posting a key fails.
    fail_keys: bool,
}

impl Default for Fake {
    fn default() -> Fake {
        Fake {
            lands: true,
            stops: true,
            opens: 1,
            locked: false,
            fail_keys: false,
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

/// A parked queue over an in-memory store whose lock probe says unlocked; never started.
fn parked_queue() -> Arc<ParkedQueue> {
    ParkedQueue::new(
        Arc::new(Store::open_in_memory().expect("an in-memory store")),
        Arc::new(|| Some(false)),
        ParkedTimings::default(),
    )
}

/// One live workspace on `user/feature-x` of repo "relay" with the open chats of `CHATS` (the
/// target's status is `status`) and one closed chat.
fn seed(conn: &Connection, status: &str) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', 'relay')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state) \
         VALUES (?1, ?1, 'r-1', 'user/feature-x', 'beta', 'ready')",
        [WORKSPACE],
    )
    .unwrap();
    let chats = [
        (CHATS[0].0, CHATS[0].1, "idle", 0, "2026-09-01 10:00:00"),
        (CHATS[1].0, CHATS[1].1, status, 0, "2026-09-01 10:01:00"),
        (HIDDEN, "Closed", "idle", 1, "2026-09-01 10:02:00"),
    ];
    for (id, title, status, hidden, created_at) in chats {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
             updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, WORKSPACE, title, status, hidden, created_at],
        )
        .unwrap();
    }
}

/// The window showing the target workspace, its first chat selected.
fn spec() -> WindowSpec {
    WindowSpec {
        repo: "relay".to_owned(),
        branch: "user/feature-x".to_owned(),
        sidebar: vec!["alpha".to_owned(), "beta".to_owned()],
        chats: CHATS.iter().map(|(_, title)| (*title).to_owned()).collect(),
        selected: 0,
        composer_value: None,
    }
}

/// A UI thread whose driver runs over a fake desktop acting as `fake` on the database at `db`.
fn fake_ui(db: std::path::PathBuf, fake: Fake, log: Log) -> UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&spec());
        let desktop = FakeDesktop::new(app.clone());
        if fake.locked {
            desktop.set_session(Some(SessionState {
                locked: true,
                on_console: true,
            }));
        }
        if fake.fail_keys {
            desktop.fail_keys("the keyboard is gone");
        }
        let pane = main_pane(&app);
        let area = pane.find_role("AXTextArea").expect("composer");
        let conn = Connection::open(&db).expect("open the test database for writing");

        let urls = Arc::clone(&log);
        desktop.on_open_url(move |url| urls.lock().unwrap().push(Seen::Url(url.to_owned())));

        let mut rows = 0;
        let mut opened = 0;
        desktop.on_key(move |key, modifiers| {
            log.lock().unwrap().push(Seen::Key(key, modifiers));
            let selected = CHATS
                .iter()
                .find(|(_, title)| {
                    pane.find_label(&format!("Close chat {title}"))
                        .is_some_and(|radio| radio.is_selected())
                })
                .map(|(id, _)| *id)
                .expect("a selected chat");
            match key {
                Key::Return => {
                    let text = area.value_text().unwrap_or_default();
                    area.set_value_text(Some(""));
                    if fake.lands {
                        rows += 1;
                        conn.execute(
                            "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                             VALUES (?1, ?2, 'user', ?3, 'turn-1')",
                            params![format!("m-{rows}"), selected, text],
                        )
                        .unwrap();
                    }
                }
                Key::Delete if modifiers.command && modifiers.shift && fake.stops => {
                    conn.execute(
                        "UPDATE sessions SET status = 'idle' WHERE id = ?1",
                        [selected],
                    )
                    .unwrap();
                }
                Key::T if modifiers.command => {
                    for _ in 0..fake.opens {
                        opened += 1;
                        conn.execute(
                            "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, \
                             created_at, updated_at) \
                             VALUES (?1, ?2, 'Untitled', 'idle', 0, ?3, ?3)",
                            params![
                                format!("s-new-{opened}"),
                                WORKSPACE,
                                format!("2026-09-02 10:00:0{opened}")
                            ],
                        )
                        .unwrap();
                    }
                }
                _ => {}
            }
        });
        Box::new(Driver::new(desktop))
    })
}

struct Rig {
    test: TestDb,
    ui: UiHandle,
    reads: Arc<Reads>,
    parked: Arc<ParkedQueue>,
    writes: Writes,
    log: Log,
}

impl Rig {
    fn new(fake: Fake, status: &str) -> Rig {
        Rig::timed(fake, status, timings())
    }

    fn timed(fake: Fake, status: &str, timings: WriteTimings) -> Rig {
        let test = TestDb::new();
        seed(&test.conn(), status);
        let log = Log::default();
        let ui = fake_ui(test.path().to_path_buf(), fake, Arc::clone(&log));
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let parked = parked_queue();
        let writes = Writes::new(
            Arc::clone(&reads),
            ui.clone(),
            Arc::new(|| true),
            timings,
            Arc::clone(&parked),
        );
        Rig {
            test,
            ui,
            reads,
            parked,
            writes,
            log,
        }
    }

    fn idle(fake: Fake) -> Rig {
        Rig::new(fake, "idle")
    }

    fn working(fake: Fake) -> Rig {
        Rig::new(fake, "working")
    }

    fn seen(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }

    fn keys(&self) -> Vec<(Key, Modifiers)> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Key(key, modifiers) => Some((key, modifiers)),
                Seen::Url(_) => None,
            })
            .collect()
    }

    fn returns(&self) -> usize {
        self.keys()
            .iter()
            .filter(|(key, _)| *key == Key::Return)
            .count()
    }

    fn status(&self, session_id: &str) -> String {
        self.test
            .conn()
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Parks `PROMPT` for `CHAT` of `workspace_id` with the chat's cursor as it is now.
    fn park(&self, workspace_id: &str) -> ParkedRow {
        let cursor = self.reads.delivery_cursor(CHAT).expect("the cursor");
        self.parked
            .park(workspace_id, CHAT, PROMPT, false, &cursor, 1)
            .expect("parked")
    }

    /// Writes the user row of `PROMPT` into `CHAT`, as Conductor does when it takes the prompt.
    fn land_prompt(&self) {
        self.test
            .conn()
            .execute(
                "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                 VALUES ('m-landed', ?1, 'user', ?2, 'turn-1')",
                params![CHAT, PROMPT],
            )
            .unwrap();
    }

    async fn send(&self, session_id: &str, client_id: Option<&str>) -> WriteAnswer {
        self.writes
            .send_prompt(SendRequest {
                session_id: session_id.to_owned(),
                text: PROMPT.to_owned(),
                workspace_id: None,
                client_id: client_id.map(str::to_owned),
                queue: false,
                client_timeout_ms: None,
                priority: Priority::Interactive,
                agent: None,
            })
            .await
    }

    async fn stop(&self, session_id: &str) -> WriteAnswer {
        self.writes
            .stop_turn(session_id.to_owned(), None, Priority::Interactive)
            .await
    }

    async fn new_chat(&self, workspace_id: &str) -> WriteAnswer {
        self.writes
            .new_chat(workspace_id.to_owned(), Priority::Interactive)
            .await
    }
}

fn command_shift() -> Modifiers {
    Modifiers {
        command: true,
        shift: true,
        ..Modifiers::default()
    }
}

// ---- send ----

#[tokio::test(flavor = "multi_thread")]
async fn send_delivered_answers_the_message_receipt() {
    let rig = Rig::idle(Fake::default());
    let answer = rig.send(CHAT, None).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.retry_after_secs, None);
    assert_eq!(
        answer.body,
        json!({
            "ok": true,
            "strategy": "accessibility",
            "attempts": 1,
            "receipt": { "kind": "message", "id": "m-1", "rowid": 1, "turnId": "turn-1" },
        })
    );
    assert_eq!(answer.body["receipt"]["kind"], "message");
    assert_eq!(rig.returns(), 1);
    assert_eq!(
        rig.seen().first(),
        Some(&Seen::Url(
            "conductor://workspace?id=ws-1&session=s-2".to_owned()
        ))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn send_repeated_client_id_types_once_and_answers_the_same() {
    let rig = Rig::idle(Fake::default());
    let first = rig.send(CHAT, Some("bubble-1")).await;
    let second = rig.send(CHAT, Some("bubble-1")).await;
    assert_eq!(first.status, 200, "{first:?}");
    assert_eq!(second, first);
    assert_eq!(rig.returns(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_receipt_that_lands_while_the_job_waits_is_not_typed_again() {
    let rig = Rig::idle(Fake::default());
    let (started_tx, started_rx) = mpsc::channel();
    let (release, gate) = mpsc::channel::<()>();
    let held = rig
        .ui
        .run(Priority::Interactive, move |_: &mut dyn UiDriver| {
            started_tx.send(()).unwrap();
            let _ = gate.recv();
        });
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .expect("the holding job started");

    let send = rig.send(CHAT, None);
    let land_and_release = async {
        // The send's job waits in the queue behind the held one.
        let waited = Instant::now();
        while rig.ui.waiting() == 0 {
            assert!(waited.elapsed() < Duration::from_secs(5), "no job queued");
            tokio::time::sleep(ms(5)).await;
        }
        rig.land_prompt();
        release.send(()).unwrap();
    };
    let (answer, ()) = tokio::join!(send, land_and_release);
    held.await.unwrap();

    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["receipt"]["kind"], "message");
    assert_eq!(rig.returns(), 0, "{:?}", rig.keys());
}

#[tokio::test(flavor = "multi_thread")]
async fn send_whose_row_never_appears_is_not_confirmed() {
    let rig = Rig::idle(Fake {
        lands: false,
        ..Fake::default()
    });
    let answer = rig.send(CHAT, None).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(answer.body["ok"], false);
    assert_eq!(answer.body["strategy"], "accessibility");
    let attempts = answer.body["attempts"].as_u64().expect("attempts");
    assert!(attempts >= 1, "{answer:?}");
    let error = answer.body["error"].as_str().expect("error");
    assert!(
        error.starts_with("Could not confirm the sent message in Conductor"),
        "{error}"
    );
    assert!(answer.body.get("receipt").is_none());
    assert_eq!(rig.returns() as u64, attempts);
}

#[tokio::test(flavor = "multi_thread")]
async fn send_for_an_unknown_chat_is_404() {
    let rig = Rig::idle(Fake::default());
    let answer = rig.send("nope", None).await;
    assert_eq!(answer.status, 404);
    assert_eq!(
        answer.body,
        json!({ "error": "workspace for session not found" })
    );
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn send_to_a_chat_that_is_not_a_tab_tries_nothing() {
    let rig = Rig::idle(Fake::default());
    let answer = rig.send(HIDDEN, None).await;
    assert_eq!(answer.status, 502);
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "accessibility", "attempts": 0, "error": NOT_A_TAB })
    );
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn send_on_a_locked_mac_says_so() {
    let rig = Rig::idle(Fake {
        locked: true,
        ..Fake::default()
    });
    let answer = rig.send(CHAT, None).await;
    assert_eq!(answer.status, 202, "{answer:?}");
    assert_eq!(answer.body["ok"], false);
    assert_eq!(answer.body["parked"], true);
    assert_eq!(answer.body["queued"]["status"], "waiting");
    assert_eq!(
        answer.body["queued"]["reason"],
        "Sends when the Mac is unlocked"
    );
    assert_eq!(answer.body["error"], PARKED_ERROR);
    let error = answer.body["error"].as_str().expect("error");
    assert!(error.starts_with("The Mac is locked"), "{error}");
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resent_locked_prompt_keeps_one_parked_row() {
    let rig = Rig::idle(Fake {
        locked: true,
        ..Fake::default()
    });
    let first = rig.send(CHAT, None).await;
    let second = rig.send(CHAT, None).await;
    assert_eq!(first.status, 202, "{first:?}");
    assert_eq!(second.status, 202, "{second:?}");
    let rows = rig.parked.list();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].session_id, CHAT);
    assert_eq!(rows[0].workspace_id, WORKSPACE);
    assert_eq!(rows[0].text, PROMPT);
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_send_that_lands_removes_the_parked_row_of_its_text() {
    let rig = Rig::idle(Fake::default());
    rig.park(WORKSPACE);
    let cursor = rig.reads.delivery_cursor(CHAT).unwrap();
    rig.parked
        .park(WORKSPACE, CHAT, "another prompt", false, &cursor, 2)
        .unwrap();
    let answer = rig.send(CHAT, None).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let left: Vec<String> = rig.parked.list().into_iter().map(|row| row.text).collect();
    assert_eq!(left, ["another prompt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_that_lands_removes_the_first_prompt_of_its_workspace() {
    let test = TestDb::new();
    seed(&test.conn(), "idle");
    let ui = fake_ui(
        test.path().to_path_buf(),
        Fake::default(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    let store = Arc::new(Store::open_in_memory().expect("an in-memory store"));
    let writes =
        Writes::new(reads, ui, Arc::new(|| true), timings(), parked_queue()).configure(WriteDeps {
            state_dir: test.dir().join("state"),
            store: Arc::clone(&store),
            commands: Arc::new(FakeCommands::default()),
            locked: Arc::new(|| Some(false)),
        });
    for workspace_id in [WORKSPACE, "ws-other"] {
        store
            .upsert_first_prompt(&NewFirstPrompt {
                workspace_id: workspace_id.to_owned(),
                text: "the first prompt".to_owned(),
                send_immediately: false,
                attachment_ids: Vec::new(),
                created_at_ms: 1,
            })
            .expect("a first prompt");
    }

    let answer = writes
        .send_prompt(SendRequest {
            session_id: CHAT.to_owned(),
            text: PROMPT.to_owned(),
            workspace_id: None,
            client_id: None,
            queue: false,
            client_timeout_ms: None,
            priority: Priority::Interactive,
            agent: None,
        })
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(store.first_prompt(WORKSPACE).unwrap(), None);
    assert!(
        store.first_prompt("ws-other").unwrap().is_some(),
        "another workspace's entry stays"
    );
}

// ---- parked prompts ----

#[tokio::test(flavor = "multi_thread")]
async fn deliver_parked_uses_the_persisted_cursor_and_does_not_type_a_landed_prompt() {
    let rig = Rig::idle(Fake::default());
    // The cursor is taken before the prompt lands, as a locked send takes it.
    let row = rig.park(WORKSPACE);
    rig.land_prompt();
    let outcome = rig.writes.deliver_parked(row).await;
    assert_eq!(outcome, ParkedOutcome::Delivered);
    assert!(rig.keys().is_empty(), "{:?}", rig.seen());
}

#[tokio::test(flavor = "multi_thread")]
async fn deliver_parked_types_a_waiting_prompt() {
    let rig = Rig::idle(Fake::default());
    let row = rig.park(WORKSPACE);
    let outcome = rig.writes.deliver_parked(row).await;
    assert_eq!(outcome, ParkedOutcome::Delivered);
    assert_eq!(rig.returns(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn deliver_parked_for_a_gone_workspace_fails_with_its_text() {
    let rig = Rig::idle(Fake::default());
    let row = rig.park(WORKSPACE);
    rig.test
        .conn()
        .execute(
            "UPDATE workspaces SET state = 'archived' WHERE id = ?1",
            [WORKSPACE],
        )
        .unwrap();
    let outcome = rig.writes.deliver_parked(row).await;
    assert_eq!(
        outcome,
        ParkedOutcome::Failed("the workspace is gone".to_owned())
    );
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn parked_prompts_lists_the_parked_row() {
    let rig = Rig::idle(Fake {
        locked: true,
        ..Fake::default()
    });
    assert!(rig.writes.parked_prompts().is_empty());
    let answer = rig.send(CHAT, None).await;
    assert_eq!(answer.status, 202, "{answer:?}");
    let listed = rig.writes.parked_prompts();
    assert_eq!(listed, [answer.body["queued"].clone()]);
    assert_eq!(listed[0]["workspaceId"], WORKSPACE);
    assert_eq!(listed[0]["sessionId"], CHAT);
    assert_eq!(listed[0]["text"], PROMPT);
    assert_eq!(listed[0]["status"], "waiting");
    assert_eq!(listed[0]["attempts"], 0);
    assert_eq!(listed[0]["reason"], PARKED_REASON);
    assert!(listed[0].get("queue").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn dismiss_parked_answers_200_then_404() {
    let rig = Rig::idle(Fake::default());
    rig.park(WORKSPACE);
    let first = rig.writes.dismiss_parked(CHAT.to_owned()).await;
    assert_eq!(first.status, 200, "{first:?}");
    assert_eq!(first.body, json!({ "ok": true }));
    assert!(rig.parked.list().is_empty());
    let second = rig.writes.dismiss_parked(CHAT.to_owned()).await;
    assert_eq!(second.status, 404, "{second:?}");
    assert_eq!(second.body, json!({ "error": "no parked prompt" }));
}

// ---- stop ----

#[tokio::test(flavor = "multi_thread")]
async fn stop_on_an_idle_chat_presses_nothing() {
    let rig = Rig::idle(Fake::default());
    let answer = rig.stop(CHAT).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["alreadyIdle"], true);
    assert_eq!(answer.body["session"]["id"], CHAT);
    assert_eq!(answer.body["session"]["status"], "idle");
    assert!(answer.body.get("strategy").is_none());
    assert!(rig.keys().is_empty(), "{:?}", rig.seen());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_on_a_working_chat_waits_for_idle() {
    let rig = Rig::working(Fake::default());
    let answer = rig.stop(CHAT).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["strategy"], "accessibility");
    assert_eq!(answer.body["session"]["id"], CHAT);
    assert_eq!(answer.body["session"]["status"], "idle");
    assert!(answer.body.get("alreadyIdle").is_none());
    assert!(rig.keys().contains(&(Key::Delete, command_shift())));
    assert_eq!(rig.status(CHAT), "idle");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_that_never_takes_is_still_working() {
    let rig = Rig::working(Fake {
        stops: false,
        ..Fake::default()
    });
    let answer = rig.stop(CHAT).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({
            "ok": false,
            "strategy": "accessibility",
            "error": "Conductor took the stop but the agent is still working. Try again, or stop it on your Mac.",
        })
    );
    assert!(rig.keys().contains(&(Key::Delete, command_shift())));
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_for_an_unknown_chat_is_404() {
    let rig = Rig::working(Fake::default());
    let answer = rig.stop("nope").await;
    assert_eq!(answer.status, 404);
    assert_eq!(
        answer.body,
        json!({ "error": "workspace for session not found" })
    );
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_for_a_chat_that_is_not_a_tab_is_409() {
    let rig = Rig::working(Fake::default());
    let answer = rig.stop(HIDDEN).await;
    assert_eq!(answer.status, 409);
    assert_eq!(answer.body, json!({ "error": NOT_A_TAB }));
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_on_a_locked_mac_is_502() {
    let rig = Rig::working(Fake {
        locked: true,
        ..Fake::default()
    });
    let answer = rig.stop(CHAT).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(answer.body["ok"], false);
    assert_eq!(answer.body["strategy"], "accessibility");
    let error = answer.body["error"].as_str().expect("error");
    assert!(error.starts_with("The Mac is locked"), "{error}");
    assert!(rig.seen().is_empty());
}

// ---- new chat ----

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_answers_the_new_id() {
    let rig = Rig::idle(Fake::default());
    let answer = rig.new_chat(WORKSPACE).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body, json!({ "ok": true, "sessionId": "s-new-1" }));
    let command = Modifiers {
        command: true,
        ..Modifiers::default()
    };
    assert_eq!(rig.keys(), [(Key::L, command), (Key::T, command)]);
    assert_eq!(
        rig.seen().first(),
        Some(&Seen::Url("conductor://workspace?id=ws-1".to_owned()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_refuses_to_guess_between_two() {
    let rig = Rig::idle(Fake {
        opens: 2,
        ..Fake::default()
    });
    let answer = rig.new_chat(WORKSPACE).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({
            "ok": false,
            "strategy": "accessibility",
            "error": "more than one new chat appeared; refusing to guess which one this request opened",
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_that_never_appears_is_not_confirmed() {
    let rig = Rig::idle(Fake {
        opens: 0,
        ..Fake::default()
    });
    let answer = rig.new_chat(WORKSPACE).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({
            "ok": false,
            "strategy": "accessibility",
            "error": "Conductor did not confirm a new chat. Check the workspace before trying again.",
        })
    );
    assert_eq!(rig.keys().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_on_a_locked_mac_answers_the_command_error() {
    let rig = Rig::idle(Fake {
        locked: true,
        ..Fake::default()
    });
    let answer = rig.new_chat(WORKSPACE).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(answer.body["ok"], false);
    assert_eq!(answer.body["strategy"], "accessibility");
    let error = answer.body["error"].as_str().expect("error");
    assert!(error.starts_with("The Mac is locked"), "{error}");
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_that_pressed_nothing_does_not_hold_the_ui_thread() {
    let rig = Rig::timed(
        Fake {
            locked: true,
            ..Fake::default()
        },
        "idle",
        WriteTimings {
            chat_poll: Duration::from_secs(2),
            chat_checks: 13,
            ..timings()
        },
    );
    let started = Instant::now();
    let answer = rig.new_chat(WORKSPACE).await;
    let elapsed = started.elapsed();
    assert_eq!(answer.status, 502, "{answer:?}");
    let error = answer.body["error"].as_str().expect("error");
    assert!(error.starts_with("The Mac is locked"), "{error}");
    assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_whose_keys_failed_still_waits_for_the_chat() {
    let poll = ms(200);
    let rig = Rig::timed(
        Fake {
            fail_keys: true,
            ..Fake::default()
        },
        "idle",
        WriteTimings {
            chat_poll: poll,
            chat_checks: 2,
            ..timings()
        },
    );
    let started = Instant::now();
    let answer = rig.new_chat(WORKSPACE).await;
    let elapsed = started.elapsed();
    assert_eq!(answer.status, 502, "{answer:?}");
    let error = answer.body["error"].as_str().expect("error");
    assert!(error.starts_with("couldn't press a key"), "{error}");
    assert!(elapsed >= poll, "took {elapsed:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_in_an_unknown_workspace_is_404() {
    let rig = Rig::idle(Fake::default());
    let answer = rig.new_chat("nope").await;
    assert_eq!(answer.status, 404);
    assert_eq!(answer.body, json!({ "error": "workspace not found" }));
    assert!(rig.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_with_a_full_ui_queue_is_busy() {
    let rig = Rig::idle(Fake::default());
    let (started_tx, started_rx) = mpsc::channel();
    let (release, gate) = mpsc::channel::<()>();
    let held = rig
        .ui
        .run(Priority::Interactive, move |_: &mut dyn UiDriver| {
            started_tx.send(()).unwrap();
            let _ = gate.recv();
        });
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .expect("the holding job started");
    let queued: Vec<_> = (0..4)
        .map(|_| rig.ui.run(Priority::Background, |_: &mut dyn UiDriver| ()))
        .collect();

    let answer = rig.new_chat(WORKSPACE).await;
    release.send(()).unwrap();
    held.await.unwrap();
    for job in queued {
        job.await.unwrap();
    }

    assert_eq!(answer.status, 503, "{answer:?}");
    assert_eq!(answer.retry_after_secs, Some(15));
    assert_eq!(
        answer.body,
        json!({
            "error": "Conductor's UI is busy \u{2014} 4 operation(s) already queued. Try again shortly.",
            "busy": true,
            "queue": { "waiting": 4, "busy": true },
        })
    );
    assert!(rig.keys().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_chat_when_the_ui_thread_failed_is_502() {
    let test = TestDb::new();
    seed(&test.conn(), "idle");
    let ui = UiActor::spawn(|| -> Box<dyn UiDriver> { panic!("no desktop in this test") });
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    let writes = Writes::new(reads, ui, Arc::new(|| true), timings(), parked_queue());
    let answer = writes
        .new_chat(WORKSPACE.to_owned(), Priority::Interactive)
        .await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({
            "ok": false,
            "strategy": "accessibility",
            "error": "the relay's UI thread failed - try again",
        })
    );
}

// ---- available ----

#[tokio::test(flavor = "multi_thread")]
async fn available_follows_the_trust_check() {
    let test = TestDb::new();
    let ui = UiActor::spawn(|| -> Box<dyn UiDriver> { panic!("never used") });
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    let untrusted = Writes::new(
        Arc::clone(&reads),
        ui.clone(),
        Arc::new(|| false),
        timings(),
        parked_queue(),
    );
    let trusted = Writes::new(reads, ui, Arc::new(|| true), timings(), parked_queue());
    assert!(!untrusted.available());
    assert!(trusted.available());
}
