//! The context breakdown read.
//!
//! Every row comes from the invented database of `support/seed_context.rs` or is built in a
//! test, in a temporary SQLite file. Each test has a database of its own: the table of kept
//! results is process-wide but keyed by the database path.

#[path = "support/seed_context.rs"]
mod seed_context;
mod support;

use conductor_remote::reads::context::{ContextBreakdown, ForkTokens};
use conductor_remote::reads::Reads;
use conductor_remote::transcript::context::{estimate_text_tokens, ContextAccumulator};
use conductor_remote::transcript::render::{render_transcript, RenderFormat};
use conductor_remote::transcript::{parse_message, StoredMessage};
use seed_context::{frame, insert_chat, insert_message, CHAT, CLOSED, OTHER};
use serde_json::{json, Value};
use support::TestDb;

/// The relay's reads over the seeded database. The `TestDb` is returned too: it owns the files.
fn seeded() -> (TestDb, Reads) {
    let test = TestDb::new();
    seed_context::seed(&test.conn());
    let reads = Reads::new(test.db(), test.root());
    (test, reads)
}

fn breakdown(reads: &Reads, chat: &str) -> ContextBreakdown {
    reads
        .context_breakdown(chat)
        .unwrap()
        .expect("the chat is open")
}

#[test]
fn the_breakdown_matches_the_golden_json() {
    let (_test, reads) = seeded();
    let actual = serde_json::to_value(breakdown(&reads, CHAT)).unwrap();
    let golden: Value = serde_json::from_str(include_str!(
        "../../../web/tests/contract/fixtures/context-breakdown.json"
    ))
    .unwrap();
    assert_eq!(actual, golden);
}

#[test]
fn the_json_keys_are_in_the_order_of_the_wire_type() {
    let (_test, reads) = seeded();
    let text = serde_json::to_string(&breakdown(&reads, CHAT)).unwrap();
    let at = |key: &str| text.find(key).unwrap_or_else(|| panic!("{key} in {text}"));
    let order = [
        at("\"totalTokens\""),
        at("\"usedPercent\""),
        at("\"compacted\""),
        at("\"categories\""),
        at("\"forkTokens\""),
    ];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
}

#[test]
fn an_unknown_chat_gives_nothing() {
    let (_test, reads) = seeded();
    assert_eq!(reads.context_breakdown("ctx-nobody").unwrap(), None);
}

#[test]
fn a_closed_chat_gives_nothing_though_it_has_rows() {
    let (_test, reads) = seeded();
    assert_eq!(reads.context_breakdown(CLOSED).unwrap(), None);
}

#[test]
fn the_rows_of_another_chat_are_not_counted() {
    let (_test, reads) = seeded();
    let other = breakdown(&reads, OTHER);
    assert_eq!(other.total_tokens, 500);
    // One prompt and one answer: 85 bytes, 20 tokens of chat; the rest is initial context.
    assert_eq!((other.categories.initial, other.categories.chat), (480, 20));
    assert_eq!(other.categories.thinking + other.categories.tools, 0);
}

/// A chat of `rows` rows in the shape of a long session, written in one transaction: turns
/// of a prompt, a thinking and calling assistant frame, a tool result and an answer, with
/// the rows of another chat in between, a result frame in every group of six rows, and a
/// compaction boundary in place of it every 120 rows, the last one in the third batch.
fn insert_long_chat(conn: &rusqlite::Connection, chat: &str, rows: u32) {
    insert_chat(conn, chat, false, 3_000, 12.5);
    conn.execute_batch("BEGIN").unwrap();
    for i in 0..rows {
        let id = format!("ctx-long-{i}");
        let (role, content) = match i % 6 {
            0 => (
                "user",
                format!("Prompt {i}: change the fetch helper, naïvely."),
            ),
            1 => (
                "assistant",
                frame(json!({ "type": "assistant", "message": { "content": [
                    { "type": "thinking", "thinking": format!("Think about step {i}.") },
                    { "type": "text", "text": format!("Looking at step {i}.") },
                    { "type": "tool_use", "id": format!("ctx-long-tool-{i}"), "name": "Read",
                      "input": { "file_path": format!("src/file-{i}.ts") } }
                ] } })),
            ),
            2 => (
                "user",
                frame(json!({ "type": "user", "message": { "content": [
                    { "type": "tool_result", "tool_use_id": format!("ctx-long-tool-{}", i - 1),
                      "content": format!("contents of file {i}") }
                ] } })),
            ),
            3 => (
                "assistant",
                frame(json!({ "type": "assistant", "message": { "content": [
                    { "type": "text", "text": format!("Done with step {i}.") }
                ] } })),
            ),
            4 if i % 120 == 52 => (
                "system",
                frame(json!({ "type": "system", "subtype": "compact_boundary" })),
            ),
            4 => (
                "assistant",
                frame(json!({ "type": "result", "subtype": "success" })),
            ),
            _ => ("user", format!("A follow-up {i}.")),
        };
        // The other chat's rows share the rowid sequence with the long chat's.
        if i % 10 == 0 {
            insert_message(conn, &format!("ctx-long-o-{i}"), OTHER, i, "user", "Other.");
        }
        insert_message(conn, &id, chat, i, role, &content);
    }
    conn.execute_batch("COMMIT").unwrap();
}

