mod support;

use conductor_remote::reads::states::{workspace_title, SessionStateRow};
use conductor_remote::reads::Reads;
use rusqlite::{params, Connection};
use serde_json::json;
use support::TestDb;

fn reads(test: &TestDb) -> Reads {
    Reads::new(test.db(), test.root())
}

fn add_repo(conn: &Connection, id: &str, name: &str) {
    conn.execute(
        "INSERT INTO repos (id, name) VALUES (?1, ?2)",
        params![id, name],
    )
    .unwrap();
}

struct Ws<'a> {
    id: &'a str,
    state: &'a str,
    repo: Option<&'a str>,
    name: Option<&'a str>,
    pr_title: Option<&'a str>,
    branch: Option<&'a str>,
    directory: Option<&'a str>,
}

impl<'a> Ws<'a> {
    fn ready(id: &'a str) -> Self {
        Self {
            id,
            state: "ready",
            repo: None,
            name: None,
            pr_title: None,
            branch: None,
            directory: None,
        }
    }
}

fn add_workspace(conn: &Connection, w: &Ws) {
    conn.execute(
        "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, state, \
                                 workspace_name, pr_title) \
         VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            w.id,
            w.repo,
            w.directory,
            w.branch,
            w.state,
            w.name,
            w.pr_title
        ],
    )
    .unwrap();
}

fn add_chat(
    conn: &Connection,
    id: &str,
    workspace: &str,
    title: &str,
    hidden: Option<i64>,
    last_user_message_at: Option<&str>,
) {
    conn.execute(
        "INSERT INTO sessions (id, status, title, workspace_id, is_hidden, last_user_message_at) \
         VALUES (?1, 'working', ?2, ?3, ?4, ?5)",
        params![id, title, workspace, hidden, last_user_message_at],
    )
    .unwrap();
}

/// A user prompt of a turn, dispatched at `sent_at`.
fn add_prompt(conn: &Connection, id: &str, session: &str, turn: &str, sent_at: &str) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, role, content, sent_at, turn_id) \
         VALUES (?1, ?2, 'user', 'hello', ?3, ?4)",
        params![id, session, sent_at, turn],
    )
    .unwrap();
}

fn add_frame(conn: &Connection, id: &str, session: &str, content: String) {
    conn.execute(
        "INSERT INTO session_messages (id, session_id, content) VALUES (?1, ?2, ?3)",
        params![id, session, content],
    )
    .unwrap();
}

fn assistant_text(id: &str, session: &str, text: &str) -> (String, String, String) {
    (
        id.to_owned(),
        session.to_owned(),
        json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": text }
        ] } })
        .to_string(),
    )
}

fn say(conn: &Connection, id: &str, session: &str, text: &str) {
    let (id, session, content) = assistant_text(id, session, text);
    add_frame(conn, &id, &session, content);
}

fn by_session(rows: Vec<SessionStateRow>) -> Vec<SessionStateRow> {
    let mut rows = rows;
    rows.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    rows
}

fn ids(rows: &[SessionStateRow]) -> Vec<&str> {
    rows.iter().map(|r| r.session_id.as_str()).collect()
}

#[test]
fn hidden_chats_and_workspaces_that_are_not_ready_are_left_out() {
    let test = TestDb::new();
    let conn = test.conn();
    add_workspace(&conn, &Ws::ready("w-ready"));
    add_workspace(
        &conn,
        &Ws {
            state: "setting_up",
            ..Ws::ready("w-setup")
        },
    );
    add_workspace(
        &conn,
        &Ws {
            state: "archived",
            ..Ws::ready("w-archived")
        },
    );
    add_chat(&conn, "c-open", "w-ready", "Open", Some(0), None);
    add_chat(&conn, "c-null", "w-ready", "Null flag", None, None);
    add_chat(&conn, "c-hidden", "w-ready", "Hidden", Some(1), None);
    add_chat(&conn, "c-setup", "w-setup", "Setting up", Some(0), None);
    add_chat(&conn, "c-archived", "w-archived", "Archived", Some(0), None);
    add_chat(
        &conn,
        "c-orphan",
        "w-missing",
        "No workspace",
        Some(0),
        None,
    );

    let rows = by_session(reads(&test).session_states().unwrap());
    assert_eq!(ids(&rows), ["c-null", "c-open"]);
    assert!(rows.iter().all(|r| r.workspace_id == "w-ready"));
    assert_eq!(rows[0].status.as_deref(), Some("working"));
}

