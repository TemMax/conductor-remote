//! Tool usage: what each tool cost in a range, from invented rows in a synthetic database.
//!
//! The rules are those of the research report
//! `docs/superpowers/research/2026-10-04-reads-review-search-usage.md`, section 4.2. The service
//! reads the clock itself, so every timestamp here is an offset from the present, with margins
//! of a minute.

mod support;

use std::cell::Cell;
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_remote::usage::tools::{
    ToolRange, ToolUsageError, ToolUsageRow, ToolUsageService, ToolUsageSnapshot,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use support::TestDb;

// ── rows ────────────────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// The invented rows of one test.
struct Seed {
    conn: Connection,
    next: Cell<u32>,
}

impl Seed {
    fn new(test: &TestDb) -> Self {
        Self {
            conn: test.conn(),
            next: Cell::new(0),
        }
    }

    /// `YYYY-MM-DD HH:MM:SS`, Conductor's own format, `seconds` from now.
    fn sqlite_at(&self, seconds: i64) -> String {
        self.stamp("%Y-%m-%d %H:%M:%S", seconds)
    }

    /// `YYYY-MM-DDTHH:MM:SSZ`, the format the apps write.
    fn iso_at(&self, seconds: i64) -> String {
        self.stamp("%Y-%m-%dT%H:%M:%SZ", seconds)
    }

    fn stamp(&self, format: &str, seconds: i64) -> String {
        let at = now_ms() / 1000 + seconds;
        self.conn
            .query_row(
                "SELECT strftime(?1, ?2, 'unixepoch')",
                (format, at),
                |row| row.get(0),
            )
            .unwrap()
    }

    /// A chat updated a minute ago.
    fn session(&self, id: &str, agent: Option<&str>) {
        self.session_updated(id, agent, &self.sqlite_at(-60));
    }

    fn session_updated(&self, id: &str, agent: Option<&str>, updated_at: &str) {
        self.conn
            .execute(
                "INSERT INTO sessions (id, agent_type, is_hidden, updated_at) VALUES (?1, ?2, 1, ?3)",
                (id, agent, updated_at),
            )
            .unwrap();
    }

    fn raw(&self, session: &str, created_at: &str, content: &str) {
        let n = self.next.get();
        self.next.set(n + 1);
        self.conn
            .execute(
                "INSERT INTO session_messages (id, session_id, role, content, created_at) \
                 VALUES (?1, ?2, 'assistant', ?3, ?4)",
                (format!("m{n}"), session, content, created_at),
            )
            .unwrap();
    }

    /// A frame of `blocks` saved a minute ago.
    fn message(&self, session: &str, blocks: &[Value]) {
        self.message_at(session, &self.sqlite_at(-60), blocks);
    }

    fn message_at(&self, session: &str, created_at: &str, blocks: &[Value]) {
        self.raw(session, created_at, &frame(blocks, None));
    }
}

fn frame(blocks: &[Value], parent: Option<&str>) -> String {
    let mut frame = json!({ "type": "assistant", "message": { "content": blocks } });
    if let Some(parent) = parent {
        frame["parent_tool_use_id"] = json!(parent);
    }
    frame.to_string()
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

fn result(id: &str, content: Value) -> Value {
    json!({ "type": "tool_result", "tool_use_id": id, "content": content })
}

fn text(size: usize) -> Value {
    json!("x".repeat(size))
}

/// The tokens of a block of plain ASCII: its JSON text in bytes, divided by 4, rounded up.
fn tokens(block: &Value) -> u64 {
    (block.to_string().len() as u64).div_ceil(4)
}

fn scan(test: &TestDb, range: ToolRange) -> ToolUsageSnapshot {
    ToolUsageService::new(test.path().to_path_buf())
        .read(range, false)
        .unwrap()
}

fn tool<'a>(
    snapshot: &'a ToolUsageSnapshot,
    provider: &str,
    name: Option<&str>,
) -> &'a ToolUsageRow {
    snapshot
        .providers
        .iter()
        .find(|group| group.provider == provider)
        .unwrap_or_else(|| panic!("no provider {provider}"))
        .tools
        .iter()
        .find(|row| row.name.as_deref() == name)
        .unwrap_or_else(|| panic!("no tool {name:?}"))
}

fn names(snapshot: &ToolUsageSnapshot, provider: usize) -> Vec<Option<&str>> {
    snapshot.providers[provider]
        .tools
        .iter()
        .map(|row| row.name.as_deref())
        .collect()
}

// ── the data cases of the reference ─────────────────────────────────────────────

