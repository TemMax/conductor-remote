//! The context breakdown of a chat and the transcript text a fork attaches.
//!
//! Every input here is invented. The rule names are those of the research report
//! `docs/superpowers/research/2026-10-04-reads-transcript.md`, section 3.

use conductor_remote::transcript::context::{
    estimate_text_tokens, ContextAccumulator, ContextCategories,
};
use conductor_remote::transcript::render::{render_transcript, RenderFormat};
use conductor_remote::transcript::{parse_message, StoredMessage, TranscriptEntry, TranscriptRole};
use serde_json::{json, Value};

// ── rows ────────────────────────────────────────────────────────────────────────

type Row = (Option<String>, Option<String>);

fn prompt(text: &str) -> Row {
    (Some("user".to_owned()), Some(text.to_owned()))
}

fn raw(role: &str, content: &str) -> Row {
    (Some(role.to_owned()), Some(content.to_owned()))
}

/// A frame as Conductor stores it: under the assistant role, typed by its own `type`.
fn sdk(kind: &str, blocks: Value) -> Row {
    raw(
        "assistant",
        &json!({ "type": kind, "message": { "content": blocks } }).to_string(),
    )
}

fn child(kind: &str, blocks: Value) -> Row {
    raw(
        "assistant",
        &json!({ "type": kind, "parent_tool_use_id": "tool-child", "message": { "content": blocks } })
            .to_string(),
    )
}

fn result() -> Row {
    raw("result", &json!({ "type": "result" }).to_string())
}

fn child_result() -> Row {
    raw(
        "result",
        &json!({ "type": "result", "parent_tool_use_id": "tool-child" }).to_string(),
    )
}

fn boundary() -> Row {
    raw(
        "assistant",
        &json!({ "type": "system", "subtype": "compact_boundary" }).to_string(),
    )
}

fn child_boundary() -> Row {
    raw(
        "assistant",
        &json!({ "type": "system", "subtype": "compact_boundary", "parent_tool_use_id": "tool-child" })
            .to_string(),
    )
}

/// A block of `kind` whose JSON text is exactly `bytes` long, padded through `field`.
fn sized(kind: &str, field: &str, bytes: usize) -> Value {
    let base = json!({ "type": kind, field: "" }).to_string().len();
    assert!(bytes >= base, "a {kind} block is at least {base} bytes");
    json!({ "type": kind, field: "p".repeat(bytes - base) })
}

fn estimate(rows: &[Row], total: i64) -> (ContextCategories, bool) {
    let mut accumulator = ContextAccumulator::new();
    for (role, content) in rows {
        accumulator.push(role.as_deref(), content.as_deref());
    }
    accumulator.finish(total)
}

fn sum(categories: ContextCategories) -> i64 {
    categories.initial + categories.chat + categories.thinking + categories.tools
}

fn ceil4(bytes: usize) -> i64 {
    i64::try_from(bytes.div_ceil(4)).unwrap()
}

// ── the reference cases ─────────────────────────────────────────────────────────

#[test]
fn separates_chat_thinking_and_tools_while_preserving_the_total() {
    let (categories, compacted) = estimate(
        &[
            prompt("Please inspect this."),
            sdk(
                "assistant",
                json!([
                    { "type": "thinking", "thinking": "I should inspect the relevant source first." },
                    { "type": "tool_use", "name": "Read", "input": { "file_path": "src/index.ts" } },
                    { "type": "text", "text": "The issue is in the parser." }
                ]),
            ),
            sdk(
                "user",
                json!([{ "type": "tool_result", "content": "export const answer = 42" }]),
            ),
            result(),
        ],
        500,
    );
    assert!(!compacted);
    assert!(categories.chat > 0);
    assert!(categories.thinking > 0);
    assert!(categories.tools > 0);
    assert!(categories.initial > 0);
    assert_eq!(sum(categories), 500);
}

#[test]
fn starts_at_the_latest_completed_compaction_boundary() {
    let (categories, compacted) = estimate(
        &[
            sdk(
                "assistant",
                json!([{ "type": "tool_use", "name": "Bash", "input": { "command": "x".repeat(4000) } }]),
            ),
            boundary(),
            prompt("Continue from the summary."),
            sdk("assistant", json!([{ "type": "text", "text": "Done." }])),
            result(),
        ],
        200,
    );
    assert!(compacted);
    assert_eq!(categories.tools, 0);
    assert!(categories.chat > 0);
    assert_eq!(sum(categories), 200);
}

#[test]
fn does_not_mix_a_streaming_turn_into_the_completed_total() {
    let (categories, _) = estimate(
        &[
            prompt("Finished prompt"),
            sdk(
                "assistant",
                json!([{ "type": "text", "text": "Finished answer" }]),
            ),
            result(),
            sdk(
                "assistant",
                json!([{ "type": "tool_use", "name": "Read", "input": { "file_path": "large".repeat(2000) } }]),
            ),
        ],
        100,
    );
    assert_eq!(categories.tools, 0);
    assert_eq!(sum(categories), 100);
}

