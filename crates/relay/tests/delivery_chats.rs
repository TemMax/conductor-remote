//! Restore, history joins and split-to-chat over the fake desktop, a synthetic database and a
//! temporary worktree. The fake desktop lives on the UI thread and writes into the database what
//! Conductor would; nothing here reaches the Mac.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{SplitDestination, SplitRequest, WriteAnswer, WriteService};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::testing::FakeCommands;
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::fake::{conductor_app, FakeDesktop, WindowSpec};
use conductor_remote::ui::keys::Key;
use conductor_remote::ui::screen::SessionState;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;

const WORKSPACE: &str = "ws-1";
/// A live workspace whose worktree cannot be resolved.
const NO_TREE: &str = "ws-2";
/// An archived workspace.
const ARCHIVED: &str = "ws-old";
/// The open chats of `WORKSPACE`, in tab order: (id, title).
const CHATS: [(&str, &str); 2] = [("s-1", "One"), ("s-2", "Two")];
/// The chat with a transcript.
const CHAT: &str = "s-2";
/// A chat with no rows.
const EMPTY: &str = "s-1";
/// A closed chat of `WORKSPACE`.
const HIDDEN: &str = "s-hidden";
/// A chat of `NO_TREE`.
const ELSEWHERE: &str = "s-elsewhere";
/// A chat of `ARCHIVED`.
const ARCHIVED_CHAT: &str = "s-archived";
/// `<root>/<repo>/<directory>` is the worktree of `WORKSPACE`.
const REPO: &str = "relay";
const DIRECTORY: &str = "beta-dir";

const NOT_RESTORED: &str =
    "Conductor has not restored this tab yet. Try again, or update Conductor on your Mac.";
const LOCKED: &str = "The Mac is locked - the lock screen hides Conductor from the relay, so \
                      nothing can be sent or pressed. Unlock the Mac and try again.";
const NO_NEW_CHAT: &str =
    "Conductor did not confirm a new chat. Check the workspace before trying again.";

/// How the fake Conductor reacts.
#[derive(Clone, Copy)]
struct Fake {
    /// Opening a chat's deep link un-hides that chat.
    unhides: bool,
    /// How many visible chats Cmd+T opens.
    opens: usize,
    /// The Mac is locked.
    locked: bool,
}

impl Default for Fake {
    fn default() -> Fake {
        Fake {
            unhides: true,
            opens: 1,
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

fn insert_chat(conn: &Connection, id: &str, workspace: &str, title: &str, hidden: i64, at: &str) {
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
         updated_at) VALUES (?1, ?2, ?3, 'idle', ?4, ?5, ?5)",
        params![id, workspace, title, hidden, at],
    )
    .unwrap();
}

fn insert_row(conn: &Connection, id: &str, content: &str) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, content, created_at, sent_at) \
         VALUES (?1, ?2, ?3, '2026-09-01T10:05:00.000Z', '2026-09-01T10:05:00.000Z')",
        params![id, CHAT, content],
    )
    .unwrap();
}