#[test]
fn joins_results_by_id_combines_calls_by_name_and_ranks_by_inputs_plus_results() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("s", Some("claude"));
    let big = result("read1", text(4000));
    let read1 = call("read1", "Read", json!({}));
    let read2 = call("read2", "Read", json!({}));
    let edit1 = call("edit1", "Edit", json!({}));
    let short = result("read2", json!("short"));
    let done = result("edit1", json!("done"));
    // The result is saved before its call.
    seed.message("s", std::slice::from_ref(&big));
    seed.message("s", &[read1.clone(), read2.clone(), edit1.clone()]);
    seed.message("s", &[short.clone(), done.clone()]);

    let snapshot = scan(&test, ToolRange::Day);

    assert_eq!(snapshot.providers.len(), 1);
    assert_eq!(names(&snapshot, 0), [Some("Read"), Some("Edit")]);
    let read = &snapshot.providers[0].tools[0];
    let input = tokens(&read1) + tokens(&read2);
    let output = tokens(&big) + tokens(&short);
    assert_eq!(read.calls, 2);
    assert_eq!(read.input_tokens, input);
    assert_eq!(read.output_tokens, output);
    assert_eq!(read.total_tokens, input + output);
    // The largest call is the first one: its input and its 4000 byte result.
    assert_eq!(read.largest_call_tokens, tokens(&read1) + tokens(&big));
    assert!(read.largest_call_tokens > read.total_tokens / 2);
    assert!(read.largest_call_tokens < read.total_tokens);
    let edit = &snapshot.providers[0].tools[1];
    assert_eq!(edit.calls, 1);
    assert_eq!(edit.total_tokens, tokens(&edit1) + tokens(&done));
}

#[test]
fn counts_duplicate_snapshots_once_and_keeps_the_fullest_saved_result() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("s", Some("claude"));
    let first = call("id", "Read", json!({}));
    let full = result("id", text(4000));
    for block in [
        first.clone(),
        result("id", json!("short")),
        first.clone(),
        full.clone(),
        result("id", json!("short")),
    ] {
        seed.message("s", &[block]);
    }

    let snapshot = scan(&test, ToolRange::Day);

    let read = tool(&snapshot, "claude", Some("Read"));
    assert_eq!(read.calls, 1);
    assert_eq!(read.input_tokens, tokens(&first));
    assert_eq!(read.output_tokens, tokens(&full));
    assert!(read.output_tokens > 1000 && read.output_tokens < 1100);
}

#[test]
fn keeps_prior_call_names_without_charging_their_input_and_retains_unlinked_results() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("s", Some("claude"));
    // A day and a minute ago: before the window, so it names its result but costs nothing.
    seed.message_at(
        "s",
        &seed.sqlite_at(-86_400 - 60),
        &[call("old", "Bash", json!({ "command": "x".repeat(4000) }))],
    );
    let done = result("old", json!("done"));
    let orphan = result("missing", json!("orphan"));
    seed.message("s", &[done.clone(), orphan.clone()]);

    let snapshot = scan(&test, ToolRange::Day);

    let bash = tool(&snapshot, "claude", Some("Bash"));
    assert_eq!((bash.calls, bash.input_tokens), (1, 0));
    assert_eq!(bash.output_tokens, tokens(&done));
    let unnamed = tool(&snapshot, "claude", None);
    assert_eq!(unnamed.calls, 1);
    assert_eq!(unnamed.output_tokens, tokens(&orphan));
}

#[test]
fn ignores_mirrored_child_internals_malformed_frames_ordinary_json_and_binary_image_bytes() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("s", Some("claude"));
    let at = seed.sqlite_at(-60);
    seed.raw(
        "s",
        &at,
        &frame(
            &[call("child", "ChildOnly", json!({}))],
            Some("agent-parent"),
        ),
    );
    seed.raw("s", &at, "{\"tool_use\" broken");
    seed.raw(
        "s",
        &at,
        &json!({ "task": "tool_use", "input": "not a frame" }).to_string(),
    );
    let shot = call("image", "Screenshot", json!({}));
    let image =
        json!([{ "type": "image", "source": { "type": "base64", "data": "x".repeat(100_000) } }]);
    seed.message("s", &[shot.clone(), result("image", image)]);

    let snapshot = scan(&test, ToolRange::Day);

    assert_eq!(snapshot.providers[0].tools.len(), 1);
    let screenshot = tool(&snapshot, "claude", Some("Screenshot"));
    // The picture counts as the marker that replaces it.
    let marker =
        json!([{ "type": "image", "source": { "type": "base64", "data": "[binary data]" } }]);
    assert_eq!(
        screenshot.total_tokens,
        tokens(&shot) + tokens(&result("image", marker))
    );
    assert!(screenshot.total_tokens < 100);
}