#[test]
fn the_chat_title_is_named_only_when_the_workspace_has_more_than_one_open_chat() {
    let test = TestDb::new();
    let conn = test.conn();
    add_workspace(&conn, &Ws::ready("w-solo"));
    add_workspace(&conn, &Ws::ready("w-pair"));
    add_workspace(&conn, &Ws::ready("w-hidden-pair"));
    add_chat(&conn, "solo", "w-solo", "Solo chat", Some(0), None);
    add_chat(&conn, "pair-a", "w-pair", "Chat A", Some(0), None);
    add_chat(&conn, "pair-b", "w-pair", "Chat B", None, None);
    add_chat(&conn, "hp-open", "w-hidden-pair", "Visible", Some(0), None);
    // A hidden chat is not a tab: it does not make the other one ambiguous.
    add_chat(&conn, "hp-hidden", "w-hidden-pair", "Closed", Some(1), None);

    let rows = by_session(reads(&test).session_states().unwrap());
    let titles: Vec<_> = rows
        .iter()
        .map(|r| (r.session_id.as_str(), r.session_title.as_deref()))
        .collect();
    assert_eq!(
        titles,
        [
            ("hp-open", None),
            ("pair-a", Some("Chat A")),
            ("pair-b", Some("Chat B")),
            ("solo", None),
        ]
    );
}

#[test]
fn the_row_carries_the_workspace_title_and_the_repository() {
    let test = TestDb::new();
    let conn = test.conn();
    add_repo(&conn, "r1", "anvil");
    add_workspace(
        &conn,
        &Ws {
            repo: Some("r1"),
            name: Some("Checkout flow"),
            pr_title: Some("Ignored PR title"),
            ..Ws::ready("w-named")
        },
    );
    add_workspace(
        &conn,
        &Ws {
            branch: Some("alice/fix-login_flow"),
            directory: Some("managua"),
            ..Ws::ready("w-branch")
        },
    );
    add_chat(&conn, "c-named", "w-named", "Chat", Some(0), None);
    add_chat(&conn, "c-branch", "w-branch", "Chat", Some(0), None);

    let rows = by_session(reads(&test).session_states().unwrap());
    assert_eq!(rows[0].session_id, "c-branch");
    assert_eq!(rows[0].workspace_title, "Fix login flow");
    assert_eq!(rows[0].repo_name, None);
    assert_eq!(rows[1].workspace_title, "Checkout flow");
    assert_eq!(rows[1].repo_name.as_deref(), Some("anvil"));
}

#[test]
fn the_turn_start_and_the_last_prompt_time_come_through() {
    let test = TestDb::new();
    let conn = test.conn();
    add_workspace(&conn, &Ws::ready("w"));
    add_chat(
        &conn,
        "turn",
        "w",
        "Turn",
        Some(0),
        Some("2026-09-14T10:05:00.000Z"),
    );
    add_chat(&conn, "legacy", "w", "Legacy", Some(0), None);
    add_chat(&conn, "quiet", "w", "Quiet", Some(0), None);
    // An older turn, then the latest turn made of two dispatches: its earliest one counts.
    add_prompt(&conn, "p1", "turn", "t1", "2026-09-14T09:00:00.000Z");
    add_prompt(&conn, "p2", "turn", "t2", "2026-09-14T10:00:00.000Z");
    add_prompt(&conn, "p3", "turn", "t2", "2026-09-14T10:05:00.000Z");
    // A legacy queued message: no turn id, a queue order, the latest dispatch counts.
    for (id, sent) in [
        ("q1", "2026-09-14T08:00:00.000Z"),
        ("q2", "2026-09-14T08:30:00.000Z"),
    ] {
        conn.execute(
            "INSERT INTO session_messages (id, session_id, role, content, sent_at, queue_order) \
             VALUES (?1, 'legacy', 'user', 'queued', ?2, 1)",
            params![id, sent],
        )
        .unwrap();
    }

    let rows = by_session(reads(&test).session_states().unwrap());
    let get = |id: &str| rows.iter().find(|r| r.session_id == id).unwrap();
    assert_eq!(
        get("turn").turn_started_at.as_deref(),
        Some("2026-09-14T10:00:00.000Z")
    );
    assert_eq!(
        get("turn").last_user_message_at.as_deref(),
        Some("2026-09-14T10:05:00.000Z")
    );
    assert_eq!(
        get("legacy").turn_started_at.as_deref(),
        Some("2026-09-14T08:30:00.000Z")
    );
    assert_eq!(get("legacy").last_user_message_at, None);
    assert_eq!(get("quiet").turn_started_at, None);
}

#[test]
fn no_chats_is_an_empty_list() {
    let test = TestDb::new();
    assert!(reads(&test).session_states().unwrap().is_empty());
}

#[test]
fn the_last_assistant_text_is_the_newest_one_trimmed() {
    let test = TestDb::new();
    let conn = test.conn();
    say(&conn, "a1", "chat", "An older answer.");
    say(&conn, "a2", "chat", "  \n The newest answer.\n ");
    say(&conn, "o1", "other", "Another chat's answer.");
    assert_eq!(
        reads(&test).last_assistant_text("chat").unwrap().as_deref(),
        Some("The newest answer.")
    );
}

