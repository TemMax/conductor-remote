//! An invented data set for the session reads: one workspace, `ses-workspace`, and the chats
//! and messages that exercise each rule of `list_sessions` and `list_closed_sessions`.

use rusqlite::{params, Connection};

pub const WORKSPACE: &str = "ses-workspace";

/// One `sessions` row. `Chat::new` fills the columns Conductor's defaults would fill; override
/// the rest with struct update syntax.
struct Chat<'a> {
    id: &'a str,
    status: Option<&'a str>,
    title: Option<&'a str>,
    model: Option<&'a str>,
    permission_mode: Option<&'a str>,
    claude_effort_level: Option<&'a str>,
    codex_thinking_level: Option<&'a str>,
    fast_mode: Option<i64>,
    agent_type: Option<&'a str>,
    context_used_percent: Option<f64>,
    unread_count: Option<i64>,
    created_at: &'a str,
    updated_at: &'a str,
    last_user_message_at: Option<&'a str>,
    is_hidden: Option<i64>,
}

impl<'a> Chat<'a> {
    fn new(id: &'a str, agent_type: &'a str, created_at: &'a str) -> Self {
        Chat {
            id,
            status: Some("idle"),
            title: Some("Untitled"),
            model: None,
            permission_mode: Some("default"),
            claude_effort_level: None,
            codex_thinking_level: None,
            fast_mode: Some(0),
            agent_type: Some(agent_type),
            context_used_percent: None,
            unread_count: Some(0),
            created_at,
            updated_at: created_at,
            last_user_message_at: None,
            is_hidden: Some(0),
        }
    }

    fn insert(&self, conn: &Connection) {
        conn.execute(
            "INSERT INTO sessions (id, status, title, model, permission_mode, claude_effort_level, \
             codex_thinking_level, fast_mode, agent_type, context_used_percent, unread_count, \
             created_at, updated_at, last_user_message_at, workspace_id, is_hidden) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                self.id,
                self.status,
                self.title,
                self.model,
                self.permission_mode,
                self.claude_effort_level,
                self.codex_thinking_level,
                self.fast_mode,
                self.agent_type,
                self.context_used_percent,
                self.unread_count,
                self.created_at,
                self.updated_at,
                self.last_user_message_at,
                WORKSPACE,
                self.is_hidden,
            ],
        )
        .unwrap();
    }
}

/// One `session_messages` row. Rows are inserted in the order the seed writes them, which is
/// their rowid order.
#[derive(Default)]
struct Message<'a> {
    id: &'a str,
    session_id: &'a str,
    role: &'a str,
    content: &'a str,
    sent_at: Option<&'a str>,
    turn_id: Option<&'a str>,
    queue_order: Option<i64>,
}

impl Message<'_> {
    fn insert(&self, conn: &Connection) {
        conn.execute(
            "INSERT INTO session_messages \
             (id, session_id, role, content, sent_at, turn_id, queue_order) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.id,
                self.session_id,
                self.role,
                self.content,
                self.sent_at,
                self.turn_id,
                self.queue_order
            ],
        )
        .unwrap();
    }
}

/// An assistant frame whose usage reports the given cache writes, in tokens.
fn cache_write(five_minutes: u32, one_hour: u32) -> String {
    format!(
        "{{\"type\":\"assistant\",\"message\":{{\"usage\":{{\"cache_creation\":\
         {{\"ephemeral_5m_input_tokens\":{five_minutes},\"ephemeral_1h_input_tokens\":{one_hour}}}}}}}}}"
    )
}

pub fn seed(conn: &Connection) {
    conn.execute(
        "INSERT INTO workspaces (local_id, id, directory_name, state) \
         VALUES (?1, ?1, 'ses-directory', 'ready')",
        [WORKSPACE],
    )
    .unwrap();

    seed_open_chats(conn);
    seed_closed_chats(conn);
}