#[test]
fn a_subagent_result_is_not_the_parent_turn_boundary() {
    let (categories, _) = estimate(
        &[
            prompt("Parent prompt"),
            sdk(
                "assistant",
                json!([{ "type": "text", "text": "Parent answer" }]),
            ),
            result(),
            child_result(),
            sdk(
                "assistant",
                json!([{ "type": "tool_use", "name": "Read", "input": { "file_path": "child-only" } }]),
            ),
        ],
        100,
    );
    assert_eq!(categories.tools, 0);
    assert_eq!(sum(categories), 100);
}

#[test]
fn a_delegated_childs_frames_are_not_charged_to_the_parent() {
    let (categories, _) = estimate(
        &[
            prompt("Delegate this."),
            sdk(
                "assistant",
                json!([{ "type": "tool_use", "name": "Agent", "input": { "prompt": "Inspect it." } }]),
            ),
            child(
                "assistant",
                json!([{ "type": "tool_use", "name": "Read", "input": { "file_path": "x".repeat(20_000) } }]),
            ),
            sdk(
                "user",
                json!([{ "type": "tool_result", "content": "The child found one issue." }]),
            ),
            sdk(
                "assistant",
                json!([{ "type": "text", "text": "There is one issue." }]),
            ),
            result(),
        ],
        1000,
    );
    assert!(categories.tools < 100);
    assert!(categories.initial > 800);
    assert_eq!(sum(categories), 1000);
}

#[test]
fn excludes_signatures_and_fits_an_overshoot_proportionally() {
    let (categories, _) = estimate(
        &[
            prompt(&"x".repeat(400)),
            sdk(
                "assistant",
                json!([
                    { "type": "thinking", "thinking": "y".repeat(400), "signature": "s".repeat(40_000) },
                    { "type": "tool_use", "name": "Bash", "input": { "command": "z".repeat(400) } }
                ]),
            ),
            result(),
        ],
        10,
    );
    assert_eq!(categories.initial, 0);
    assert!(categories.chat > 0);
    assert!(categories.thinking > 0);
    assert!(categories.tools > 0);
    assert_eq!(sum(categories), 10);
}

#[test]
fn a_human_json_prompt_stays_in_chat() {
    let (categories, _) = estimate(
        &[
            prompt(&json!({ "task": "inspect this object", "depth": 2 }).to_string()),
            sdk(
                "assistant",
                json!([{ "type": "text", "text": "I inspected it." }]),
            ),
            result(),
        ],
        100,
    );
    assert!(categories.chat > 10);
    assert_eq!(sum(categories), 100);
}

#[test]
fn an_embedded_data_uri_is_not_tool_prose() {
    let (categories, _) = estimate(
        &[
            sdk(
                "assistant",
                json!([{
                    "type": "tool_use",
                    "name": "InspectImage",
                    "input": { "image_url": format!("data:image/png;base64,{}", "x".repeat(40_000)) }
                }]),
            ),
            result(),
        ],
        20_000,
    );
    assert!(categories.tools < 100);
    assert!(categories.initial > 19_000);
}

#[test]
fn fork_estimates_use_utf8_bytes() {
    assert_eq!(estimate_text_tokens("1234"), 1);
    assert_eq!(estimate_text_tokens("🙂"), 1);
    assert_eq!(estimate_text_tokens("12345"), 2);
    assert_eq!(estimate_text_tokens(""), 0);
}

#[test]
fn excludes_mcp_images_serialized_in_a_tool_result() {
    let inner = json!({ "content": [{ "type": "image", "mimeType": "image/png", "data": "x".repeat(400_000) }] });
    let (categories, _) = estimate(
        &[
            sdk(
                "user",
                json!([{ "type": "tool_result", "content": inner.to_string() }]),
            ),
            result(),
        ],
        200_000,
    );
    assert!(categories.tools < 100);
    assert_eq!(sum(categories), 200_000);
}

// ── 1. frame recognition ────────────────────────────────────────────────────────