/// Three workspaces: `WORKSPACE` (live, repo "relay", branch `user/feature-x`, a worktree), with
/// the open chats of `CHATS` and one closed chat; `NO_TREE` (live, no worktree) with one chat;
/// `ARCHIVED` with one chat. `CHAT` holds six rows: rowid 1 a prompt; 2 thinking, prose and a
/// Read call; 3 its successful result; 4 prose; 5 a prompt; 6 prose.
fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', ?1)", [REPO])
        .unwrap();
    let workspaces = [
        (WORKSPACE, "user/feature-x", "beta", DIRECTORY, "ready"),
        (NO_TREE, "user/other", "gamma", "gamma-dir", "ready"),
        (ARCHIVED, "user/old", "delta", "delta-dir", "archived"),
    ];
    for (id, branch, name, directory, state) in workspaces {
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
             directory_name, state) VALUES (?1, ?1, 'r-1', ?2, ?3, ?4, ?5)",
            params![id, branch, name, directory, state],
        )
        .unwrap();
    }
    insert_chat(
        conn,
        CHATS[0].0,
        WORKSPACE,
        CHATS[0].1,
        0,
        "2026-09-01 10:00:00",
    );
    insert_chat(
        conn,
        CHATS[1].0,
        WORKSPACE,
        CHATS[1].1,
        0,
        "2026-09-01 10:01:00",
    );
    insert_chat(conn, HIDDEN, WORKSPACE, "Closed", 1, "2026-09-01 10:02:00");
    insert_chat(
        conn,
        ELSEWHERE,
        NO_TREE,
        "Elsewhere",
        0,
        "2026-09-01 10:03:00",
    );
    insert_chat(
        conn,
        ARCHIVED_CHAT,
        ARCHIVED,
        "Old",
        1,
        "2026-09-01 10:04:00",
    );

    insert_row(conn, "m-1", "Why does the build fail?");
    insert_row(
        conn,
        "m-2",
        &json!({ "type": "assistant", "message": { "content": [
            { "type": "thinking", "thinking": "Check the log first." },
            { "type": "text", "text": "I'll read the log." },
            { "type": "tool_use", "id": "tool-1", "name": "Read",
              "input": { "file_path": "build.log" } }
        ] } })
        .to_string(),
    );
    insert_row(
        conn,
        "m-3",
        &json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "tool-1", "content": "ld: missing -lz" }
        ] } })
        .to_string(),
    );
    insert_row(
        conn,
        "m-4",
        &json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "The linker is missing a flag." }
        ] } })
        .to_string(),
    );
    insert_row(conn, "m-5", "Add it then.");
    insert_row(
        conn,
        "m-6",
        &json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "Done; the build passes." }
        ] } })
        .to_string(),
    );
}

/// The window showing `WORKSPACE`, its first chat selected.
fn spec() -> WindowSpec {
    WindowSpec {
        repo: REPO.to_owned(),
        branch: "user/feature-x".to_owned(),
        sidebar: vec!["alpha".to_owned(), "beta".to_owned()],
        chats: CHATS.iter().map(|(_, title)| (*title).to_owned()).collect(),
        selected: 0,
        composer_value: None,
    }
}

type Urls = Arc<Mutex<Vec<String>>>;

/// A UI thread whose driver runs over a fake desktop acting as `fake` on the database at `db`.
fn fake_ui(db: PathBuf, fake: Fake, urls: Urls) -> UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&spec());
        let desktop = FakeDesktop::new(app);
        if fake.locked {
            desktop.set_session(Some(SessionState {
                locked: true,
                on_console: true,
            }));
        }
        let links = Connection::open(&db).expect("open the test database for writing");
        desktop.on_open_url(move |url| {
            urls.lock().unwrap().push(url.to_owned());
            let Some((_, session)) = url.split_once("&session=") else {
                return;
            };
            if fake.unhides {
                links
                    .execute("UPDATE sessions SET is_hidden = 0 WHERE id = ?1", [session])
                    .unwrap();
            }
        });
        let chats = Connection::open(&db).expect("open the test database for writing");
        let mut opened = 0;
        desktop.on_key(move |key, modifiers| {
            if key == Key::T && modifiers.command {
                for _ in 0..fake.opens {
                    opened += 1;
                    chats
                        .execute(
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
        });
        Box::new(Driver::new(desktop))
    })
}

struct Rig {
    test: TestDb,
    writes: Writes,
    urls: Urls,
}