fn seed_open_chats(conn: &Connection) {
    // A Claude chat: effort in `claude_effort_level` (the stale Codex column is ignored), a
    // 5-minute prompt cache, and a space-separated timestamp.
    Chat {
        title: Some("Plan the migration"),
        model: Some("ses-model-opus"),
        permission_mode: Some("plan"),
        claude_effort_level: Some("high"),
        codex_thinking_level: Some("low"),
        fast_mode: Some(1),
        context_used_percent: Some(42.5),
        unread_count: Some(1),
        updated_at: "2026-09-01 08:30:00",
        last_user_message_at: Some("2026-09-01 08:20:00"),
        ..Chat::new("ses-claude-5m", "claude", "2026-09-01 08:00:00")
    }
    .insert(conn);
    Message {
        id: "ses-msg-a1",
        session_id: "ses-claude-5m",
        role: "user",
        content: "invented prompt",
        sent_at: Some("2026-09-01 08:20:00"),
        turn_id: Some("ses-turn-a1"),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-a2",
        session_id: "ses-claude-5m",
        role: "assistant",
        content: &cache_write(1200, 0),
        sent_at: Some("2026-09-01 08:21:00"),
        turn_id: Some("ses-turn-a1"),
        ..Default::default()
    }
    .insert(conn);

    // A Codex chat: effort in `codex_thinking_level`, where `ultra` is served as `ultracode`.
    // A cache write in its transcript is ignored, since only Claude chats report one.
    Chat {
        status: Some("working"),
        title: Some("Port the parser"),
        model: Some("ses-model-codex"),
        claude_effort_level: Some("medium"),
        codex_thinking_level: Some("ultra"),
        context_used_percent: Some(7.25),
        ..Chat::new("ses-codex-ultra", "codex", "2026-09-01 09:00:00")
    }
    .insert(conn);
    Message {
        id: "ses-msg-b1",
        session_id: "ses-codex-ultra",
        role: "assistant",
        content: &cache_write(500, 0),
        ..Default::default()
    }
    .insert(conn);

    // A Codex chat with no Codex effort: the Claude column is not a fallback.
    Chat {
        title: Some("Tidy the config"),
        claude_effort_level: Some("high"),
        ..Chat::new("ses-codex-no-effort", "codex", "2026-09-01 10:00:00")
    }
    .insert(conn);

    // Any other agent type reads `claude_effort_level`.
    Chat {
        title: Some("Sketch the panel"),
        model: Some("ses-model-cursor"),
        claude_effort_level: Some("xhigh"),
        codex_thinking_level: Some("high"),
        ..Chat::new("ses-cursor", "cursor", "2026-09-01 11:00:00")
    }
    .insert(conn);

    // The newest cache write decides: an older 5-minute write, then a 1-hour-only write.
    Chat {
        title: Some("Review the diff"),
        claude_effort_level: Some("max"),
        ..Chat::new("ses-anthropic-1h", "anthropic", "2026-09-02 08:00:00")
    }
    .insert(conn);
    Message {
        id: "ses-msg-c1",
        session_id: "ses-anthropic-1h",
        role: "assistant",
        content: &cache_write(900, 0),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-c2",
        session_id: "ses-anthropic-1h",
        role: "assistant",
        content: &cache_write(0, 700),
        ..Default::default()
    }
    .insert(conn);

    // One write that reports both lifetimes: the shorter one wins.
    Chat {
        title: Some("Mixed cache"),
        claude_effort_level: Some("low"),
        ..Chat::new("ses-claude-mixed", "claude", "2026-09-02 09:00:00")
    }
    .insert(conn);
    Message {
        id: "ses-msg-d1",
        session_id: "ses-claude-mixed",
        role: "assistant",
        content: &cache_write(300, 900),
        ..Default::default()
    }
    .insert(conn);

    // Cache counters of zero are not a cache write.
    Chat {
        title: Some("No cache write"),
        claude_effort_level: Some("none"),
        ..Chat::new("ses-claude-zero", "claude", "2026-09-02 10:00:00")
    }
    .insert(conn);
    Message {
        id: "ses-msg-e1",
        session_id: "ses-claude-zero",
        role: "assistant",
        content: &cache_write(0, 0),
        ..Default::default()
    }
    .insert(conn);

    // An ISO timestamp (`T`, `Z`) that is the earliest of its day but, as a string, sorts after
    // the space-separated ones of that day.
    Chat {
        title: Some("Clock format"),
        claude_effort_level: Some("max"),
        updated_at: "2026-09-02T07:30:00.000Z",
        last_user_message_at: Some("2026-09-02T07:20:00.000Z"),
        ..Chat::new("ses-iso-created", "claude", "2026-09-02T07:00:00.000Z")
    }
    .insert(conn);

    // A running turn: the user message that started it and a steering message sent into it share
    // a turn id, an older turn precedes them, a queued message waits behind them, and an old
    // legacy-queue message is ignored because turn ids are present.
    Chat {
        status: Some("working"),
        title: Some("Running turn"),
        claude_effort_level: Some("medium"),
        last_user_message_at: Some("2026-09-03 10:05:00"),
        ..Chat::new("ses-running-turn", "claude", "2026-09-03 08:00:00")
    }
    .insert(conn);
    Message {
        id: "ses-msg-f0",
        session_id: "ses-running-turn",
        role: "user",
        content: "legacy prompt",
        sent_at: Some("2026-08-01 07:00:00"),
        queue_order: Some(1),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-f1",
        session_id: "ses-running-turn",
        role: "user",
        content: "first prompt",
        sent_at: Some("2026-09-03 09:00:00"),
        turn_id: Some("ses-turn-f1"),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-f2",
        session_id: "ses-running-turn",
        role: "user",
        content: "second prompt",
        sent_at: Some("2026-09-03 10:00:00"),
        turn_id: Some("ses-turn-f2"),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-f3",
        session_id: "ses-running-turn",
        role: "assistant",
        content: "working on it",
        sent_at: Some("2026-09-03 10:02:00"),
        turn_id: Some("ses-turn-f2"),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-f4",
        session_id: "ses-running-turn",
        role: "user",
        content: "steering message",
        sent_at: Some("2026-09-03 10:05:00"),
        turn_id: Some("ses-turn-f2"),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-f5",
        session_id: "ses-running-turn",
        role: "user",
        content: "queued prompt",
        turn_id: Some("ses-turn-f3"),
        ..Default::default()
    }
    .insert(conn);

    // A queued message that has not been sent does not start a turn: the previous one does.
    Chat {
        title: Some("Queued behind"),
        claude_effort_level: Some("high"),
        ..Chat::new("ses-queued", "claude", "2026-09-03 09:00:00")
    }
    .insert(conn);
    Message {
        id: "ses-msg-g1",
        session_id: "ses-queued",
        role: "user",
        content: "sent prompt",
        sent_at: Some("2026-09-03 09:10:00"),
        turn_id: Some("ses-turn-g1"),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-g2",
        session_id: "ses-queued",
        role: "user",
        content: "queued prompt",
        turn_id: Some("ses-turn-g2"),
        ..Default::default()
    }
    .insert(conn);

    // Older chats have no turn ids; the latest sent message of their queue starts the turn.
    Chat {
        title: Some("Before turn ids"),
        claude_effort_level: Some("low"),
        ..Chat::new("ses-legacy-queue", "claude", "2026-09-03 10:00:00")
    }
    .insert(conn);
    Message {
        id: "ses-msg-h1",
        session_id: "ses-legacy-queue",
        role: "user",
        content: "first prompt",
        sent_at: Some("2026-08-20 12:00:00"),
        queue_order: Some(1),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-h2",
        session_id: "ses-legacy-queue",
        role: "user",
        content: "second prompt",
        sent_at: Some("2026-08-20 12:30:00"),
        queue_order: Some(2),
        ..Default::default()
    }
    .insert(conn);
    Message {
        id: "ses-msg-h3",
        session_id: "ses-legacy-queue",
        role: "user",
        content: "unsent prompt",
        queue_order: Some(3),
        ..Default::default()
    }
    .insert(conn);

    // A chat from before `is_hidden` was written: NULL counts as open. Every nullable column
    // is NULL too.
    Chat {
        status: None,
        title: None,
        permission_mode: None,
        fast_mode: None,
        agent_type: None,
        unread_count: None,
        is_hidden: None,
        ..Chat::new("ses-legacy-open", "claude", "2026-09-04 08:00:00")
    }
    .insert(conn);
}