#[test]
fn rule_1_only_typed_frames_are_frames() {
    // A message frame whose content is not an array is no frame: under the user role it
    // is a prompt, under any other role it is not counted at all.
    let odd = json!({ "type": "assistant", "message": { "content": "plain" } }).to_string();
    let (as_user, _) = estimate(&[raw("user", &odd)], 1000);
    assert_eq!(as_user.chat, ceil4(odd.len()));
    let (as_assistant, _) = estimate(&[raw("assistant", &odd)], 1000);
    assert_eq!(
        as_assistant,
        ContextCategories {
            initial: 1000,
            ..Default::default()
        }
    );

    // Malformed JSON and a JSON array behave the same way.
    let (broken, _) = estimate(&[raw("assistant", "{not json")], 1000);
    assert_eq!(broken.chat, 0);
    let (broken_user, _) = estimate(&[raw("user", "{not json")], 1000);
    assert_eq!(broken_user.chat, ceil4("{not json".len()));

    // Text that does not start with a brace is chat whatever its role, and a missing
    // content is an empty prompt.
    let (plain, _) = estimate(&[raw("assistant", " {\"type\":\"result\"}")], 1000);
    assert_eq!(plain.chat, ceil4(" {\"type\":\"result\"}".len()));
    let (missing, _) = estimate(&[(None, None)], 1000);
    assert_eq!(missing.chat, 0);

    // An `error` frame is a frame: a following streaming row is still counted, because
    // it is not a result, but the error frame itself has no blocks to count.
    let (error, _) = estimate(
        &[raw("assistant", "{\"type\":\"error\",\"error\":\"boom\"}")],
        1000,
    );
    assert_eq!(error.chat, 0);
}

// ── 2. root frames ──────────────────────────────────────────────────────────────

#[test]
fn rule_2_a_subagent_compaction_does_not_reset_the_window() {
    let (categories, compacted) = estimate(
        &[
            prompt(&"a".repeat(40)),
            child_boundary(),
            prompt(&"b".repeat(40)),
            result(),
        ],
        1000,
    );
    assert!(!compacted);
    assert_eq!(categories.chat, 20);

    // A falsy parent id (null, empty string) is a root frame.
    let empty_parent =
        json!({ "type": "system", "subtype": "compact_boundary", "parent_tool_use_id": "" });
    let (categories, compacted) = estimate(
        &[
            prompt(&"a".repeat(40)),
            raw("assistant", &empty_parent.to_string()),
            prompt(&"b".repeat(40)),
        ],
        1000,
    );
    assert!(compacted);
    assert_eq!(categories.chat, 10);
}

// ── 3. the completed cut ────────────────────────────────────────────────────────

#[test]
fn rule_3_the_cut_is_the_last_root_result_or_the_last_row() {
    // No result: everything counts.
    let (open, _) = estimate(&[prompt(&"a".repeat(40)), prompt(&"b".repeat(40))], 1000);
    assert_eq!(open.chat, 20);
    // A result in the middle: the rows after it wait for their own result.
    let (middle, _) = estimate(
        &[prompt(&"a".repeat(40)), result(), prompt(&"b".repeat(40))],
        1000,
    );
    assert_eq!(middle.chat, 10);
    // Two results: the later one is the cut.
    let (later, _) = estimate(
        &[
            prompt(&"a".repeat(40)),
            result(),
            prompt(&"b".repeat(40)),
            result(),
            prompt(&"c".repeat(40)),
        ],
        1000,
    );
    assert_eq!(later.chat, 20);
    // No rows at all.
    let (empty, compacted) = estimate(&[], 50);
    assert_eq!(
        empty,
        ContextCategories {
            initial: 50,
            ..Default::default()
        }
    );
    assert!(!compacted);
}

// ── 4. the compaction boundary ──────────────────────────────────────────────────

#[test]
fn rule_4_only_a_boundary_at_or_before_the_cut_counts() {
    // A boundary after the last result belongs to a turn still streaming.
    let (after, compacted) = estimate(
        &[
            prompt(&"a".repeat(40)),
            result(),
            boundary(),
            prompt(&"b".repeat(40)),
        ],
        1000,
    );
    assert!(!compacted);
    assert_eq!(after.chat, 10);

    // Of two boundaries before the cut, the later one starts the window.
    let (two, compacted) = estimate(
        &[
            prompt(&"a".repeat(40)),
            boundary(),
            prompt(&"b".repeat(40)),
            boundary(),
            prompt(&"c".repeat(80)),
            result(),
        ],
        1000,
    );
    assert!(compacted);
    assert_eq!(two.chat, 20);

    // Without any result the cut is the last row, so a trailing boundary empties the window.
    let (trailing, compacted) = estimate(&[prompt(&"a".repeat(40)), boundary()], 1000);
    assert!(compacted);
    assert_eq!(trailing.chat, 0);

    // A system frame with another subtype is no boundary.
    let other = json!({ "type": "system", "subtype": "init" }).to_string();
    let (init, compacted) = estimate(&[prompt(&"a".repeat(40)), raw("assistant", &other)], 1000);
    assert!(!compacted);
    assert_eq!(init.chat, 10);
}

// ── 5. byte counting ────────────────────────────────────────────────────────────