impl Rig {
    fn new(fake: Fake) -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        std::fs::create_dir_all(test.root().join(REPO).join(DIRECTORY).join(".git"))
            .expect("the worktree");
        let urls = Urls::default();
        let ui = fake_ui(test.path().to_path_buf(), fake, Arc::clone(&urls));
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
                commands: Arc::new(FakeCommands::default()),
                locked: Arc::new(|| Some(false)),
            });
        Rig { test, writes, urls }
    }

    fn urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }

    fn worktree(&self) -> PathBuf {
        self.test.root().join(REPO).join(DIRECTORY)
    }

    fn hidden(&self, session_id: &str) -> i64 {
        self.test
            .conn()
            .query_row(
                "SELECT is_hidden FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    async fn restore(&self, session_id: &str, workspace_id: Option<&str>) -> WriteAnswer {
        self.writes
            .restore_chat(
                session_id.to_owned(),
                workspace_id.map(str::to_owned),
                Priority::Interactive,
            )
            .await
    }

    async fn join(&self, session_id: &str, workspace_id: &str, previous: &str) -> WriteAnswer {
        self.writes
            .join_history(
                session_id.to_owned(),
                workspace_id.to_owned(),
                previous.to_owned(),
            )
            .await
    }

    async fn split(&self, session_id: &str, request: SplitRequest) -> WriteAnswer {
        self.writes
            .split_chat(session_id.to_owned(), request, Priority::Interactive)
            .await
    }

    /// The transcript a split wrote, read back from the worktree.
    fn written(&self, answer: &WriteAnswer) -> String {
        let path = answer.body["attachment"]["path"].as_str().expect("a path");
        std::fs::read_to_string(self.worktree().join(path)).expect("the transcript file")
    }
}

/// The phone's defaults: thinking in, tools out, no cut.
fn request() -> SplitRequest {
    SplitRequest {
        destination: SplitDestination::Chat,
        workspace_id: None,
        prompt: None,
        include_thinking: true,
        include_tools: false,
        through_rowid: None,
        only_rowid: None,
    }
}

fn error(answer: &WriteAnswer) -> Value {
    answer.body["error"].clone()
}

// ---- restore ----