#[test]
fn does_not_charge_mcp_image_data_serialized_inside_a_result_string_as_text_tokens() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("s", Some("codex"));
    let output = json!({
        "content": [
            { "type": "text", "text": "Screenshot saved." },
            { "type": "image", "mimeType": "image/png", "data": "x".repeat(400_000) }
        ]
    });
    seed.message(
        "s",
        &[
            call("image", "Screenshot", json!({})),
            result("image", json!(output.to_string())),
        ],
    );

    let snapshot = scan(&test, ToolRange::Day);

    assert!(tool(&snapshot, "codex", Some("Screenshot")).total_tokens < 150);
}

#[test]
fn includes_hidden_chats_handles_both_timestamp_formats_and_omits_older_and_future_traffic() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    // Hidden, and updated in the apps' format.
    seed.session_updated("s", Some("codex"), &seed.iso_at(-60));
    seed.message_at(
        "s",
        &seed.sqlite_at(-86_400 - 60),
        &[call("prior", "Bash", json!({}))],
    );
    seed.message_at(
        "s",
        &seed.iso_at(-86_400 + 60),
        &[result("prior", json!("boundary result"))],
    );
    seed.message_at(
        "s",
        &seed.sqlite_at(-60),
        &[
            call("now", "Read", json!({})),
            result("now", json!("a source file")),
        ],
    );
    seed.message_at(
        "s",
        &seed.iso_at(60),
        &[call("future", "Future", json!({}))],
    );
    seed.message_at(
        "s",
        &seed.sqlite_at(-2 * 86_400),
        &[call("old", "Old", json!({})), result("old", json!("older"))],
    );

    let day = scan(&test, ToolRange::Day);

    assert_eq!(day.providers[0].session_count, 1);
    let mut found = names(&day, 0);
    found.sort();
    assert_eq!(found, [Some("Bash"), Some("Read")]);
    assert_eq!(tool(&day, "codex", Some("Bash")).input_tokens, 0);
    assert!(tool(&day, "codex", Some("Bash")).output_tokens > 0);
    let week = scan(&test, ToolRange::Week);
    assert!(names(&week, 0).contains(&Some("Old")));
    assert!(!names(&week, 0).contains(&Some("Future")));
}

#[test]
fn isolates_reused_call_ids_by_chat_and_groups_opencode_under_its_provider() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("a", Some("acp"));
    seed.session("b", Some("acp"));
    seed.session("empty", Some("claude"));
    seed.message(
        "a",
        &[
            call("reused", "Read", json!({})),
            result("reused", json!("file")),
        ],
    );
    seed.message(
        "b",
        &[
            call("reused", "Read", json!({})),
            result("reused", json!("other file")),
        ],
    );

    let snapshot = scan(&test, ToolRange::Day);

    assert_eq!(snapshot.providers.len(), 1);
    let group = &snapshot.providers[0];
    assert_eq!(
        (group.provider.as_str(), group.session_count),
        ("opencode", 2)
    );
    assert_eq!(group.tools.len(), 1);
    assert_eq!(
        (group.tools[0].name.as_deref(), group.tools[0].calls),
        (Some("Read"), 2)
    );
}

// ── rules the report does not list ──────────────────────────────────────────────

#[test]
fn a_result_without_an_id_is_its_own_call_per_row_and_index() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("s", Some("claude"));
    let loose = json!({ "type": "tool_result", "content": "loose" });
    seed.message("s", &[loose.clone(), loose.clone()]);
    seed.message("s", std::slice::from_ref(&loose));

    let snapshot = scan(&test, ToolRange::Day);

    let unnamed = tool(&snapshot, "claude", None);
    assert_eq!(unnamed.calls, 3);
    assert_eq!(unnamed.output_tokens, 3 * tokens(&loose));
    assert_eq!(unnamed.largest_call_tokens, tokens(&loose));
}

#[test]
fn a_call_name_is_trimmed_and_a_blank_one_groups_under_null() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("s", Some("claude"));
    seed.message(
        "s",
        &[
            call("a", " \u{feff}Read\n", json!({})),
            call("b", "Read", json!({})),
            call("c", "  ", json!({})),
            call("d", "", json!({})),
        ],
    );

    let snapshot = scan(&test, ToolRange::Day);

    assert_eq!(tool(&snapshot, "claude", Some("Read")).calls, 2);
    assert_eq!(tool(&snapshot, "claude", None).calls, 2);
    assert_eq!(snapshot.providers[0].tools.len(), 2);
}