#[test]
fn rule_5_blocks_are_counted_by_type() {
    let thinking = sized("thinking", "thinking", 40);
    let redacted = sized("redacted_thinking", "data", 40);
    let reasoning = sized("reasoning", "summary", 40);
    let tool_use = sized("tool_use", "name", 80);
    let tool_result = sized("tool_result", "content", 80);
    let text = sized("text", "text", 120);
    let output_text = sized("output_text", "text", 120);
    let (categories, _) = estimate(
        &[
            prompt(&"a".repeat(8)),
            sdk(
                "assistant",
                json!([thinking, redacted, reasoning, tool_use, text, output_text, "loose string", 7, ["array"], { "type": "image" }]),
            ),
            // In a user frame text is tool plumbing, not chat.
            sdk("user", json!([tool_result, sized("text", "text", 400)])),
            result(),
        ],
        10_000,
    );
    assert_eq!(categories.thinking, 30);
    assert_eq!(categories.tools, 40);
    assert_eq!(categories.chat, 2 + 60);
    assert_eq!(categories.initial, 10_000 - 30 - 40 - 62);

    // Byte sums are rounded up per category, not per row.
    let (rounded, _) = estimate(&[prompt("a"), prompt("b"), prompt("c")], 100);
    assert_eq!(rounded.chat, 1);

    // UTF-8 bytes, not characters.
    let (wide, _) = estimate(&[prompt("ąćęł")], 100);
    assert_eq!(wide.chat, 2);
}

// ── 6. the size of a block ──────────────────────────────────────────────────────

fn tool_bytes(block: Value) -> i64 {
    estimate(&[sdk("assistant", json!([block]))], 1_000_000)
        .0
        .tools
}

