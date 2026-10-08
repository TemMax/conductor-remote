//! The milestone-4 contract: the create deep link, and writes that answer as before once
//! configured. The UI runs over the fake desktop and a synthetic database; nothing here reaches
//! the Mac.

mod support;

use std::sync::Arc;
use std::time::Duration;

use conductor_remote::contract::Priority;
use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{SendRequest, WriteService};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::testing::FakeCommands;
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::{UiActor, UiHandle};
use conductor_remote::ui::driver::create_link;
use conductor_remote::ui::fake::{conductor_app, main_pane, FakeDesktop, WindowSpec};
use conductor_remote::ui::keys::Key;
use rusqlite::{params, Connection};
use serde_json::json;
use support::TestDb;

const WORKSPACE: &str = "ws-1";
const CHAT: &str = "s-1";
const PROMPT: &str = "fix the tests";

// ---- create_link ----

#[test]
fn create_link_encodes_a_prompt_like_encode_uri_component() {
    assert_eq!(
        create_link(Some("fix (all) the tests&path=/etc"), None),
        "conductor://prompt=fix%20(all)%20the%20tests%26path%3D%2Fetc"
    );
}

#[test]
fn create_link_encodes_a_path_with_spaces() {
    assert_eq!(
        create_link(None, Some("/Users/me/My Projects/relay")),
        "conductor://path=%2FUsers%2Fme%2FMy%20Projects%2Frelay"
    );
}

#[test]
fn create_link_joins_both_and_keeps_the_unreserved_marks() {
    assert_eq!(
        create_link(Some("a-_.!~*'()é"), Some("/r")),
        "conductor://prompt=a-_.!~*'()%C3%A9&path=%2Fr"
    );
}

#[test]
fn create_link_without_either_is_the_bare_scheme() {
    assert_eq!(create_link(None, None), "conductor://");
    assert_eq!(create_link(Some(""), Some("")), "conductor://");
}

// ---- configure ----

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

/// One live workspace on `user/feature-x` of repo "relay" with one open chat.
fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', 'relay')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state) \
         VALUES (?1, ?1, 'r-1', 'user/feature-x', 'beta', 'ready')",
        [WORKSPACE],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
         updated_at) VALUES (?1, ?2, 'One', 'idle', 0, ?3, ?3)",
        params![CHAT, WORKSPACE, "2026-09-01 10:00:00"],
    )
    .unwrap();
}

/// A UI thread over a fake desktop showing the workspace, where Return writes the composer's
/// text as a user row of the chat.
fn fake_ui(db: std::path::PathBuf) -> UiHandle {
    UiActor::spawn(move || {
        let app = conductor_app(&WindowSpec {
            repo: "relay".to_owned(),
            branch: "user/feature-x".to_owned(),
            sidebar: vec!["beta".to_owned()],
            chats: vec!["One".to_owned()],
            selected: 0,
            composer_value: None,
        });
        let desktop = FakeDesktop::new(app.clone());
        let area = main_pane(&app).find_role("AXTextArea").expect("composer");
        let conn = Connection::open(&db).expect("open the test database for writing");
        desktop.on_key(move |key, _| {
            if key == Key::Return {
                let text = area.value_text().unwrap_or_default();
                area.set_value_text(Some(""));
                conn.execute(
                    "INSERT INTO session_messages (id, session_id, role, content, turn_id) \
                     VALUES ('m-1', ?1, 'user', ?2, 'turn-1')",
                    params![CHAT, text],
                )
                .unwrap();
            }
        });
        Box::new(Driver::new(desktop))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_writes_answer_a_send_as_before() {
    let test = TestDb::new();
    seed(&test.conn());
    let ui = fake_ui(test.path().to_path_buf());
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
    assert_eq!(
        answer.body,
        json!({
            "ok": true,
            "strategy": "accessibility",
            "attempts": 1,
            "receipt": { "kind": "message", "id": "m-1", "rowid": 1, "turnId": "turn-1" },
        })
    );
}
