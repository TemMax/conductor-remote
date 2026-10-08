#[path = "support/seed_sessions.rs"]
mod seed_sessions;
mod support;

use conductor_remote::reads::sessions::{ClosedSession, SessionRow};
use conductor_remote::reads::Reads;
use serde_json::{json, Value};
use support::TestDb;

/// The relay's reads over the seeded database. The `TestDb` is returned too: it owns the files.
fn seeded() -> (TestDb, Reads) {
    let test = TestDb::new();
    seed_sessions::seed(&test.conn());
    let reads = Reads::new(test.db(), test.root());
    (test, reads)
}

fn open_chats() -> Vec<SessionRow> {
    let (_test, reads) = seeded();
    reads.list_sessions(seed_sessions::WORKSPACE).unwrap()
}

fn closed_chats() -> Vec<ClosedSession> {
    let (_test, reads) = seeded();
    reads
        .list_closed_sessions(seed_sessions::WORKSPACE)
        .unwrap()
}

fn open_chat(id: &str) -> SessionRow {
    open_chats()
        .into_iter()
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("{id} is not an open chat"))
}

#[test]
fn open_chats_are_in_tab_order_by_the_stored_creation_time() {
    let ids: Vec<_> = open_chats().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        [
            "ses-claude-5m",
            "ses-codex-ultra",
            "ses-codex-no-effort",
            "ses-cursor",
            "ses-anthropic-1h",
            "ses-claude-mixed",
            "ses-claude-zero",
            // Created at 07:00 by the clock, but "T" sorts after " ": the stored text decides.
            "ses-iso-created",
            "ses-running-turn",
            "ses-queued",
            "ses-legacy-queue",
            "ses-legacy-open",
        ]
    );
}

#[test]
fn a_workspace_gets_only_its_own_chats() {
    let (_test, reads) = seeded();
    assert!(reads.list_sessions("ses-elsewhere").unwrap().is_empty());
    assert!(reads
        .list_closed_sessions("ses-elsewhere")
        .unwrap()
        .is_empty());
}

#[test]
fn an_empty_database_has_no_chats() {
    let test = TestDb::new();
    let reads = Reads::new(test.db(), test.root());
    assert!(reads.list_sessions("ses-workspace").unwrap().is_empty());
    assert!(reads
        .list_closed_sessions("ses-workspace")
        .unwrap()
        .is_empty());
}

#[test]
fn hidden_chats_are_closed_and_the_rest_are_open() {
    let open: Vec<_> = open_chats().into_iter().map(|s| s.id).collect();
    let closed: Vec<_> = closed_chats().into_iter().map(|s| s.id).collect();
    assert!(open.iter().all(|id| !id.starts_with("ses-closed-")));
    assert!(closed.iter().all(|id| id.starts_with("ses-closed-")));
    assert_eq!(open.len() + closed.len(), 18);
}

#[test]
fn a_null_is_hidden_counts_as_open_and_not_as_closed() {
    let (test, reads) = seeded();
    let null_rows: i64 = test
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE is_hidden IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(null_rows, 1);

    let open = reads.list_sessions(seed_sessions::WORKSPACE).unwrap();
    assert!(open.iter().any(|s| s.id == "ses-legacy-open"));
    let closed = reads
        .list_closed_sessions(seed_sessions::WORKSPACE)
        .unwrap();
    assert!(closed.iter().all(|s| s.id != "ses-legacy-open"));
}

#[test]
fn closed_chats_are_most_recently_updated_first_across_both_timestamp_formats() {
    let ids: Vec<_> = closed_chats().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        [
            // 13:00 in the space format is later than 12:00 in the ISO format.
            "ses-closed-space",
            "ses-closed-iso",
            // Same update time: the later creation first, then the id.
            "ses-closed-tie-c",
            "ses-closed-tie-a",
            "ses-closed-tie-b",
            "ses-closed-old",
        ]
    );
}

#[test]
fn timestamps_are_served_as_stored_in_both_formats() {
    let space = open_chat("ses-claude-5m");
    assert_eq!(space.created_at, "2026-09-01 08:00:00");
    assert_eq!(space.updated_at, "2026-09-01 08:30:00");
    let iso = open_chat("ses-iso-created");
    assert_eq!(iso.created_at, "2026-09-02T07:00:00.000Z");
    assert_eq!(iso.updated_at, "2026-09-02T07:30:00.000Z");
    assert_eq!(
        iso.last_user_message_at.as_deref(),
        Some("2026-09-02T07:20:00.000Z")
    );
    let closed = closed_chats();
    assert_eq!(closed[1].updated_at, "2026-09-10T12:00:00.000Z");
}

#[test]
fn effort_comes_from_the_column_of_the_agent_type() {
    // (chat, agent type, effort served)
    let cases = [
        // The stale `codex_thinking_level` of a Claude chat is ignored.
        ("ses-claude-5m", "claude", Some("high")),
        ("ses-anthropic-1h", "anthropic", Some("max")),
        ("ses-cursor", "cursor", Some("xhigh")),
        ("ses-codex-ultra", "codex", Some("ultracode")),
        // No fallback from a Codex chat to the Claude column.
        ("ses-codex-no-effort", "codex", None),
    ];
    for (id, agent, effort) in cases {
        let chat = open_chat(id);
        assert_eq!(chat.agent_type.as_deref(), Some(agent), "{id}");
        assert_eq!(chat.claude_effort_level.as_deref(), effort, "{id}");
    }
}