#[test]
fn rule_6_the_size_drops_signatures_and_binary_strings() {
    // Signatures and encrypted content are left out entirely.
    let thinking = json!({ "type": "thinking", "thinking": "abcd", "signature": "s".repeat(999) });
    let (categories, _) = estimate(&[sdk("assistant", json!([thinking]))], 1000);
    assert_eq!(
        categories.thinking,
        ceil4(r#"{"type":"thinking","thinking":"abcd"}"#.len())
    );
    let reasoning =
        json!({ "type": "reasoning", "encrypted_content": "e".repeat(999), "summary": [] });
    let (categories, _) = estimate(&[sdk("assistant", json!([reasoning]))], 1000);
    assert_eq!(
        categories.thinking,
        ceil4(r#"{"type":"reasoning","summary":[]}"#.len())
    );

    // Any string that starts with a base64 data URI, at any depth, in any case.
    let uri = json!({ "type": "tool_use", "input": { "list": [format!("DATA:image/png;BASE64,{}", "x".repeat(999))] } });
    assert_eq!(
        tool_bytes(uri),
        ceil4(r#"{"type":"tool_use","input":{"list":["[binary data]"]}}"#.len())
    );
    // A data URI that is not base64, or not at the start, stays.
    let text_uri = json!({ "type": "tool_use", "input": "data:text/plain,hello" });
    assert_eq!(
        tool_bytes(text_uri),
        ceil4(r#"{"type":"tool_use","input":"data:text/plain,hello"}"#.len())
    );

    // `data` beside a binary type.
    let image =
        json!({ "type": "tool_result", "content": [{ "type": "image", "data": "x".repeat(999) }] });
    assert_eq!(
        tool_bytes(image),
        ceil4(
            r#"{"type":"tool_result","content":[{"type":"image","data":"[binary data]"}]}"#.len()
        )
    );
    let text_data = json!({ "type": "tool_use", "input": { "type": "text", "data": "abc" } });
    assert_eq!(
        tool_bytes(text_data),
        ceil4(r#"{"type":"tool_use","input":{"type":"text","data":"abc"}}"#.len())
    );

    // `blob` beside a string mime type.
    let blob = json!({ "type": "tool_use", "input": { "mimeType": "image/png", "blob": "x".repeat(999) } });
    assert_eq!(
        tool_bytes(blob),
        ceil4(
            r#"{"type":"tool_use","input":{"mimeType":"image/png","blob":"[binary data]"}}"#.len()
        )
    );
}

#[test]
fn rule_6_serialized_tool_results_are_cleaned_only_when_something_changes() {
    // The serialized result loses its image and is written compactly.
    let inner = r#"{ "content": [ { "type": "image", "data": "xxxxxxxx" } ] }"#;
    let block = json!({ "type": "tool_result", "content": inner });
    let cleaned = r#"{"content":[{"type":"image","data":"[binary data]"}]}"#;
    let expected = json!({ "type": "tool_result", "content": cleaned }).to_string();
    assert_eq!(tool_bytes(block), ceil4(expected.len()));

    // It names a "data" key but nothing is binary: the original text, spaces and all.
    let harmless = r#"{ "data" : "plain words" }"#;
    let block = json!({ "type": "tool_result", "content": harmless });
    let expected = json!({ "type": "tool_result", "content": harmless }).to_string();
    assert_eq!(tool_bytes(block), ceil4(expected.len()));

    // Not JSON at all: kept as it is.
    let prose = r#"the key "data": was missing"#;
    let block = json!({ "type": "tool_result", "content": prose });
    let expected = json!({ "type": "tool_result", "content": prose }).to_string();
    assert_eq!(tool_bytes(block), ceil4(expected.len()));
}

// ── 7. fitting ──────────────────────────────────────────────────────────────────

/// Rows whose estimates are exactly `chat`, `thinking` and `tools` tokens.
/// A zero leaves the block out; a block's own JSON makes a non-zero thinking or tools
/// estimate at least 9 tokens.
fn measured(chat: usize, thinking: usize, tools: usize) -> Vec<Row> {
    let mut blocks = Vec::new();
    if thinking > 0 {
        blocks.push(sized("thinking", "thinking", thinking * 4));
    }
    if tools > 0 {
        blocks.push(sized("tool_use", "name", tools * 4));
    }
    vec![
        prompt(&"c".repeat(chat * 4)),
        sdk("assistant", Value::Array(blocks)),
        result(),
    ]
}

#[test]
fn rule_7_an_estimate_within_the_total_leaves_the_rest_as_initial() {
    let (categories, _) = estimate(&measured(50, 30, 20), 1000);
    assert_eq!(
        categories,
        ContextCategories {
            initial: 900,
            chat: 50,
            thinking: 30,
            tools: 20
        }
    );
    let (exact, _) = estimate(&measured(50, 30, 20), 100);
    assert_eq!(
        exact,
        ContextCategories {
            initial: 0,
            chat: 50,
            thinking: 30,
            tools: 20
        }
    );
    assert_eq!(sum(exact), 100);
}

#[test]
fn rule_7_an_overshoot_is_scaled_and_the_remainder_goes_to_the_largest_fraction() {
    // 3.5, 2.1 and 1.4 floor to 3, 2, 1; the one token left goes to chat (0.5).
    let (categories, _) = estimate(&measured(50, 30, 20), 7);
    assert_eq!(
        categories,
        ContextCategories {
            initial: 0,
            chat: 4,
            thinking: 2,
            tools: 1
        }
    );
    assert_eq!(sum(categories), 7);

    // Equal fractions are served in chat, thinking, tools order.
    let (tied, _) = estimate(&measured(10, 10, 10), 4);
    assert_eq!(
        tied,
        ContextCategories {
            initial: 0,
            chat: 2,
            thinking: 1,
            tools: 1
        }
    );
    let (two_left, _) = estimate(&measured(10, 10, 10), 5);
    assert_eq!(
        two_left,
        ContextCategories {
            initial: 0,
            chat: 2,
            thinking: 2,
            tools: 1
        }
    );
}

#[test]
fn rule_7_a_zero_or_negative_total_is_all_zeros() {
    let (zero, _) = estimate(&measured(5, 10, 10), 0);
    assert_eq!(zero, ContextCategories::default());
    let (negative, _) = estimate(&measured(5, 10, 10), -40);
    assert_eq!(negative, ContextCategories::default());
    let (nothing, _) = estimate(&[], 0);
    assert_eq!(nothing, ContextCategories::default());
}

#[test]
fn rule_7_the_categories_sum_to_the_total_in_every_branch() {
    for (chat, thinking, tools) in [
        (0, 0, 0),
        (1, 0, 0),
        (0, 0, 12),
        (7, 13, 29),
        (50, 30, 20),
        (333, 9, 0),
        (10, 10, 10),
    ] {
        for total in [0, 1, 2, 3, 7, 10, 59, 100, 1000, 123_457] {
            let (categories, _) = estimate(&measured(chat, thinking, tools), total);
            assert_eq!(
                sum(categories),
                total,
                "{chat}/{thinking}/{tools} in {total}"
            );
            assert!(categories.initial >= 0 && categories.chat >= 0);
            assert!(categories.thinking >= 0 && categories.tools >= 0);
        }
    }
}

// ── the one pass equals the slice-based definition ──────────────────────────────

/// The report's definition, written plainly: hold every row, find the cut and the
/// boundary by index, then sum the slice between them. Block sizes here are the compact
/// JSON text, which is what the accumulator measures for blocks without opaque keys.
fn sliced(rows: &[Row]) -> (ContextCategories, bool) {
    let frames: Vec<Option<Value>> = rows
        .iter()
        .map(|(_, content)| {
            let content = content.as_deref().unwrap_or("");
            if !content.starts_with('{') {
                return None;
            }
            let frame: Value = serde_json::from_str(content).ok()?;
            let kind = frame.get("type").and_then(Value::as_str);
            let blocks = frame.get("message").and_then(|m| m.get("content"));
            match kind {
                Some("assistant" | "user") if blocks.is_some_and(Value::is_array) => Some(frame),
                Some("system" | "result" | "error") => Some(frame),
                _ => None,
            }
        })
        .collect();
    let root = |frame: &Value| {
        frame
            .get("parent_tool_use_id")
            .is_none_or(|id| id.is_null() || id.as_str() == Some(""))
    };
    let kind = |frame: &Value| frame.get("type").and_then(Value::as_str).map(str::to_owned);

    let last_result = frames.iter().rposition(|f| {
        f.as_ref()
            .is_some_and(|f| root(f) && kind(f).as_deref() == Some("result"))
    });
    let cut = last_result.map_or(rows.len() as isize - 1, |i| i as isize);
    let mut boundary: isize = -1;
    for i in 0..=cut {
        if let Some(f) = &frames[i as usize] {
            if root(f)
                && kind(f).as_deref() == Some("system")
                && f.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
            {
                boundary = i;
            }
        }
    }

    let (mut chat, mut thinking, mut tools) = (0usize, 0usize, 0usize);
    for i in (boundary + 1)..=cut {
        let i = i as usize;
        let content = rows[i].1.as_deref().unwrap_or("");
        let Some(frame) = &frames[i] else {
            if rows[i].0.as_deref() == Some("user") || !content.starts_with('{') {
                chat += content.len();
            }
            continue;
        };
        if !root(frame) {
            continue;
        }
        let Some(blocks) = frame
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for block in blocks.iter().filter(|b| b.is_object()) {
            let t = block.get("type").and_then(Value::as_str).unwrap_or("");
            let size = block.to_string().len();
            if t == "tool_use" || t == "tool_result" {
                tools += size;
            } else if t.contains("thinking") || t.contains("reasoning") {
                thinking += size;
            } else if (t == "text" || t == "output_text") && kind(frame).as_deref() != Some("user")
            {
                chat += size;
            }
        }
    }
    let (chat, thinking, tools) = (ceil4(chat), ceil4(thinking), ceil4(tools));
    const TOTAL: i64 = 1_000_000_000;
    (
        ContextCategories {
            initial: TOTAL - chat - thinking - tools,
            chat,
            thinking,
            tools,
        },
        boundary >= 0,
    )
}

#[test]
fn the_one_pass_equals_the_slice_based_definition() {
    let talk = |n: usize| {
        vec![
            prompt(&format!("question {n} {}", "q".repeat(n * 7))),
            sdk(
                "assistant",
                json!([
                    { "type": "thinking", "thinking": "t".repeat(n * 11) },
                    { "type": "tool_use", "name": "Read", "input": { "file_path": "f".repeat(n * 13) } }
                ]),
            ),
            sdk(
                "user",
                json!([{ "type": "tool_result", "content": "r".repeat(n * 17) }]),
            ),
            sdk(
                "assistant",
                json!([{ "type": "text", "text": "a".repeat(n * 19) }]),
            ),
        ]
    };
    let subagent = || {
        vec![
            child(
                "assistant",
                json!([{ "type": "tool_use", "name": "Grep", "input": "g".repeat(500) }]),
            ),
            child_boundary(),
            child(
                "assistant",
                json!([{ "type": "text", "text": "child prose" }]),
            ),
            child_result(),
        ]
    };
    let shapes: Vec<(&str, Vec<Row>)> = vec![
        ("empty", vec![]),
        ("no result", [talk(1), talk(2)].concat()),
        ("result at the end", [talk(1), vec![result()]].concat()),
        (
            "result in the middle",
            [talk(1), vec![result()], talk(3)].concat(),
        ),
        (
            "two results",
            [talk(1), vec![result()], talk(2), vec![result()], talk(4)].concat(),
        ),
        (
            "boundary before the cut",
            [talk(1), vec![boundary()], talk(2), vec![result()], talk(3)].concat(),
        ),
        (
            "boundary after the cut",
            [talk(1), vec![result()], vec![boundary()], talk(2)].concat(),
        ),
        (
            "boundaries on both sides of the cut",
            [
                talk(1),
                vec![boundary()],
                talk(2),
                vec![result()],
                vec![boundary()],
                talk(3),
            ]
            .concat(),
        ),
        (
            "boundary without a result",
            [talk(1), vec![boundary()], talk(2)].concat(),
        ),
        (
            "subagent frames",
            [
                talk(1),
                subagent(),
                talk(2),
                vec![result()],
                subagent(),
                talk(3),
            ]
            .concat(),
        ),
        (
            "subagent frames after a boundary",
            [
                talk(2),
                vec![boundary()],
                subagent(),
                talk(1),
                vec![result()],
            ]
            .concat(),
        ),
        (
            "plain and odd rows",
            vec![
                raw("assistant", "{broken"),
                raw("user", "{broken"),
                (None, None),
                raw(
                    "assistant",
                    &json!({ "type": "assistant", "message": {} }).to_string(),
                ),
                prompt(&json!({ "task": "x" }).to_string()),
                result(),
                raw("assistant", "trailing"),
            ],
        ),
    ];
    for (name, rows) in shapes {
        assert_eq!(estimate(&rows, 1_000_000_000), sliced(&rows), "{name}");
    }
}

// ── rendering ───────────────────────────────────────────────────────────────────

fn entry(role: TranscriptRole, text: &str) -> TranscriptEntry {
    TranscriptEntry {
        id: "message-1".to_owned(),
        rowid: 1,
        role,
        text: text.to_owned(),
        tool: None,
        detail: None,
        tool_use_id: None,
        parent_tool_use_id: None,
        subagent_label: None,
        output: None,
        diff: false,
        images: Vec::new(),
        error: false,
        ts: "2026-09-01T00:00:00.000Z".to_owned(),
        queued: false,
    }
}

fn call(tool: &str, text: &str, detail: Option<&str>) -> TranscriptEntry {
    TranscriptEntry {
        tool: Some(tool.to_owned()),
        detail: detail.map(str::to_owned),
        ..entry(TranscriptRole::Tool, text)
    }
}

fn output(text: &str, error: bool) -> TranscriptEntry {
    TranscriptEntry {
        output: Some(text.to_owned()),
        error,
        ..entry(TranscriptRole::Tool, text)
    }
}

const CONCISE: RenderFormat = RenderFormat {
    thinking: false,
    tools: false,
};
const REASONING: RenderFormat = RenderFormat {
    thinking: true,
    tools: false,
};
const FULL: RenderFormat = RenderFormat {
    thinking: true,
    tools: true,
};

/// Hidden entries at the start, in the middle and at the end.
fn chat() -> Vec<TranscriptEntry> {
    vec![
        entry(TranscriptRole::Thinking, "Considering."),
        call("Bash", "List files", Some("ls")),
        output("a.rs\nb.rs", false),
        entry(TranscriptRole::Assistant, "Here is the plan."),
        call("Read", "Read", Some("")),
        output("No such file", true),
        entry(TranscriptRole::Thinking, "Middle thought."),
        entry(TranscriptRole::User, "Go on."),
        entry(TranscriptRole::Assistant, "Done."),
        call("Edit", "Edit main", Some("main.rs")),
        entry(TranscriptRole::Thinking, "Trailing."),
    ]
}

#[test]
fn render_concise() {
    assert_eq!(
        render_transcript(&chat(), CONCISE),
        "## Assistant\n\n\
         [1 tool call, 1 thinking block elided]\n\n\
         Here is the plan.\n\n\
         ## User\n\n\
         [2 tool calls, 1 thinking block elided]\n\n\
         Go on.\n\n\
         ## Assistant\n\n\
         Done.\n\n\
         [1 tool call, 1 thinking block elided]\n"
    );
}

#[test]
fn render_with_reasoning() {
    assert_eq!(
        render_transcript(&chat(), REASONING),
        "## Thinking\n\n\
         Considering.\n\n\
         ## Assistant\n\n\
         [1 tool call elided]\n\n\
         Here is the plan.\n\n\
         ## Thinking\n\n\
         [2 tool calls elided]\n\n\
         Middle thought.\n\n\
         ## User\n\n\
         Go on.\n\n\
         ## Assistant\n\n\
         Done.\n\n\
         ## Thinking\n\n\
         [1 tool call elided]\n\n\
         Trailing.\n"
    );
}

#[test]
fn render_full() {
    assert_eq!(
        render_transcript(&chat(), FULL),
        "## Thinking\n\n\
         Considering.\n\n\
         ## Tools\n\n\
         - [Bash] List files — `ls`\n\n\
         ## Assistant\n\n\
         Here is the plan.\n\n\
         ## Tools\n\n\
         - [Read] Read\n\
         - [error] No such file\n\n\
         ## Thinking\n\n\
         Middle thought.\n\n\
         ## User\n\n\
         Go on.\n\n\
         ## Assistant\n\n\
         Done.\n\n\
         ## Tools\n\n\
         - [Edit] Edit main — `main.rs`\n\n\
         ## Thinking\n\n\
         Trailing.\n"
    );
}

#[test]
fn render_an_empty_or_fully_hidden_transcript() {
    assert_eq!(render_transcript(&[], FULL), "\n");
    assert_eq!(estimate_text_tokens(&render_transcript(&[], CONCISE)), 1);
    // Only successful results: nothing is printed and nothing is admitted to.
    assert_eq!(
        render_transcript(&[output("x", false), output("y", false)], CONCISE),
        "\n"
    );
    // Only hidden entries: the marker alone.
    assert_eq!(
        render_transcript(
            &[
                call("Bash", "a", None),
                entry(TranscriptRole::Thinking, "b")
            ],
            CONCISE
        ),
        "[1 tool call, 1 thinking block elided]\n"
    );
}

fn stored(content: &str, rowid: i64, id: &str) -> StoredMessage {
    StoredMessage {
        rowid,
        id: id.to_owned(),
        content: Some(content.to_owned()),
        created_at: Some("2026-09-01T00:00:00.000Z".to_owned()),
        sent_at: Some("2026-09-01T00:00:00.000Z".to_owned()),
        queue_order: None,
    }
}

/// A call, its successful result and a failed result, as the parser makes them.
fn parsed_fork_entries() -> Vec<TranscriptEntry> {
    let call = json!({ "type": "assistant", "message": { "content": [
        { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "rg -n needle src" } }
    ] } });
    let result = |id: &str, content: &str, is_error: bool| {
        json!({ "type": "user", "message": { "content": [
            { "type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error }
        ] } })
        .to_string()
    };
    [
        parse_message(&stored(&call.to_string(), 1, "message-1"), None),
        parse_message(
            &stored(
                &result("toolu_1", "src/a.ts:3:needle", false),
                2,
                "message-2",
            ),
            None,
        ),
        parse_message(
            &stored(&result("toolu_2", "no such file", true), 3, "message-3"),
            None,
        ),
    ]
    .concat()
}

#[test]
fn render_prints_the_call_and_the_failure_never_a_successful_output() {
    let text = render_transcript(&parsed_fork_entries(), FULL);
    assert!(text.contains("[Bash] Bash — `rg -n needle src`"), "{text}");
    assert!(text.contains("[error] no such file"), "{text}");
    assert!(!text.contains("src/a.ts:3:needle"), "{text}");
    assert!(!text.contains("elided"), "{text}");
}

#[test]
fn render_an_output_is_not_counted_as_an_elided_tool_call() {
    let text = render_transcript(&parsed_fork_entries(), REASONING);
    assert_eq!(text, "[2 tool calls elided]\n");
}

#[test]
fn render_a_cut_chat_without_its_thinking() {
    let cut = vec![
        entry(TranscriptRole::User, "the original question"),
        entry(TranscriptRole::Thinking, "why it might go this way"),
        entry(TranscriptRole::Assistant, "the answer"),
    ];
    assert_eq!(
        render_transcript(&cut[1..], CONCISE),
        "## Assistant\n\n[1 thinking block elided]\n\nthe answer\n"
    );
    assert_eq!(
        render_transcript(&cut, CONCISE),
        "## User\n\nthe original question\n\n## Assistant\n\n[1 thinking block elided]\n\nthe answer\n"
    );
}

#[test]
fn render_lines_headings_and_joining() {
    let entries = vec![
        entry(TranscriptRole::System, "  aborted by user"),
        TranscriptEntry {
            tool: None,
            ..entry(TranscriptRole::Tool, "unnamed")
        },
        entry(TranscriptRole::User, "- a user list item"),
        call("Grep", "Search", None),
        entry(TranscriptRole::Assistant, "Answer with trailing space   "),
    ];
    assert_eq!(
        render_transcript(&entries, FULL),
        "## System\n\n\
         \x20 aborted by user\n\n\
         ## Tools\n\n\
         - [tool] unnamed\n\n\
         ## User\n\n\
         - a user list item\n\n\
         ## Tools\n\n\
         - [Grep] Search\n\n\
         ## Assistant\n\n\
         Answer with trailing space\n"
    );
    // Consecutive tool lines form one list.
    let joined = vec![
        entry(TranscriptRole::Tool, "unused"),
        call("Bash", "first", None),
        call("Bash", "second", Some("cmd")),
    ];
    assert_eq!(
        render_transcript(&joined, FULL),
        "## Tools\n\n- [tool] unused\n- [Bash] first\n- [Bash] second — `cmd`\n"
    );
    let consecutive = vec![
        entry(TranscriptRole::Assistant, "one"),
        entry(TranscriptRole::Assistant, "two"),
    ];
    assert_eq!(
        render_transcript(&consecutive, CONCISE),
        "## Assistant\n\none\n\ntwo\n"
    );
    // The joining rule looks at the text only: prose that reads as list items joins too.
    let listed = vec![
        entry(TranscriptRole::Assistant, "- one"),
        entry(TranscriptRole::Assistant, "- two"),
    ];
    assert_eq!(
        render_transcript(&listed, CONCISE),
        "## Assistant\n\n- one\n- two\n"
    );
}