#[tokio::test(flavor = "multi_thread")]
async fn restore_of_an_unknown_chat_is_not_found() {
    let rig = Rig::new(Fake::default());
    let answer = rig.restore("s-nowhere", None).await;
    assert_eq!(answer.status, 404, "{answer:?}");
    assert_eq!(answer.body, json!({ "error": "chat not found" }));
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_in_another_workspace_is_a_conflict() {
    let rig = Rig::new(Fake::default());
    let answer = rig.restore(HIDDEN, Some(NO_TREE)).await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "chat is not in that workspace" })
    );
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_in_an_archived_workspace_asks_for_the_workspace_first() {
    let rig = Rig::new(Fake::default());
    let answer = rig.restore(ARCHIVED_CHAT, Some(ARCHIVED)).await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "Restore this workspace in Conductor before restoring its tabs." })
    );
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_of_an_open_chat_answers_without_the_ui() {
    let rig = Rig::new(Fake {
        locked: true,
        ..Fake::default()
    });
    let answer = rig.restore(CHAT, Some(WORKSPACE)).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["strategy"], "deep-link");
    assert_eq!(answer.body["alreadyOpen"], true);
    assert_eq!(answer.body["session"]["id"], CHAT);
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_opens_the_chat_link_and_answers_the_session() {
    let rig = Rig::new(Fake::default());
    let answer = rig.restore(HIDDEN, None).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["ok"], true);
    assert_eq!(answer.body["strategy"], "deep-link");
    assert_eq!(answer.body.get("alreadyOpen"), None);
    assert_eq!(answer.body["session"]["id"], HIDDEN);
    assert_eq!(answer.body["session"]["title"], "Closed");
    assert_eq!(
        rig.urls(),
        ["conductor://workspace?id=ws-1&session=s-hidden"]
    );
    assert_eq!(rig.hidden(HIDDEN), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_that_conductor_ignores_is_a_bad_gateway() {
    let rig = Rig::new(Fake {
        unhides: false,
        ..Fake::default()
    });
    let answer = rig.restore(HIDDEN, Some(WORKSPACE)).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "deep-link", "error": NOT_RESTORED })
    );
    assert_eq!(
        rig.urls(),
        ["conductor://workspace?id=ws-1&session=s-hidden"]
    );
    assert_eq!(rig.hidden(HIDDEN), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_on_a_locked_mac_opens_nothing() {
    let rig = Rig::new(Fake {
        locked: true,
        ..Fake::default()
    });
    let answer = rig.restore(HIDDEN, None).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "ok": false, "strategy": "deep-link", "error": LOCKED })
    );
    assert!(rig.urls().is_empty());
    assert_eq!(rig.hidden(HIDDEN), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_restores_of_one_chat_open_its_link_once() {
    let rig = Rig::new(Fake::default());
    let (first, second) = tokio::join!(rig.restore(HIDDEN, None), rig.restore(HIDDEN, None));
    assert_eq!(first.status, 200, "{first:?}");
    assert_eq!(second.status, 200, "{second:?}");
    assert_eq!(first.body["session"]["id"], HIDDEN);
    assert_eq!(second.body["session"]["id"], HIDDEN);
    assert_eq!(rig.urls().len(), 1, "{:?}", rig.urls());
}

// ---- join ----

#[tokio::test(flavor = "multi_thread")]
async fn join_links_two_chats_without_the_ui() {
    let rig = Rig::new(Fake::default());
    assert_eq!(rig.writes.chat_history(WORKSPACE), json!({}));
    let answer = rig.join(CHAT, WORKSPACE, EMPTY).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body, json!({ "ok": true }));
    // A repeat of the same link is answered the same.
    let again = rig.join(CHAT, WORKSPACE, EMPTY).await;
    assert_eq!(again, answer);
    assert_eq!(
        rig.writes.chat_history(WORKSPACE),
        json!({
            "s-2": {
                "previousSessionId": "s-1",
                "title": "One",
                "createdAt": "2026-09-01 10:00:00",
            }
        })
    );
    assert_eq!(rig.writes.chat_history(NO_TREE), json!({}));
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn join_accepts_a_closed_previous_chat() {
    let rig = Rig::new(Fake::default());
    let answer = rig.join(EMPTY, WORKSPACE, HIDDEN).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        rig.writes.chat_history(WORKSPACE)["s-1"],
        json!({
            "previousSessionId": "s-hidden",
            "title": "Closed",
            "createdAt": "2026-09-01 10:02:00",
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn join_across_workspaces_is_not_found() {
    let rig = Rig::new(Fake::default());
    for (session, workspace, previous) in [
        (CHAT, WORKSPACE, ELSEWHERE),
        (ELSEWHERE, WORKSPACE, CHAT),
        (CHAT, NO_TREE, EMPTY),
        // A previous chat that does not exist has no owner either.
        (CHAT, WORKSPACE, "s-nowhere"),
    ] {
        let answer = rig.join(session, workspace, previous).await;
        assert_eq!(
            answer.status, 404,
            "{session} {workspace} {previous}: {answer:?}"
        );
        assert_eq!(
            answer.body,
            json!({ "error": "chats not found in that workspace" })
        );
    }
    assert_eq!(rig.writes.chat_history(WORKSPACE), json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn join_refusals_are_conflicts_with_the_store_text() {
    let rig = Rig::new(Fake::default());
    assert_eq!(rig.join(CHAT, WORKSPACE, EMPTY).await.status, 200);

    let relinked = rig.join(CHAT, WORKSPACE, HIDDEN).await;
    assert_eq!(relinked.status, 409, "{relinked:?}");
    assert_eq!(
        relinked.body,
        json!({ "error": "This chat already belongs to a conversation" })
    );

    let branched = rig.join(HIDDEN, WORKSPACE, EMPTY).await;
    assert_eq!(branched.status, 409, "{branched:?}");
    assert_eq!(
        branched.body,
        json!({
            "error": "This chat already continues in another tab. Refresh to open the latest \
                      conversation."
        })
    );

    let cycle = rig.join(EMPTY, WORKSPACE, CHAT).await;
    assert_eq!(cycle.status, 409, "{cycle:?}");
    assert_eq!(
        cycle.body,
        json!({ "error": "Chat history cannot contain a cycle" })
    );
    assert!(rig.urls().is_empty());
}

// ---- split ----

const HEAD_THROUGH: &str = "# Transcript of Two\n\
\n\
relay · user/feature-x\n\
Copied from the Conductor chat `s-2` by Conductor Remote. thinking included, tool calls omitted.\n\
The copy stops partway through: 3 later entries are not in it.\n\
\n";

#[tokio::test(flavor = "multi_thread")]
async fn split_through_a_middle_row_cuts_after_its_entries() {
    let rig = Rig::new(Fake::default());
    let answer = rig
        .split(
            CHAT,
            SplitRequest {
                through_rowid: Some(2),
                prompt: Some("  take it from here \n".to_owned()),
                ..request()
            },
        )
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let transcript = rig.written(&answer);
    assert_eq!(
        transcript,
        format!(
            "{HEAD_THROUGH}## User\n\nWhy does the build fail?\n\n## Thinking\n\n\
             Check the log first.\n\n## Assistant\n\nI'll read the log.\n\n\
             [1 tool call elided]\n"
        )
    );

    let attachment = &answer.body["attachment"];
    let path = attachment["path"].as_str().unwrap().to_owned();
    assert!(
        path.starts_with(".context/attachments/") && path.ends_with("/Transcript of Two.md"),
        "{path}"
    );
    let token = format!(
        "@⟦Transcript of Two.md⟧({})",
        path.replace('/', "%2F").replace(' ', "%20")
    );
    assert_eq!(
        answer.body,
        json!({
            "ok": true,
            "destination": "chat",
            "sessionId": "s-new-1",
            "workspaceId": "ws-1",
            "text": format!("Forked from {token}\n\ntake it from here"),
            "attachment": {
                "name": "Transcript of Two.md",
                "path": path,
                "bytes": transcript.len(),
                "kept": 3,
                "elided": { "thinking": 0, "tools": 1, "earlier": 0, "later": 3 },
            },
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn split_of_one_row_keeps_only_its_entries() {
    let rig = Rig::new(Fake::default());
    let answer = rig
        .split(
            CHAT,
            SplitRequest {
                only_rowid: Some(2),
                include_thinking: false,
                ..request()
            },
        )
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        rig.written(&answer),
        "# Transcript of Two\n\
         \n\
         relay · user/feature-x\n\
         Copied from the Conductor chat `s-2` by Conductor Remote. thinking omitted, tool calls \
         omitted.\n\
         The copy contains only the selected source message; all earlier and later messages are \
         omitted.\n\
         \n\
         ## Assistant\n\n[1 thinking block elided]\n\nI'll read the log.\n\n[1 tool call elided]\n"
    );
    assert_eq!(answer.body["attachment"]["kept"], 1);
    assert_eq!(
        answer.body["attachment"]["elided"],
        json!({ "thinking": 1, "tools": 1, "earlier": 1, "later": 3 })
    );
    assert_eq!(
        answer.body["text"],
        format!(
            "Forked from @⟦Transcript of Two.md⟧({})\n\n",
            answer.body["attachment"]["path"]
                .as_str()
                .unwrap()
                .replace('/', "%2F")
                .replace(' ', "%20")
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn split_of_the_whole_chat_with_tools_has_no_stop_line() {
    let rig = Rig::new(Fake::default());
    let answer = rig
        .split(
            CHAT,
            SplitRequest {
                include_tools: true,
                ..request()
            },
        )
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        rig.written(&answer),
        "# Transcript of Two\n\
         \n\
         relay · user/feature-x\n\
         Copied from the Conductor chat `s-2` by Conductor Remote. thinking included, tool calls \
         included.\n\
         \n\
         ## User\n\nWhy does the build fail?\n\n## Thinking\n\nCheck the log first.\n\n\
         ## Assistant\n\nI'll read the log.\n\n## Tools\n\n- [Read] Read — `build.log`\n\n\
         ## Assistant\n\nThe linker is missing a flag.\n\n## User\n\nAdd it then.\n\n\
         ## Assistant\n\nDone; the build passes.\n"
    );
    assert_eq!(answer.body["attachment"]["kept"], 7);
    assert_eq!(
        answer.body["attachment"]["elided"],
        json!({ "thinking": 0, "tools": 0, "earlier": 0, "later": 0 })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn split_through_the_result_row_counts_the_result_nowhere() {
    let rig = Rig::new(Fake::default());
    let answer = rig
        .split(
            CHAT,
            SplitRequest {
                through_rowid: Some(3),
                ..request()
            },
        )
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.body["attachment"]["kept"], 3);
    assert_eq!(
        answer.body["attachment"]["elided"],
        json!({ "thinking": 0, "tools": 1, "earlier": 0, "later": 3 })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn split_refuses_bad_rowids() {
    let rig = Rig::new(Fake::default());
    let cases = [
        (Some(0), None, "throughRowid must be a positive integer"),
        (Some(-3), None, "throughRowid must be a positive integer"),
        (
            Some(9_007_199_254_740_992),
            None,
            "throughRowid must be a positive integer",
        ),
        (None, Some(0), "onlyRowid must be a positive integer"),
        (
            Some(1),
            Some(2),
            "throughRowid and onlyRowid cannot be combined",
        ),
    ];
    for (through_rowid, only_rowid, text) in cases {
        let answer = rig
            .split(
                CHAT,
                SplitRequest {
                    through_rowid,
                    only_rowid,
                    ..request()
                },
            )
            .await;
        assert_eq!(
            answer.status, 400,
            "{through_rowid:?} {only_rowid:?}: {answer:?}"
        );
        assert_eq!(answer.body, json!({ "error": text }));
    }
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn split_at_a_row_of_no_entry_of_this_chat_is_a_conflict() {
    let rig = Rig::new(Fake::default());
    for (through_rowid, only_rowid) in [(Some(99), None), (None, Some(99))] {
        let answer = rig
            .split(
                CHAT,
                SplitRequest {
                    through_rowid,
                    only_rowid,
                    ..request()
                },
            )
            .await;
        assert_eq!(answer.status, 409, "{answer:?}");
        assert_eq!(
            answer.body,
            json!({ "error": "that message is not in this chat" })
        );
    }
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn split_of_an_empty_chat_has_nothing_to_copy() {
    let rig = Rig::new(Fake::default());
    let answer = rig.split(EMPTY, request()).await;
    assert_eq!(answer.status, 409, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({ "error": "that chat has nothing to copy yet" })
    );
    assert!(!rig.worktree().join(".context").exists());
    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn split_where_the_chat_is_not_reachable_answers_each_check() {
    let rig = Rig::new(Fake::default());

    let nowhere = rig.split("s-nowhere", request()).await;
    assert_eq!(nowhere.status, 404, "{nowhere:?}");
    assert_eq!(error(&nowhere), "workspace for session not found");

    let archived = rig.split(ARCHIVED_CHAT, request()).await;
    assert_eq!(archived.status, 404, "{archived:?}");
    assert_eq!(error(&archived), "workspace for session not found");

    let no_tree = rig.split(ELSEWHERE, request()).await;
    assert_eq!(no_tree.status, 409, "{no_tree:?}");
    assert_eq!(no_tree.body, json!({ "error": "worktree path unresolved" }));

    let closed = rig.split(HIDDEN, request()).await;
    assert_eq!(closed.status, 404, "{closed:?}");
    assert_eq!(
        closed.body,
        json!({ "error": "chat not found in that workspace" })
    );

    let other_workspace = rig
        .split(
            CHAT,
            SplitRequest {
                workspace_id: Some(NO_TREE.to_owned()),
                ..request()
            },
        )
        .await;
    assert_eq!(other_workspace.status, 409, "{other_workspace:?}");
    assert_eq!(error(&other_workspace), "worktree path unresolved");

    assert!(rig.urls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn split_whose_new_chat_never_appears_keeps_the_attachment_in_its_answer() {
    let rig = Rig::new(Fake {
        opens: 0,
        ..Fake::default()
    });
    let answer = rig.split(CHAT, request()).await;
    assert_eq!(answer.status, 502, "{answer:?}");
    let path = answer.body["attachment"]["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let bytes = rig.written(&answer).len();
    assert_eq!(
        answer.body,
        json!({
            "ok": false,
            "strategy": "accessibility",
            "error": NO_NEW_CHAT,
            "destination": "chat",
            "attachment": {
                "name": "Transcript of Two.md",
                "path": path,
                "bytes": bytes,
                "kept": 6,
                "elided": { "thinking": 0, "tools": 1, "earlier": 0, "later": 0 },
            },
        })
    );
}