#[test]
fn a_codex_ultra_is_served_as_ultracode_and_other_values_are_not_renamed() {
    assert_eq!(
        open_chat("ses-codex-ultra").claude_effort_level.as_deref(),
        Some("ultracode")
    );
    assert_eq!(
        open_chat("ses-claude-zero").claude_effort_level.as_deref(),
        Some("none")
    );
    assert_eq!(
        open_chat("ses-claude-mixed").claude_effort_level.as_deref(),
        Some("low")
    );
}

#[test]
fn the_prompt_cache_lifetime_is_that_of_the_newest_claude_cache_write() {
    let ttl = |id| open_chat(id).prompt_cache_ttl_ms;
    // Five-minute write.
    assert_eq!(ttl("ses-claude-5m"), Some(300_000));
    // An older five-minute write, then a one-hour write: the newest decides.
    assert_eq!(ttl("ses-anthropic-1h"), Some(3_600_000));
    // One write reporting both: the shorter lifetime wins.
    assert_eq!(ttl("ses-claude-mixed"), Some(300_000));
    // Counters of zero are no write.
    assert_eq!(ttl("ses-claude-zero"), None);
    // Other agents report none, even when their transcript has the counters.
    assert_eq!(ttl("ses-codex-ultra"), None);
    // No messages at all.
    assert_eq!(ttl("ses-legacy-open"), None);
}

#[test]
fn a_running_turn_started_with_its_first_message_not_the_steering_one() {
    // Turn "f2" began at 10:00; the steering message at 10:05 and the assistant reply share
    // its turn id; the older turn "f1" began at 09:00.
    assert_eq!(
        open_chat("ses-running-turn").turn_started_at.as_deref(),
        Some("2026-09-03 10:00:00")
    );
}

#[test]
fn a_queued_message_does_not_start_a_turn() {
    // "ses-running-turn" also has an unsent message of a later turn, and "ses-queued" has one
    // after its only sent turn: neither moves the turn start.
    assert_eq!(
        open_chat("ses-queued").turn_started_at.as_deref(),
        Some("2026-09-03 09:10:00")
    );
    assert_eq!(
        open_chat("ses-running-turn").turn_started_at.as_deref(),
        Some("2026-09-03 10:00:00")
    );
}

#[test]
fn without_turn_ids_the_latest_sent_message_of_the_legacy_queue_starts_the_turn() {
    assert_eq!(
        open_chat("ses-legacy-queue").turn_started_at.as_deref(),
        Some("2026-08-20 12:30:00")
    );
}

#[test]
fn turn_ids_win_over_the_legacy_queue() {
    // "ses-running-turn" has a legacy message sent on 2026-08-01 and turn-id messages after it.
    let started = open_chat("ses-running-turn").turn_started_at;
    assert_eq!(started.as_deref(), Some("2026-09-03 10:00:00"));
}

#[test]
fn a_chat_without_messages_has_no_turn_start() {
    assert_eq!(open_chat("ses-legacy-open").turn_started_at, None);
    assert_eq!(open_chat("ses-codex-ultra").turn_started_at, None);
}

#[test]
fn null_columns_are_served_as_nulls() {
    let chat = open_chat("ses-legacy-open");
    assert_eq!(chat.status, None);
    assert_eq!(chat.title, None);
    assert_eq!(chat.model, None);
    assert_eq!(chat.permission_mode, None);
    assert_eq!(chat.claude_effort_level, None);
    assert_eq!(chat.fast_mode, None);
    assert_eq!(chat.agent_type, None);
    assert_eq!(chat.context_used_percent, None);
    assert_eq!(chat.unread_count, None);
    assert_eq!(chat.last_user_message_at, None);
}

#[test]
fn counters_and_flags_are_served_as_numbers() {
    let chat = open_chat("ses-claude-5m");
    assert_eq!(chat.fast_mode, Some(1));
    assert_eq!(chat.unread_count, Some(1));
    assert_eq!(chat.context_used_percent, Some(42.5));
    assert_eq!(chat.permission_mode.as_deref(), Some("plan"));
}

#[test]
fn background_tasks_are_empty_for_every_chat() {
    assert!(open_chats().iter().all(|s| s.background_tasks.is_empty()));
}

#[test]
fn a_session_row_has_exactly_the_wire_keys() {
    let value = serde_json::to_value(open_chat("ses-claude-5m")).unwrap();
    let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "agent_type",
            "background_tasks",
            "claude_effort_level",
            "context_used_percent",
            "created_at",
            "fast_mode",
            "id",
            "last_user_message_at",
            "model",
            "permission_mode",
            "prompt_cache_ttl_ms",
            "status",
            "title",
            "turn_started_at",
            "unread_count",
            "updated_at",
        ]
    );
    assert_eq!(value["background_tasks"], json!([]));
}

#[test]
fn a_closed_session_has_exactly_the_wire_keys() {
    let value = serde_json::to_value(closed_chats().remove(0)).unwrap();
    let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "agent_type",
            "created_at",
            "id",
            "model",
            "title",
            "updated_at"
        ]
    );
}

#[test]
fn list_sessions_matches_the_golden_json() {
    let (_test, reads) = seeded();
    let actual = serde_json::to_value(reads.list_sessions(seed_sessions::WORKSPACE).unwrap());
    let golden: Value = serde_json::from_str(include_str!(
        "../../../web/tests/contract/fixtures/sessions.json"
    ))
    .unwrap();
    assert_eq!(actual.unwrap(), golden);
}

#[test]
fn list_closed_sessions_matches_the_golden_json() {
    let (_test, reads) = seeded();
    let actual = serde_json::to_value(
        reads
            .list_closed_sessions(seed_sessions::WORKSPACE)
            .unwrap(),
    );
    let golden: Value = serde_json::from_str(include_str!(
        "../../../web/tests/contract/fixtures/sessions-closed.json"
    ))
    .unwrap();
    assert_eq!(actual.unwrap(), golden);
}