fn seed_closed_chats(conn: &Connection) {
    // Newest by clock, though its `updated_at` is in the space-separated format.
    Chat {
        title: Some("Closed, space format"),
        model: Some("ses-model-opus"),
        updated_at: "2026-09-10 13:00:00",
        is_hidden: Some(1),
        ..Chat::new("ses-closed-space", "claude", "2026-09-05 09:00:00")
    }
    .insert(conn);
    Chat {
        title: Some("Closed, ISO format"),
        model: Some("ses-model-codex"),
        updated_at: "2026-09-10T12:00:00.000Z",
        is_hidden: Some(1),
        ..Chat::new("ses-closed-iso", "codex", "2026-09-05 08:00:00")
    }
    .insert(conn);

    // Three chats closed at the same moment: newest creation first, then by id.
    Chat {
        title: Some("Tie, created last"),
        updated_at: "2026-09-09 10:00:00",
        is_hidden: Some(1),
        ..Chat::new("ses-closed-tie-c", "claude", "2026-09-05 11:00:00")
    }
    .insert(conn);
    Chat {
        title: None,
        updated_at: "2026-09-09 10:00:00",
        is_hidden: Some(1),
        ..Chat::new("ses-closed-tie-b", "cursor", "2026-09-05 10:00:00")
    }
    .insert(conn);
    Chat {
        title: Some("Tie, same creation"),
        updated_at: "2026-09-09 10:00:00",
        is_hidden: Some(1),
        ..Chat::new("ses-closed-tie-a", "claude", "2026-09-05 10:00:00")
    }
    .insert(conn);

    Chat {
        title: Some("Closed long ago"),
        agent_type: None,
        updated_at: "2026-08-01 09:00:00",
        is_hidden: Some(1),
        ..Chat::new("ses-closed-old", "claude", "2026-08-01 08:00:00")
    }
    .insert(conn);
}