/// What the breakdown must be, computed with the transcript pieces over all the rows of the
/// chat at once, without batches.
fn unbatched(test: &TestDb, chat: &str, total: i64, percent: f64) -> ContextBreakdown {
    let conn = test.conn();
    let mut stmt = conn
        .prepare(
            "SELECT rowid, id, role, content, created_at, sent_at, queue_order \
             FROM session_messages WHERE session_id = ?1 ORDER BY rowid",
        )
        .unwrap();
    let rows: Vec<(Option<String>, StoredMessage)> = stmt
        .query_map([chat], |row| {
            Ok((
                row.get(2)?,
                StoredMessage {
                    rowid: row.get(0)?,
                    id: row.get(1)?,
                    content: row.get(3)?,
                    created_at: row.get(4)?,
                    sent_at: row.get(5)?,
                    queue_order: row.get(6)?,
                },
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut accumulator = ContextAccumulator::new();
    let mut entries = Vec::new();
    for (role, message) in &rows {
        accumulator.push(role.as_deref(), message.content.as_deref());
        entries.extend(parse_message(message, None));
    }
    let (categories, compacted) = accumulator.finish(total);
    let fork = |thinking, tools| {
        estimate_text_tokens(&render_transcript(
            &entries,
            RenderFormat { thinking, tools },
        ))
    };
    ContextBreakdown {
        total_tokens: total,
        used_percent: Some(percent),
        compacted,
        categories,
        fork_tokens: ForkTokens {
            concise: fork(false, false),
            reasoning: fork(true, false),
            full: fork(true, true),
        },
    }
}

#[test]
fn a_chat_of_more_than_two_batches_reads_as_one_pass_over_its_rows() {
    let test = TestDb::new();
    let conn = test.conn();
    seed_context::seed(&conn);
    // 1,203 rows: two full batches of 500 and a partial third.
    insert_long_chat(&conn, "ctx-long", 1_203);
    let reads = Reads::new(test.db(), test.root());

    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_messages WHERE session_id = 'ctx-long'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1_203);

    let expected = unbatched(&test, "ctx-long", 3_000, 12.5);
    // The boundary and the result frames are past the first batches, so the check is not
    // vacuous: the chat is compacted and its categories are not all initial context.
    assert!(expected.compacted);
    assert!(expected.categories.initial < 3_000);
    assert!(expected.categories.tools > 0 && expected.categories.thinking > 0);
    assert_eq!(breakdown(&reads, "ctx-long"), expected);
}

#[test]
fn a_chat_of_exactly_one_batch_reads_as_one_pass_over_its_rows() {
    let test = TestDb::new();
    let conn = test.conn();
    seed_context::seed(&conn);
    insert_long_chat(&conn, "ctx-long", 500);
    let reads = Reads::new(test.db(), test.root());
    assert_eq!(
        breakdown(&reads, "ctx-long"),
        unbatched(&test, "ctx-long", 3_000, 12.5)
    );
}

#[test]
fn an_unchanged_chat_returns_an_equal_result_and_a_new_row_changes_it() {
    let (test, reads) = seeded();
    let first = breakdown(&reads, CHAT);
    assert_eq!(breakdown(&reads, CHAT), first);

    insert_message(
        &test.conn(),
        "ctx-u4",
        CHAT,
        30,
        "user",
        "And bump the version, please, with a note in the changelog.",
    );
    let second = breakdown(&reads, CHAT);
    assert_ne!(second, first);
    assert!(second.fork_tokens.concise > first.fork_tokens.concise);
    // The new prompt follows the last completed turn, so the counted window is the same.
    assert_eq!(second.categories, first.categories);
    assert_eq!(breakdown(&reads, CHAT), second);
}

#[test]
fn a_changed_counter_changes_the_result() {
    let (test, reads) = seeded();
    let first = breakdown(&reads, CHAT);
    test.conn()
        .execute(
            "UPDATE sessions SET context_token_count = 9500, context_used_percent = 8 \
             WHERE id = ?1",
            [CHAT],
        )
        .unwrap();
    let second = breakdown(&reads, CHAT);
    assert_eq!(second.total_tokens, 9_500);
    assert_eq!(second.used_percent, Some(8.0));
    assert_ne!(second, first);
}

#[test]
fn a_deleted_row_changes_the_result() {
    let (test, reads) = seeded();
    let first = breakdown(&reads, CHAT);
    test.conn()
        .execute("DELETE FROM session_messages WHERE id = 'ctx-a5'", [])
        .unwrap();
    assert_ne!(breakdown(&reads, CHAT), first);
}

#[test]
fn a_rewrite_that_keeps_the_rows_and_counters_is_not_noticed() {
    // The stamp is the counters, the highest rowid and the number of rows. A row rewritten in
    // place changes none of them, so the kept result is returned: the one way to see that a
    // result is kept at all.
    let (test, reads) = seeded();
    let first = breakdown(&reads, CHAT);
    test.conn()
        .execute(
            "UPDATE session_messages SET content = ?1 WHERE id = 'ctx-u3'",
            ["x".repeat(4_000)],
        )
        .unwrap();
    assert_eq!(breakdown(&reads, CHAT), first);
}

#[test]
fn a_chat_closed_after_it_was_read_gives_nothing_again() {
    let (test, reads) = seeded();
    assert!(reads.context_breakdown(CHAT).unwrap().is_some());
    test.conn()
        .execute("UPDATE sessions SET is_hidden = 1 WHERE id = ?1", [CHAT])
        .unwrap();
    assert_eq!(reads.context_breakdown(CHAT).unwrap(), None);
}

/// The breakdown of the golden chat after its counters were set to `tokens` and `percent`.
fn with_counters(tokens: &str, percent: &str) -> ContextBreakdown {
    let (test, reads) = seeded();
    test.conn()
        .execute(
            &format!(
                "UPDATE sessions SET context_token_count = {tokens}, \
                 context_used_percent = {percent} WHERE id = ?1"
            ),
            [CHAT],
        )
        .unwrap();
    breakdown(&reads, CHAT)
}

#[test]
fn the_total_is_never_negative() {
    let result = with_counters("-250", "1.0");
    assert_eq!(result.total_tokens, 0);
    assert_eq!(result.categories, Default::default());
}

#[test]
fn a_fractional_total_is_rounded() {
    assert_eq!(with_counters("12000.4", "1.0").total_tokens, 12_000);
    assert_eq!(with_counters("12000.5", "1.0").total_tokens, 12_001);
}

#[test]
fn a_missing_total_is_zero() {
    let result = with_counters("NULL", "1.0");
    assert_eq!(result.total_tokens, 0);
    assert_eq!(result.categories, Default::default());
    // Only the categories depend on the total.
    assert!(result.fork_tokens.full > 0);
}

#[test]
fn the_categories_always_sum_to_the_total() {
    for total in ["1", "40", "9000", "123456"] {
        let result = with_counters(total, "1.0");
        let c = result.categories;
        assert_eq!(
            c.initial + c.chat + c.thinking + c.tools,
            result.total_tokens
        );
    }
}

#[test]
fn a_missing_percentage_is_left_out() {
    let result = with_counters("9000", "NULL");
    assert_eq!(result.used_percent, None);
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["usedPercent"], Value::Null);
    assert_eq!(json["totalTokens"], 9_000);
}

#[test]
fn a_stored_percentage_is_passed_on() {
    assert_eq!(with_counters("9000", "42.25").used_percent, Some(42.25));
}