#[test]
fn a_call_without_bytes_in_range_is_skipped() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("s", Some("claude"));
    // Only an earlier call: it names, but nothing of it falls in the window.
    seed.message_at(
        "s",
        &seed.sqlite_at(-86_400 - 60),
        &[call("old", "Bash", json!({}))],
    );

    let snapshot = scan(&test, ToolRange::Day);

    assert!(snapshot.providers.is_empty());
}

#[test]
fn providers_are_ordered_by_name_and_a_missing_agent_is_unknown() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    for (id, agent) in [
        ("1", Some("codex")),
        ("2", None),
        ("3", Some("claude")),
        ("4", Some("acp")),
    ] {
        seed.session(id, agent);
        seed.message(id, &[call(id, "Read", json!({}))]);
    }

    let snapshot = scan(&test, ToolRange::Day);

    let providers: Vec<&str> = snapshot
        .providers
        .iter()
        .map(|group| group.provider.as_str())
        .collect();
    assert_eq!(providers, ["claude", "codex", "opencode", "unknown"]);
}

#[test]
fn tools_are_ordered_by_total_tokens_then_name_and_merged_across_chats() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session("one", Some("claude"));
    seed.session("two", Some("claude"));
    // Equal sizes: the name decides.
    seed.message(
        "one",
        &[call("a", "Beta", json!({})), call("b", "Alpha", json!({}))],
    );
    // The same tool in a second chat: its sums add up, its largest call is the larger.
    let small = call("c", "Gamma", json!({}));
    let large = call("d", "Gamma", json!({ "text": "x".repeat(400) }));
    seed.message("one", std::slice::from_ref(&small));
    seed.message("two", std::slice::from_ref(&large));

    let snapshot = scan(&test, ToolRange::Day);

    assert_eq!(snapshot.providers[0].session_count, 2);
    assert_eq!(
        names(&snapshot, 0),
        [Some("Gamma"), Some("Alpha"), Some("Beta")]
    );
    let gamma = tool(&snapshot, "claude", Some("Gamma"));
    assert_eq!(gamma.calls, 2);
    assert_eq!(gamma.total_tokens, tokens(&small) + tokens(&large));
    assert_eq!(gamma.largest_call_tokens, tokens(&large));
}

#[test]
fn only_chats_updated_in_the_range_are_scanned() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    seed.session_updated("stale", Some("claude"), &seed.sqlite_at(-2 * 86_400));
    seed.message("stale", &[call("a", "Read", json!({}))]);

    assert!(scan(&test, ToolRange::Day).providers.is_empty());
    assert_eq!(scan(&test, ToolRange::Week).providers.len(), 1);
}

#[test]
fn the_bounds_are_javascript_iso_strings_a_range_apart() {
    let test = TestDb::new();
    let conn = test.conn();
    for (range, days) in [
        (ToolRange::Day, 1),
        (ToolRange::Week, 7),
        (ToolRange::Month, 30),
    ] {
        let snapshot = scan(&test, range);
        assert_eq!(snapshot.range, range);
        // SQLite's own calendar, for the whole seconds.
        let oracle = |ms: i64| -> String {
            let whole: String = conn
                .query_row(
                    "SELECT strftime('%Y-%m-%dT%H:%M:%S', ?1, 'unixepoch')",
                    [ms.div_euclid(1000)],
                    |row| row.get(0),
                )
                .unwrap();
            format!("{whole}.{:03}Z", ms.rem_euclid(1000))
        };
        assert_eq!(snapshot.until, oracle(snapshot.fetched_at));
        assert_eq!(
            snapshot.since,
            oracle(snapshot.fetched_at - days * 86_400_000)
        );
        assert!((snapshot.fetched_at - now_ms()).abs() < 60_000);
    }
}

// ── the service ─────────────────────────────────────────────────────────────────

fn one_read_seed(test: &TestDb) -> Seed {
    let seed = Seed::new(test);
    seed.session("s", Some("claude"));
    seed.message("s", &[call("first", "Read", json!({}))]);
    seed
}

fn reads(snapshot: &ToolUsageSnapshot) -> u64 {
    tool(snapshot, "claude", Some("Read")).calls
}