#[test]
fn the_last_assistant_text_skips_tool_user_and_empty_rows() {
    let test = TestDb::new();
    let conn = test.conn();
    say(&conn, "a1", "chat", "The real answer.");
    // After it: a tool call, its result, a blank text, a plain prompt and a frame with no entry.
    add_frame(
        &conn,
        "a2",
        "chat",
        json!({ "type": "assistant", "message": { "content": [
            { "type": "tool_use", "id": "t1", "name": "Bash",
              "input": { "description": "Run the tests", "command": "cargo test" } }
        ] } })
        .to_string(),
    );
    add_frame(
        &conn,
        "r1",
        "chat",
        json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": "t1", "content": "ok" }
        ] } })
        .to_string(),
    );
    say(&conn, "a3", "chat", "   \n\t ");
    add_frame(&conn, "u1", "chat", "Please continue".to_owned());
    add_frame(
        &conn,
        "end",
        "chat",
        json!({ "type": "result", "subtype": "success" }).to_string(),
    );
    assert_eq!(
        reads(&test).last_assistant_text("chat").unwrap().as_deref(),
        Some("The real answer.")
    );
}

#[test]
fn within_one_row_the_last_text_wins() {
    let test = TestDb::new();
    let conn = test.conn();
    add_frame(
        &conn,
        "a1",
        "chat",
        json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "First part." },
            { "type": "tool_use", "id": "t1", "name": "Read", "input": { "file_path": "a" } },
            { "type": "text", "text": "Second part." }
        ] } })
        .to_string(),
    );
    assert_eq!(
        reads(&test).last_assistant_text("chat").unwrap().as_deref(),
        Some("Second part.")
    );
}

#[test]
fn the_last_assistant_text_looks_no_further_than_twenty_rows() {
    let test = TestDb::new();
    let conn = test.conn();
    say(&conn, "a0", "chat", "Too far back.");
    // Nineteen rows after it: the answer is the 20th newest and still found.
    for i in 0..19 {
        add_frame(&conn, &format!("u{i}"), "chat", format!("prompt {i}"));
    }
    assert_eq!(
        reads(&test).last_assistant_text("chat").unwrap().as_deref(),
        Some("Too far back.")
    );
    // One more pushes it out of the window.
    add_frame(&conn, "u19", "chat", "prompt 19".to_owned());
    assert_eq!(reads(&test).last_assistant_text("chat").unwrap(), None);
}

#[test]
fn an_unknown_or_silent_chat_has_no_text() {
    let test = TestDb::new();
    let conn = test.conn();
    add_frame(&conn, "u1", "quiet", "Hello".to_owned());
    let reads = reads(&test);
    assert_eq!(reads.last_assistant_text("quiet").unwrap(), None);
    assert_eq!(reads.last_assistant_text("nobody").unwrap(), None);
}

#[test]
fn the_workspace_title_falls_back_in_order() {
    let id = "0123456789abcdef";
    // The name wins.
    assert_eq!(
        workspace_title(Some("My name"), Some("PR"), Some("a/b"), Some("dir"), id),
        "My name"
    );
    // Then the PR title.
    assert_eq!(
        workspace_title(None, Some("Add retries"), Some("a/b"), Some("dir"), id),
        "Add retries"
    );
    // Then the humanised branch: the prefix goes, dashes and underscores become spaces.
    assert_eq!(
        workspace_title(None, None, Some("alice/fix-login_flow"), Some("dir"), id),
        "Fix login flow"
    );
    // Then the directory name.
    assert_eq!(
        workspace_title(None, None, None, Some("managua-v2"), id),
        "managua-v2"
    );
    // Then the first 8 characters of the id.
    assert_eq!(workspace_title(None, None, None, None, id), "01234567");
}

#[test]
fn empty_strings_count_as_missing_in_the_workspace_title() {
    let id = "abcdefghijkl";
    assert_eq!(
        workspace_title(Some(""), Some(""), Some("u/add-search"), Some(""), id),
        "Add search"
    );
    assert_eq!(
        workspace_title(Some(""), Some(""), Some(""), Some("codename"), id),
        "codename"
    );
    assert_eq!(
        workspace_title(Some(""), Some(""), Some(""), Some(""), id),
        "abcdefgh"
    );
    // A branch with nothing after its prefix, or only separators, humanises to nothing.
    assert_eq!(
        workspace_title(None, None, Some("alice/"), Some("dir"), id),
        "dir"
    );
    assert_eq!(
        workspace_title(None, None, Some("alice/-_-"), None, id),
        "abcdefgh"
    );
    // A short id is used whole.
    assert_eq!(workspace_title(None, None, None, None, "abc"), "abc");
}

#[test]
fn a_branch_is_humanised_like_the_reference_does() {
    let title = |branch: &str| workspace_title(None, None, Some(branch), None, "id");
    // No prefix: the whole branch.
    assert_eq!(title("fix-login"), "Fix login");
    // Only the first segment is the prefix.
    assert_eq!(title("alice/feature/add-x"), "Feature/add x");
    // The first letter is raised, the rest is left as it is.
    assert_eq!(title("alice/fixLogin-Now"), "FixLogin Now");
    // Leading and trailing separators are trimmed before the first letter is raised.
    assert_eq!(title("alice/_wip-"), "Wip");
    // Non-ASCII letters are raised too.
    assert_eq!(
        title("u/\u{e9}t\u{e9}-d\u{e9}j\u{e0}"),
        "\u{c9}t\u{e9} d\u{e9}j\u{e0}"
    );
}