#[test]
fn a_snapshot_is_served_from_the_cache_until_it_is_forced() {
    let test = TestDb::new();
    let seed = one_read_seed(&test);
    let service = ToolUsageService::new(test.path().to_path_buf());

    let first = service.read(ToolRange::Day, false).unwrap();
    assert_eq!(reads(&first), 1);
    seed.message("s", &[call("second", "Read", json!({}))]);

    assert_eq!(service.read(ToolRange::Day, false).unwrap(), first);
    // Another range is another snapshot.
    assert_eq!(reads(&service.read(ToolRange::Week, false).unwrap()), 2);
    let forced = service.read(ToolRange::Day, true).unwrap();
    assert_eq!(reads(&forced), 2);
    // The forced snapshot is the one kept.
    assert_eq!(service.read(ToolRange::Day, false).unwrap(), forced);
}

#[test]
fn a_snapshot_older_than_the_ttl_is_scanned_again() {
    let test = TestDb::new();
    let seed = one_read_seed(&test);
    let service = ToolUsageService::with_limits(
        test.path().to_path_buf(),
        Duration::ZERO,
        Duration::from_secs(60),
    );

    assert_eq!(reads(&service.read(ToolRange::Day, false).unwrap()), 1);
    seed.message("s", &[call("second", "Read", json!({}))]);

    assert_eq!(reads(&service.read(ToolRange::Day, false).unwrap()), 2);
}

/// The database in rollback-journal mode, so that an exclusive transaction of the test holds a
/// scan at its first query; the scan waits for the lock (up to two seconds) and goes on when
/// it is released.
fn lock_everyone_out(test: &TestDb) -> Connection {
    test.conn()
        .pragma_update(None, "journal_mode", "delete")
        .unwrap();
    let writer = test.conn();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    writer
}

#[test]
fn reads_of_one_range_share_a_scan_a_forced_read_joins_it_and_other_ranges_queue_behind_it() {
    let test = TestDb::new();
    // Leaving the journal mode needs the database to itself: the seeding connection goes first.
    drop(one_read_seed(&test));
    let writer = lock_everyone_out(&test);
    let service = ToolUsageService::new(test.path().to_path_buf());

    let (first, second, forced, queued, released) = thread::scope(|scope| {
        let first = scope.spawn(|| service.read(ToolRange::Day, false));
        // The first scan has started, and waits for the database.
        thread::sleep(Duration::from_millis(400));
        let second = scope.spawn(|| service.read(ToolRange::Day, false));
        let forced = scope.spawn(|| service.read(ToolRange::Day, true));
        let queued = scope.spawn(|| service.read(ToolRange::Week, false));
        thread::sleep(Duration::from_millis(400));
        let released = now_ms();
        writer.execute_batch("COMMIT").unwrap();
        (
            first.join().unwrap().unwrap(),
            second.join().unwrap().unwrap(),
            forced.join().unwrap().unwrap(),
            queued.join().unwrap().unwrap(),
            released,
        )
    });

    // One scan answered all three, and it began before the lock was let go.
    assert_eq!(second, first);
    assert_eq!(forced, first);
    assert!(first.fetched_at < released);
    // The other range scanned after it, not beside it.
    assert_eq!(queued.range, ToolRange::Week);
    assert!(queued.fetched_at >= released);
    // The shared scan is the cached one.
    assert_eq!(service.read(ToolRange::Day, false).unwrap(), first);
}

#[test]
fn an_error_is_not_cached_and_the_read_can_be_retried_when_the_source_appears() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("later.db");
    let service = ToolUsageService::new(missing.clone());

    assert!(matches!(
        service.read(ToolRange::Day, false),
        Err(ToolUsageError::Read(_))
    ));
    create_database(&missing);

    assert!(service
        .read(ToolRange::Day, false)
        .unwrap()
        .providers
        .is_empty());
}

fn create_database(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(support::SCHEMA).unwrap();
}

#[test]
fn a_scan_past_its_deadline_is_abandoned_and_nothing_is_kept() {
    let test = TestDb::new();
    let seed = Seed::new(&test);
    for id in ["a", "b", "c"] {
        seed.session(id, Some("claude"));
        seed.message(id, &[call(id, "Read", json!({}))]);
    }
    let service = ToolUsageService::with_limits(
        test.path().to_path_buf(),
        Duration::from_secs(60),
        Duration::from_nanos(1),
    );

    assert!(matches!(
        service.read(ToolRange::Day, false),
        Err(ToolUsageError::TimedOut)
    ));
    // The range is no longer under way: the next read scans, and fails the same way, at once.
    assert!(matches!(
        service.read(ToolRange::Day, false),
        Err(ToolUsageError::TimedOut)
    ));
    // The data itself is fine.
    let patient = ToolUsageService::new(test.path().to_path_buf());
    assert_eq!(
        patient.read(ToolRange::Day, false).unwrap().providers[0].session_count,
        3
    );
}
