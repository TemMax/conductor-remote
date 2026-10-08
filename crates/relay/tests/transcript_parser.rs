//! The transcript parser: one stored row in, the entries the phone renders out.
//!
//! Every input here is invented. The rule numbers (R1-R8, B1-B5) are those of the research
//! report `docs/superpowers/research/2026-10-04-reads-transcript.md`, section 2.

use conductor_remote::transcript::{
    parse_message, parse_outbox_message, StoredMessage, StoredOutboxMessage, TranscriptEntry,
    TranscriptRole,
};
use serde_json::{json, Value};

const STAMP: &str = "2026-09-01T00:00:00.000Z";
const WORKTREE: &str = "/Users/example/conductor/workspaces/project/krakow";

fn row(content: &str, rowid: i64, id: &str) -> StoredMessage {
    StoredMessage {
        rowid,
        id: id.to_owned(),
        content: Some(content.to_owned()),
        created_at: Some(STAMP.to_owned()),
        sent_at: Some(STAMP.to_owned()),
        queue_order: None,
    }
}

fn parse(content: &str) -> Vec<TranscriptEntry> {
    parse_message(&row(content, 1, "message-1"), None)
}

fn parse_frame(frame: &Value) -> Vec<TranscriptEntry> {
    parse(&frame.to_string())
}

/// The only entry of a row that must produce exactly one.
fn only(entries: Vec<TranscriptEntry>) -> TranscriptEntry {
    assert_eq!(entries.len(), 1, "expected one entry, got {entries:#?}");
    entries.into_iter().next().unwrap()
}

fn assistant_frame(blocks: Value) -> Value {
    json!({ "type": "assistant", "message": { "role": "assistant", "content": blocks } })
}

fn call_frame(tool_use_id: &str) -> String {
    assistant_frame(json!([{
        "type": "tool_use",
        "id": tool_use_id,
        "name": "Bash",
        "input": { "command": "rg -n needle src" }
    }]))
    .to_string()
}

fn result_frame(tool_use_id: &str, content: Value, is_error: bool) -> String {
    json!({
        "type": "user",
        "message": { "content": [{
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error
        }] }
    })
    .to_string()
}

/// A `user` frame whose single tool result has `content` as written, so that the test
/// controls the exact JSON text: number spelling and key order.
fn raw_result_frame(content: &str) -> String {
    format!(
        r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":{content}}}]}}}}"#
    )
}

fn tool_call(input: Value, name: &str) -> TranscriptEntry {
    only(parse_frame(&assistant_frame(json!([{
        "type": "tool_use", "id": "toolu_1", "name": name, "input": input
    }]))))
}

fn output_of(content: Value) -> Option<String> {
    only(parse(&result_frame("toolu_1", content, false))).output
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn roles_and_texts(entries: &[TranscriptEntry]) -> Vec<(TranscriptRole, &str)> {
    entries.iter().map(|e| (e.role, e.text.as_str())).collect()
}

// ---------------------------------------------------------------------------
// Ported: tool output on the wire
// ---------------------------------------------------------------------------

#[test]
fn a_call_carries_its_id_and_a_result_carries_the_output() {
    let call = only(parse(&call_frame("toolu_1")));
    let result = only(parse_message(
        &row(
            &result_frame("toolu_1", json!("src/a.ts:3:needle"), false),
            2,
            "message-2",
        ),
        None,
    ));

    assert_eq!(call.tool.as_deref(), Some("Bash"));
    assert_eq!(call.tool_use_id.as_deref(), Some("toolu_1"));
    assert_eq!(call.output, None);
    assert_eq!(result.tool_use_id.as_deref(), Some("toolu_1"));
    assert_eq!(result.output.as_deref(), Some("src/a.ts:3:needle"));
    assert_eq!(result.tool, None);
    // A success is folded onto its call, so its text would only be a second copy of the output.
    assert_eq!(result.text, "");
}

#[test]
fn reads_a_result_whose_content_is_a_block_array_and_clips_a_long_one() {
    let short = output_of(json!([{ "type": "text", "text": "two lines\nof output" }]));
    assert_eq!(short.as_deref(), Some("two lines\nof output"));

    let long = output_of(json!("x".repeat(9000))).unwrap();
    assert_eq!(utf16_len(&long), 2001);
    assert!(long.ends_with('…'));
}

#[test]
fn a_failed_result_stays_a_row_of_its_own_as_well_as_an_output() {
    let failed = only(parse(&result_frame(
        "toolu_1",
        json!("<tool_use_error>no such file</tool_use_error>"),
        true,
    )));
    assert!(failed.error);
    assert_eq!(failed.text, "no such file");
    assert_eq!(failed.output.as_deref(), Some("no such file"));
}

#[test]
fn an_empty_successful_result_is_not_a_row() {
    assert_eq!(parse(&result_frame("toolu_1", json!("   "), false)), vec![]);
}

// ---------------------------------------------------------------------------
// Ported: the shapes a result comes in
// ---------------------------------------------------------------------------

#[test]
fn an_edit_result_becomes_its_status_line_and_a_diff() {
    let content = json!({
        "status": format!("update {WORKTREE}/src/a.ts"),
        "diffString": "@@ -1,2 +1,2 @@\n-old\n+new\n"
    });
    let entry = only(parse_message(
        &row(&result_frame("toolu_1", content, false), 1, "message-1"),
        Some(WORKTREE),
    ));

    assert!(entry.diff);
    assert_eq!(
        entry.output.as_deref(),
        Some("update src/a.ts\n@@ -1,2 +1,2 @@\n-old\n+new")
    );
}

#[test]
fn a_tool_reference_list_names_the_tools() {
    let output = output_of(json!([
        { "type": "tool_reference", "tool_name": "navigate" },
        { "type": "tool_reference", "tool_name": "read_page" }
    ]));
    assert_eq!(output.as_deref(), Some("2 tools: navigate, read_page"));
}

#[test]
fn images_travel_as_references_never_as_bytes() {
    let content = json!([
        { "type": "text", "text": "captured" },
        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } }
    ]);
    let entry = only(parse_message(
        &row(&result_frame("toolu_1", content, false), 42, "message-1"),
        None,
    ));

    assert_eq!(entry.output.as_deref(), Some("captured"));
    assert_eq!(entry.images, ["42.0"]);
    assert!(!serde_json::to_string(&entry)
        .unwrap()
        .contains("iVBORw0KGgo="));
}

#[test]
fn an_image_only_result_is_still_a_row() {
    let content =
        json!([{ "type": "image", "source": { "type": "base64", "data": "iVBORw0KGgo=" } }]);
    let entry = only(parse_message(
        &row(&result_frame("toolu_1", content, false), 7, "message-1"),
        None,
    ));

    assert_eq!(entry.output.as_deref(), Some(""));
    assert_eq!(entry.images, ["7.0"]);
}

#[test]
fn an_unknown_shape_falls_back_to_its_own_json_rather_than_to_silence() {
    // Written as text, not with `json!`, so the key order is the input's and not a map's.
    let entry = only(parse(&raw_result_frame(r#"{"total":3,"kind":"summary"}"#)));
    assert_eq!(
        entry.output.as_deref(),
        Some(r#"{"total":3,"kind":"summary"}"#)
    );
}

/// The reference case also looks each image up again by its reference; that lookup is not
/// part of this module, so only the numbering half is ported.
#[test]
fn image_numbering_is_per_row() {
    let frame = json!({
        "type": "user",
        "message": { "content": [
            {
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [{ "type": "image", "source": { "type": "base64", "data": "iVBORfirst" } }]
            },
            {
                "type": "tool_result",
                "tool_use_id": "toolu_2",
                "content": [{ "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "/9j/second" } }]
            }
        ] }
    });
    let entries = parse_message(&row(&frame.to_string(), 99, "message-1"), None);

    let images: Vec<&[String]> = entries.iter().map(|e| e.images.as_slice()).collect();
    assert_eq!(images, [["99.0"], ["99.1"]]);
}

// ---------------------------------------------------------------------------
// Ported: rendering a transcript for a fork (the parser half)
// ---------------------------------------------------------------------------

/// The reference cases feed these three rows to a transcript renderer, which this project
/// does not have. What they rely on from the parser is asserted here: a call with its
/// detail, a success with no text of its own, and a failure that repeats its output.
#[test]
fn the_entries_a_fork_rendering_starts_from() {
    let mut entries = parse(&call_frame("toolu_1"));
    entries.extend(parse_message(
        &row(
            &result_frame("toolu_1", json!("src/a.ts:3:needle"), false),
            2,
            "message-2",
        ),
        None,
    ));
    entries.extend(parse_message(
        &row(
            &result_frame("toolu_2", json!("no such file"), true),
            3,
            "message-3",
        ),
        None,
    ));

    assert_eq!(entries.len(), 3);
    let (call, success, failure) = (&entries[0], &entries[1], &entries[2]);
    assert_eq!(call.tool.as_deref(), Some("Bash"));
    assert_eq!(call.text, "Bash");
    assert_eq!(call.detail.as_deref(), Some("rg -n needle src"));
    assert_eq!(success.text, "");
    assert_eq!(success.output.as_deref(), Some("src/a.ts:3:needle"));
    assert!(!success.error);
    assert_eq!(success.tool, None);
    assert_eq!(failure.text, "no such file");
    assert!(failure.error);
    assert_eq!(failure.tool, None);
}

// ---------------------------------------------------------------------------
// Ported: transcript tool details
// ---------------------------------------------------------------------------

#[test]
fn preserves_a_full_multiline_command_while_stripping_the_worktree_prefix() {
    let tail = format!("printf '%s\\n' '{}'", "detail-".repeat(32));
    let command = format!("cd {WORKTREE} && rg -n \"first\" src\n{tail}");
    let content = json!({
        "type": "assistant",
        "message": { "content": [{ "type": "tool_use", "name": "Bash", "input": { "command": command } }] }
    })
    .to_string();

    let entry = only(parse_message(
        &row(&content, 1, "message-1"),
        Some(WORKTREE),
    ));

    assert_eq!(entry.role, TranscriptRole::Tool);
    assert_eq!(entry.text, "Bash");
    let detail = entry.detail.unwrap();
    assert_eq!(detail, format!("rg -n \"first\" src\n{tail}"));
    assert!(utf16_len(&detail) > 160);
}

// ---------------------------------------------------------------------------
// Ported: subagent transcript metadata
// ---------------------------------------------------------------------------

#[test]
fn labels_a_codex_collaboration_call_from_its_task_path() {
    let entry = only(parse_frame(&assistant_frame(json!([{
        "type": "tool_use",
        "id": "call_rebase",
        "name": "collab__spawnAgent",
        "input": { "agent_nickname": "Hypatia", "agent_path": "/root/rebase_main" }
    }]))));

    assert_eq!(entry.subagent_label.as_deref(), Some("Rebase main"));
    assert_eq!(entry.tool_use_id.as_deref(), Some("call_rebase"));
}

#[test]
fn uses_a_claude_agent_description_and_carries_its_parent_id_on_every_child_entry() {
    let call = only(parse_frame(&assistant_frame(json!([{
        "type": "tool_use",
        "id": "toolu_explore",
        "name": "Agent",
        "input": { "description": "Map the message renderer", "prompt": "Find the relevant files." }
    }]))));
    let child = only(parse_message(
        &row(
            &json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "I found the parser." }] },
                "parent_tool_use_id": "toolu_explore"
            })
            .to_string(),
            2,
            "message-2",
        ),
        None,
    ));

    assert_eq!(
        call.subagent_label.as_deref(),
        Some("Map the message renderer")
    );
    assert_eq!(child.role, TranscriptRole::Assistant);
    assert_eq!(child.text, "I found the parser.");
    assert_eq!(child.parent_tool_use_id.as_deref(), Some("toolu_explore"));
}

// ---------------------------------------------------------------------------
// Row-level rules
// ---------------------------------------------------------------------------

/// R1: a row that is not a frame and holds nothing but whitespace is not an entry.
#[test]
fn a_blank_plain_row_is_not_an_entry() {
    assert_eq!(parse(""), vec![]);
    assert_eq!(parse(" \n\t\u{feff}"), vec![]);

    let mut without_content = row("", 1, "message-1");
    without_content.content = None;
    assert_eq!(parse_message(&without_content, None), vec![]);
}

/// R2: a plain row is the user's prompt, with the row's id and its text as stored.
#[test]
fn a_plain_row_is_a_user_entry_with_its_text_untrimmed() {
    let entry = only(parse_message(
        &row("  Fix the build.\n", 12, "message-12"),
        None,
    ));

    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({
            "id": "message-12",
            "rowid": 12,
            "role": "user",
            "text": "  Fix the build.\n",
            "ts": STAMP,
            "queued": false
        })
    );
}

/// R2: the test for a frame is a literal leading brace, with no trimming before it.
#[test]
fn a_row_with_whitespace_before_its_brace_is_a_plain_prompt() {
    let entry = only(parse(" {\"type\":\"system\"}"));
    assert_eq!(entry.role, TranscriptRole::User);
    assert_eq!(entry.text, " {\"type\":\"system\"}");
}

/// The base fields: a row is queued while it has a queue position and has not been sent.
#[test]
fn a_row_is_queued_while_it_has_a_queue_position_and_no_send_time() {
    let queued = |queue_order: Option<i64>, sent_at: Option<&str>| {
        let mut stored = row("Next: update the docs.", 1, "message-1");
        stored.queue_order = queue_order;
        stored.sent_at = sent_at.map(str::to_owned);
        only(parse_message(&stored, None)).queued
    };

    assert!(queued(Some(0), None));
    assert!(!queued(Some(0), Some(STAMP)));
    assert!(!queued(None, None));
    assert!(!queued(None, Some(STAMP)));
}

/// R3: a row that starts like a frame but is not JSON is shown raw, clipped to 200 units.
#[test]
fn malformed_json_becomes_a_clipped_system_entry() {
    let short = only(parse("{\"type\":\"assistant\",\"message\":"));
    assert_eq!(short.id, "message-1");
    assert_eq!(short.role, TranscriptRole::System);
    assert_eq!(short.text, "{\"type\":\"assistant\",\"message\":");

    let content = format!("{{{}", "x".repeat(400));
    let long = only(parse(&content));
    assert_eq!(long.role, TranscriptRole::System);
    assert_eq!(long.text, format!("{{{}…", "x".repeat(199)));
    assert_eq!(long.parent_tool_use_id, None);
}

/// R4: bookkeeping frames are not entries, whatever else they carry.
#[test]
fn system_and_result_frames_are_skipped() {
    assert_eq!(
        parse_frame(&json!({ "type": "system", "subtype": "init", "tools": ["Bash"] })),
        vec![]
    );
    assert_eq!(
        parse_frame(&json!({ "type": "system", "subtype": "compact_boundary" })),
        vec![]
    );
    assert_eq!(
        parse_frame(
            &json!({ "type": "result", "subtype": "success", "result": "Done.", "usage": { "input_tokens": 3 } })
        ),
        vec![]
    );
    // Even with content that would otherwise be read.
    assert_eq!(
        parse_frame(&json!({
            "type": "result",
            "message": { "content": [{ "type": "text", "text": "not shown" }] }
        })),
        vec![]
    );
}

/// R5: an error frame says in its own words how a stopped turn ended.
#[test]
fn an_error_frame_becomes_a_system_entry_with_its_own_wording() {
    let entry = only(parse_frame(
        &json!({ "type": "error", "content": "  aborted by user\n" }),
    ));
    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({
            "id": "message-1",
            "rowid": 1,
            "role": "system",
            "text": "aborted by user",
            "ts": STAMP,
            "queued": false
        })
    );

    let child = only(parse_frame(&json!({
        "type": "error",
        "content": "e".repeat(300),
        "parent_tool_use_id": " toolu_parent "
    })));
    assert_eq!(child.text, format!("{}…", "e".repeat(200)));
    assert_eq!(child.parent_tool_use_id.as_deref(), Some("toolu_parent"));
}

/// R6: a `user` or `assistant` frame without a list of blocks has nothing to show.
#[test]
fn a_user_or_assistant_frame_without_a_block_list_is_skipped() {
    assert_eq!(parse_frame(&json!({ "type": "assistant" })), vec![]);
    assert_eq!(
        parse_frame(&json!({ "type": "assistant", "message": { "content": "just a string" } })),
        vec![]
    );
    assert_eq!(
        parse_frame(&json!({ "type": "user", "message": null })),
        vec![]
    );
    assert_eq!(
        parse_frame(&json!({ "type": "user", "message": "text" })),
        vec![]
    );
}

/// R7: any other frame without a list of blocks is shown raw, so that a change in the
/// stored format stays visible.
#[test]
fn an_unknown_frame_without_a_block_list_becomes_a_raw_system_entry() {
    let content = r#"{"type":"rate_limit_event","parent_tool_use_id":"toolu_parent","info":{"status":"allowed"}}"#;
    let entry = only(parse(content));
    assert_eq!(entry.id, "message-1");
    assert_eq!(entry.role, TranscriptRole::System);
    assert_eq!(entry.text, content);
    assert_eq!(entry.parent_tool_use_id.as_deref(), Some("toolu_parent"));

    // A JSON object that is no frame at all, and one with no type.
    assert_eq!(only(parse("{}")).text, "{}");
    let long = format!(r#"{{"note":"{}"}}"#, "n".repeat(300));
    assert_eq!(
        only(parse(&long)).text,
        format!(r#"{{"note":"{}…"#, "n".repeat(191))
    );

    // An error frame whose content is not a non-blank string falls through to the same rule.
    for content in [
        r#"{"type":"error","content":"   "}"#,
        r#"{"type":"error","content":{"code":7}}"#,
        r#"{"type":"error"}"#,
    ] {
        let entry = only(parse(content));
        assert_eq!(entry.role, TranscriptRole::System);
        assert_eq!(entry.text, content);
    }
}

/// R5 falls through to R8 as well: an error frame with blocks and no wording is read as blocks.
#[test]
fn an_error_frame_without_wording_but_with_blocks_is_read_as_blocks() {
    let entries = parse_frame(&json!({
        "type": "error",
        "message": { "content": [{ "type": "text", "text": "The model stopped." }] }
    }));
    assert_eq!(
        roles_and_texts(&entries),
        [(TranscriptRole::Assistant, "The model stopped.")]
    );
}

// ---------------------------------------------------------------------------
// Block-level rules
// ---------------------------------------------------------------------------

/// B1: prose blocks join into one assistant entry, trimmed as a whole.
#[test]
fn text_blocks_join_into_one_assistant_entry() {
    let entry = only(parse_frame(&assistant_frame(json!([
        { "type": "text", "text": "\n First line." },
        { "type": "text", "text": "Second line.  " }
    ]))));

    assert_eq!(entry.id, "message-1:0");
    assert_eq!(entry.role, TranscriptRole::Assistant);
    assert_eq!(entry.text, "First line.\nSecond line.");
}

/// B1: text inside a `user` frame is injected context, never an entry.
#[test]
fn text_inside_a_user_frame_is_ignored() {
    let entries = parse_frame(&json!({
        "type": "user",
        "message": { "content": [
            { "type": "text", "text": "<system-reminder>injected</system-reminder>" },
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" }
        ] }
    }));

    let entry = only(entries);
    assert_eq!(entry.id, "message-1:0");
    assert_eq!(entry.output.as_deref(), Some("ok"));
}

/// B2: a thinking block reads `thinking`, else `text`, trimmed; one with neither is skipped.
#[test]
fn thinking_blocks_become_thinking_entries() {
    let entries = parse_frame(&assistant_frame(json!([
        { "type": "thinking", "thinking": "  Weigh the two options.  ", "signature": "sig" },
        { "type": "thinking", "thinking": "   ", "text": "Fall back to the text." },
        { "type": "thinking", "thinking": "" },
        { "type": "thinking", "thinking": 7 },
        { "type": "thinking", "text": "Only text." }
    ])));

    assert_eq!(
        roles_and_texts(&entries),
        [
            (TranscriptRole::Thinking, "Weigh the two options."),
            (TranscriptRole::Thinking, "Fall back to the text."),
            (TranscriptRole::Thinking, "Only text."),
        ]
    );
    let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["message-1:0", "message-1:1", "message-1:2"]);
}

/// B2: even a thinking block with nothing to show ends the prose before it.
#[test]
fn an_empty_thinking_block_still_ends_the_prose_before_it() {
    let entries = parse_frame(&assistant_frame(json!([
        { "type": "text", "text": "Before." },
        { "type": "thinking", "thinking": "" },
        { "type": "text", "text": "After." }
    ])));

    assert_eq!(
        roles_and_texts(&entries),
        [
            (TranscriptRole::Assistant, "Before."),
            (TranscriptRole::Assistant, "After."),
        ]
    );
}

/// B3: a tool call's entry, field by field, on the wire.
#[test]
fn a_tool_call_serialises_to_the_wire_shape() {
    let entry = only(parse_message(
        &row(
            &json!({
                "type": "assistant",
                "parent_tool_use_id": "toolu_parent",
                "message": { "content": [{
                    "type": "tool_use",
                    "id": "toolu_9",
                    "name": "Task",
                    "input": { "description": "Audit the routes", "subagent_type": "Explore", "prompt": "List every route." }
                }] }
            })
            .to_string(),
            5,
            "message-5",
        ),
        None,
    ));

    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({
            "id": "message-5:0",
            "rowid": 5,
            "role": "tool",
            "text": "Audit the routes",
            "tool": "Task",
            "detail": "List every route.",
            "toolUseId": "toolu_9",
            "parentToolUseId": "toolu_parent",
            "subagentLabel": "Audit the routes",
            "ts": STAMP,
            "queued": false
        })
    );
}

/// B3: the title is the description, else the tool name; the detail is the first non-blank
/// primary input, left out when there is none or when it repeats the title.
#[test]
fn a_tool_call_is_summarised_by_its_description_and_primary_input() {
    let described = tool_call(
        json!({ "command": "cargo test", "description": "  Run the tests  " }),
        "Bash",
    );
    assert_eq!(described.text, "Run the tests");
    assert_eq!(described.detail.as_deref(), Some("cargo test"));
    assert_eq!(described.tool.as_deref(), Some("Bash"));
    assert_eq!(described.subagent_label, None);

    // Preference order: command, file_path, path, pattern, url, skill, prompt.
    let all = json!({
        "prompt": "p", "skill": "s", "url": "u", "pattern": "pa", "path": "pt", "file_path": "f", "command": "c"
    });
    let mut input = all.as_object().unwrap().clone();
    for expected in ["c", "f", "pt", "pa", "u", "s", "p"] {
        let entry = tool_call(Value::Object(input.clone()), "Tool");
        assert_eq!(entry.detail.as_deref(), Some(expected));
        let key = [
            "command",
            "file_path",
            "path",
            "pattern",
            "url",
            "skill",
            "prompt",
        ]
        .into_iter()
        .find(|key| input.contains_key(*key))
        .unwrap();
        input.shift_remove(key);
    }

    // A blank or non-string input is passed over, and the detail is trimmed.
    let skipped = tool_call(
        json!({ "command": "  ", "file_path": 3, "pattern": " needle " }),
        "Grep",
    );
    assert_eq!(skipped.detail.as_deref(), Some("needle"));

    // No primary input: the title alone.
    let bare = tool_call(json!({ "todos": [] }), "TodoWrite");
    assert_eq!(bare.text, "TodoWrite");
    assert_eq!(bare.detail, None);

    // A detail that repeats the title is not sent twice.
    let repeated = tool_call(json!({ "description": "ls", "command": "ls" }), "Bash");
    assert_eq!(repeated.text, "ls");
    assert_eq!(repeated.detail, None);
}

/// B3: an input that is not an object gives the tool name alone, and a call without an id
/// has no `toolUseId`.
#[test]
fn a_tool_call_without_an_object_input_or_an_id_is_the_name_alone() {
    for input in [json!(null), json!("text"), json!(3), json!([])] {
        let entry = only(parse_frame(&assistant_frame(json!([{
            "type": "tool_use", "id": "  ", "name": "Mystery", "input": input
        }]))));
        assert_eq!(entry.text, "Mystery");
        assert_eq!(entry.detail, None);
        assert_eq!(entry.tool_use_id, None);
    }

    let missing = only(parse_frame(&assistant_frame(
        json!([{ "type": "tool_use", "name": "Mystery" }]),
    )));
    assert_eq!(
        serde_json::to_value(&missing).unwrap(),
        json!({
            "id": "message-1:0",
            "rowid": 1,
            "role": "tool",
            "text": "Mystery",
            "tool": "Mystery",
            "ts": STAMP,
            "queued": false
        })
    );
}

/// The worktree is taken out of a detail: a leading `cd`, then every path under it.
#[test]
fn the_worktree_is_stripped_from_a_tool_detail() {
    let detail = |input: Value, worktree: Option<&str>| {
        let frame =
            assistant_frame(json!([{ "type": "tool_use", "name": "Bash", "input": input }]));
        only(parse_message(
            &row(&frame.to_string(), 1, "message-1"),
            worktree,
        ))
        .detail
        .unwrap()
    };

    // `cd` joined by a newline rather than `&&`.
    assert_eq!(
        detail(
            json!({ "command": format!("cd {WORKTREE}\ncargo build") }),
            Some(WORKTREE)
        ),
        "cargo build"
    );
    // Paths under the worktree become relative; the bare worktree becomes a dot.
    assert_eq!(
        detail(
            json!({ "command": format!("diff {WORKTREE}/a.rs {WORKTREE}/b.rs && ls {WORKTREE}") }),
            Some(WORKTREE)
        ),
        "diff a.rs b.rs && ls ."
    );
    assert_eq!(
        detail(
            json!({ "file_path": format!("{WORKTREE}/src/lib.rs") }),
            Some(WORKTREE)
        ),
        "src/lib.rs"
    );
    // Without a worktree, and with an empty one, the detail is left alone.
    let untouched = format!("cd {WORKTREE} && ls");
    assert_eq!(detail(json!({ "command": untouched }), None), untouched);
    assert_eq!(detail(json!({ "command": untouched }), Some("")), untouched);
}

/// The subagent label of the `Agent` and `Task` tools.
#[test]
fn an_agent_or_task_call_is_labelled_by_description_then_type() {
    let label = |name: &str, input: Value| tool_call(input, name).subagent_label;

    assert_eq!(
        label(
            "Task",
            json!({ "description": " Find the bug ", "subagent_type": "Explore" })
        )
        .as_deref(),
        Some("Find the bug")
    );
    assert_eq!(
        label(
            "Agent",
            json!({ "description": "  ", "subagent_type": "Explore" })
        )
        .as_deref(),
        Some("Explore")
    );
    assert_eq!(
        label("Agent", json!({ "prompt": "Go." })).as_deref(),
        Some("Subagent")
    );
    assert_eq!(label("Agent", json!([])).as_deref(), Some("Subagent"));
    assert_eq!(label("Agent", json!("not an object")), None);
    assert_eq!(label("agent", json!({ "description": "Lowercase" })), None);
    assert_eq!(label("Bash", json!({ "description": "List files" })), None);
}

/// The subagent label of a `spawn_agent` tool, however its name is spelled.
#[test]
fn a_spawn_agent_call_is_labelled_by_path_then_task_then_nickname() {
    let label = |name: &str, input: Value| tool_call(input, name).subagent_label;

    // The name: `spawn_agent` or `spawnagent`, any case, whole or after `_`, `.`, `:`.
    for name in [
        "spawn_agent",
        "spawnAgent",
        "SPAWN_AGENT",
        "collab__spawnAgent",
        "collab.spawn_agent",
        "mcp:spawnagent",
    ] {
        assert_eq!(
            label(name, json!({})).as_deref(),
            Some("Subagent"),
            "{name}"
        );
    }
    for name in [
        "respawn_agent",
        "spawn_agents",
        "spawn__agent",
        "collab-spawn_agent",
    ] {
        assert_eq!(label(name, json!({ "task_name": "x" })), None, "{name}");
    }

    // The last non-empty segment of the path wins over the task name and the nickname.
    assert_eq!(
        label(
            "spawn_agent",
            json!({ "agent_path": "/root/fix-flaky__tests/", "task_name": "other", "agent_nickname": "Ada" })
        )
        .as_deref(),
        Some("Fix flaky tests")
    );
    // A path with no segment falls back to the task name, then to the nickname.
    assert_eq!(
        label(
            "spawn_agent",
            json!({ "agent_path": "//", "task_name": "write_docs", "agent_nickname": "Ada" })
        )
        .as_deref(),
        Some("Write docs")
    );
    assert_eq!(
        label("spawn_agent", json!({ "agent_nickname": "ada" })).as_deref(),
        Some("Ada")
    );
    // Nothing but separators leaves no words.
    assert_eq!(
        label("spawn_agent", json!({ "task_name": "-_-" })).as_deref(),
        Some("Subagent")
    );
    // Uppercasing follows JavaScript: one UTF-16 unit, so a full mapping applies to a
    // character of the basic plane and a character outside it is left alone.
    assert_eq!(
        label("spawn_agent", json!({ "task_name": "ßtrasse" })).as_deref(),
        Some("SStrasse")
    );
    assert_eq!(
        label("spawn_agent", json!({ "task_name": "𐐨_task" })).as_deref(),
        Some("𐐨 task")
    );
}

/// B4: a result's entry, field by field, on the wire.
#[test]
fn a_tool_result_serialises_to_the_wire_shape() {
    let success = only(parse(&result_frame("toolu_1", json!("done\n"), false)));
    assert_eq!(
        serde_json::to_value(&success).unwrap(),
        json!({
            "id": "message-1:0",
            "rowid": 1,
            "role": "tool",
            "text": "",
            "toolUseId": "toolu_1",
            "output": "done",
            "ts": STAMP,
            "queued": false
        })
    );

    let content = json!([
        { "type": "image", "source": { "type": "base64", "data": "iVBORw0KGgo=" } },
        { "type": "text", "text": "the page did not load" }
    ]);
    let failure = only(parse(&result_frame("toolu_1", content, true)));
    assert_eq!(
        serde_json::to_value(&failure).unwrap(),
        json!({
            "id": "message-1:0",
            "rowid": 1,
            "role": "tool",
            "text": "the page did not load",
            "toolUseId": "toolu_1",
            "output": "the page did not load",
            "images": ["1.0"],
            "error": true,
            "ts": STAMP,
            "queued": false
        })
    );

    let edit = only(parse(&result_frame(
        "toolu_1",
        json!({ "diffString": "@@ -1 +1 @@\n-a\n+b" }),
        false,
    )));
    assert_eq!(
        serde_json::to_value(&edit).unwrap(),
        json!({
            "id": "message-1:0",
            "rowid": 1,
            "role": "tool",
            "text": "",
            "toolUseId": "toolu_1",
            "output": "@@ -1 +1 @@\n-a\n+b",
            "diff": true,
            "ts": STAMP,
            "queued": false
        })
    );
}

/// B4: a failure with nothing to say is still a row; a success with nothing to say is not.
#[test]
fn an_empty_failed_result_says_tool_error() {
    for content in [json!(""), json!(null), json!(12), json!([]), json!(true)] {
        let failed = only(parse(&result_frame("toolu_1", content.clone(), true)));
        assert_eq!(failed.text, "(tool error)");
        assert_eq!(failed.output.as_deref(), Some("(tool error)"));
        assert!(failed.error);

        assert_eq!(parse(&result_frame("toolu_1", content, false)), vec![]);
    }

    // No `content` key at all.
    let missing = json!({
        "type": "user",
        "message": { "content": [{ "type": "tool_result", "tool_use_id": "toolu_1" }] }
    });
    assert_eq!(parse_frame(&missing), vec![]);
}

/// B4: `is_error` is read as JavaScript reads a condition, not as a strict boolean.
#[test]
fn is_error_is_read_by_truthiness() {
    let error_of = |is_error: Value| {
        only(parse_frame(&json!({
            "type": "user",
            "message": { "content": [{
                "type": "tool_result", "tool_use_id": "toolu_1", "content": "out", "is_error": is_error
            }] }
        })))
        .error
    };

    for truthy in [json!(true), json!(1), json!("yes"), json!({}), json!([])] {
        assert!(error_of(truthy.clone()), "{truthy}");
    }
    for falsy in [json!(false), json!(0), json!(0.0), json!(""), json!(null)] {
        assert!(!error_of(falsy.clone()), "{falsy}");
    }
}

/// B4: the blocks of a result are joined with nothing between them, then the tools named.
#[test]
fn result_blocks_are_joined_without_a_separator() {
    let output = output_of(json!([
        "a bare string is skipped",
        null,
        7,
        ["a nested list is read as an object with no fields"],
        { "type": "text", "text": "one" },
        { "type": "text", "text": "two\n" },
        { "type": "text", "text": 3 },
        { "type": "tool_reference", "tool_name": " navigate " },
        { "type": "text", "text": "named instead", "tool_name": "wins_over_text" },
        { "type": "other", "text": "three" }
    ]));
    assert_eq!(
        output.as_deref(),
        Some("onetwo\nthree2 tools: navigate, wins_over_text")
    );

    let single = output_of(json!([{ "type": "tool_reference", "tool_name": "navigate" }]));
    assert_eq!(single.as_deref(), Some("1 tool: navigate"));
}

/// B4: error tags are removed anywhere in the text, in one pass, before trimming.
#[test]
fn error_tags_are_removed_from_a_result() {
    assert_eq!(
        output_of(json!(
            "  <tool_use_error>first</tool_use_error> and <tool_use_error>second</tool_use_error>  "
        ))
        .as_deref(),
        Some("first and second")
    );
    // A tag that only appears once another is removed stays, as one pass leaves it.
    assert_eq!(
        output_of(json!("<tool_use<tool_use_error>_error>kept")).as_deref(),
        Some("<tool_use_error>kept")
    );
    assert_eq!(
        output_of(json!("a < b <tool_use_error> c")).as_deref(),
        Some("a < b  c")
    );
}

/// B4: the edit result's status is optional, and both fields are trimmed.
#[test]
fn an_edit_result_without_a_status_is_the_diff_alone() {
    let entry = only(parse(&result_frame(
        "toolu_1",
        json!({ "status": "  ", "diffString": "\n@@ -1 +1 @@\n-a\n+b\n" }),
        false,
    )));
    assert!(entry.diff);
    assert_eq!(entry.output.as_deref(), Some("@@ -1 +1 @@\n-a\n+b"));

    // A blank diff is not an edit result: the object falls back to its JSON.
    let blank = only(parse(&raw_result_frame(
        r#"{"status":"ok","diffString":" "}"#,
    )));
    assert!(!blank.diff);
    assert_eq!(
        blank.output.as_deref(),
        Some(r#"{"status":"ok","diffString":" "}"#)
    );
}

/// B5: a block of an unknown kind is ignored and does not end the prose around it.
#[test]
fn unknown_blocks_are_ignored_and_do_not_split_prose() {
    let entries = parse_frame(&assistant_frame(json!([
        { "type": "text", "text": "Before." },
        { "type": "server_tool_use", "name": "web_search" },
        { "type": "tool_use", "input": { "command": "no name, so not a call" } },
        { "type": "tool_use", "name": 7 },
        { "type": "text", "text": { "not": "a string" } },
        { "no_type": true },
        "a string",
        null,
        42,
        { "type": "text", "text": "After." }
    ])));

    let entry = only(entries);
    assert_eq!(entry.role, TranscriptRole::Assistant);
    assert_eq!(entry.text, "Before.\nAfter.");
}

/// The order of prose across mixed blocks: each thinking, call and result block first ends
/// the prose collected before it, and prose left at the end closes the row.
#[test]
fn prose_keeps_its_place_across_mixed_blocks() {
    let entries = parse_frame(&assistant_frame(json!([
        { "type": "text", "text": "I will look first." },
        { "type": "text", "text": "Then edit." },
        { "type": "tool_use", "id": "toolu_1", "name": "Read", "input": { "file_path": "src/a.rs" } },
        { "type": "text", "text": "Read it." },
        { "type": "thinking", "thinking": "The bug is in the loop." },
        { "type": "text", "text": "   " },
        { "type": "tool_result", "tool_use_id": "toolu_1", "content": "fn main() {}" },
        { "type": "text", "text": "Done." }
    ])));

    assert_eq!(
        roles_and_texts(&entries),
        [
            (TranscriptRole::Assistant, "I will look first.\nThen edit."),
            (TranscriptRole::Tool, "Read"),
            (TranscriptRole::Assistant, "Read it."),
            (TranscriptRole::Thinking, "The bug is in the loop."),
            (TranscriptRole::Tool, ""),
            (TranscriptRole::Assistant, "Done."),
        ]
    );
    let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "message-1:0",
            "message-1:1",
            "message-1:2",
            "message-1:3",
            "message-1:4",
            "message-1:5"
        ]
    );
}

/// An entry's id counts the entries the row has produced, not the blocks read.
#[test]
fn entry_ids_count_entries_not_blocks() {
    let entries = parse_frame(&json!({
        "type": "user",
        "message": { "content": [
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "" },
            { "type": "text", "text": "ignored" },
            { "type": "tool_result", "tool_use_id": "toolu_2", "content": "second" },
            { "type": "tool_result", "tool_use_id": "toolu_3", "content": "third" }
        ] }
    }));

    let ids: Vec<(&str, Option<&str>)> = entries
        .iter()
        .map(|e| (e.id.as_str(), e.tool_use_id.as_deref()))
        .collect();
    assert_eq!(
        ids,
        [
            ("message-1:0", Some("toolu_2")),
            ("message-1:1", Some("toolu_3"))
        ]
    );
}

/// Image numbers run on across results that carry several images or none.
#[test]
fn image_numbers_continue_across_the_results_of_a_row() {
    let image =
        || json!({ "type": "image", "source": { "type": "base64", "data": "iVBORw0KGgo=" } });
    let entries = parse_message(
        &row(
            &json!({
                "type": "user",
                "message": { "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": [image(), image()] },
                    { "type": "tool_result", "tool_use_id": "toolu_2", "content": "text only" },
                    { "type": "tool_result", "tool_use_id": "toolu_3", "content": [image()] }
                ] }
            })
            .to_string(),
            31,
            "message-31",
        ),
        None,
    );

    let images: Vec<&[String]> = entries.iter().map(|e| e.images.as_slice()).collect();
    assert_eq!(images, [&["31.0", "31.1"][..], &[][..], &["31.2"][..]]);
}

// ---------------------------------------------------------------------------
// JavaScript semantics: UTF-16 lengths, trimming, JSON text
// ---------------------------------------------------------------------------

/// The 2000-unit limit of an output counts UTF-16 code units: not bytes, not characters.
#[test]
fn the_output_limit_counts_utf16_code_units() {
    // 1000 characters, 2000 units, 4000 bytes: exactly at the limit, so whole.
    let at_limit = "😀".repeat(1000);
    assert_eq!(
        output_of(json!(at_limit)).as_deref(),
        Some(at_limit.as_str())
    );

    // 1500 characters, 3000 units: cut after 2000 units, which is 1000 characters.
    let over = output_of(json!("😀".repeat(1500))).unwrap();
    assert_eq!(over, format!("{}…", "😀".repeat(1000)));
    assert_eq!(utf16_len(&over), 2001);
    assert_eq!(over.chars().count(), 1001);
    assert_eq!(over.len(), 4003);

    // 2000 two-byte characters are 2000 units and 4000 bytes: whole.
    let accented = "é".repeat(2000);
    assert_eq!(
        output_of(json!(accented)).as_deref(),
        Some(accented.as_str())
    );
    assert_eq!(
        output_of(json!("é".repeat(2001))).unwrap(),
        format!("{}…", "é".repeat(2000))
    );
}

/// The 200-unit limit of a raw row counts the same way.
#[test]
fn the_raw_limit_counts_utf16_code_units() {
    // `{` and 199 two-byte characters: 200 units, 399 bytes. Whole.
    let at_limit = format!("{{{}", "é".repeat(199));
    assert_eq!(only(parse(&at_limit)).text, at_limit);

    // `{x` and 100 characters outside the basic plane: 202 units but only 102 characters.
    let over = format!("{{x{}", "😀".repeat(100));
    let text = only(parse(&over)).text;
    assert_eq!(text, format!("{{x{}…", "😀".repeat(99)));
    assert_eq!(utf16_len(&text), 201);

    // An error frame's wording is cut the same way.
    let wording = only(parse_frame(
        &json!({ "type": "error", "content": "😀".repeat(101) }),
    ))
    .text;
    assert_eq!(wording, format!("{}…", "😀".repeat(100)));
}

/// A cut that falls inside one character keeps 200 units: U+FFFD stands for the half that
/// JavaScript would leave behind, since a Rust string cannot hold it.
#[test]
fn a_cut_inside_a_character_leaves_a_replacement_character() {
    // `{` is unit 1; each character after it takes two, so unit 200 is the first half of one.
    let content = format!("{{{}", "😀".repeat(150));
    let text = only(parse(&content)).text;

    assert_eq!(text, format!("{{{}\u{fffd}…", "😀".repeat(99)));
    assert_eq!(utf16_len(&text), 201);
}

/// Trimming uses JavaScript's whitespace: U+FEFF and U+00A0 are trimmed, U+0085 is not.
#[test]
fn trimming_uses_javascript_whitespace() {
    assert_eq!(
        output_of(json!("\u{feff}\u{a0}\u{2028} value \u{3000}\u{feff}")).as_deref(),
        Some("value")
    );
    assert_eq!(
        output_of(json!("\u{85}value\u{85}")).as_deref(),
        Some("\u{85}value\u{85}")
    );
    // A next-line character alone is therefore something to show, and a prompt.
    assert_eq!(output_of(json!("\u{85}")).as_deref(), Some("\u{85}"));
    assert_eq!(only(parse("\u{85}")).role, TranscriptRole::User);
    assert_eq!(parse("\u{feff}"), vec![]);
}

/// The JSON of an unknown result writes numbers as JavaScript does.
///
/// Every case here is a literal the JSON reader turns into the nearest double. A literal
/// with more digits than a 64-bit integer holds is not among them: the reader may land one
/// step away from the nearest double there, and the text then differs in its last digit.
#[test]
fn the_json_fallback_writes_numbers_the_javascript_way() {
    let cases = [
        ("1", "1"),
        ("1.0", "1"),
        ("-1.50", "-1.5"),
        ("0", "0"),
        ("-0", "0"),
        ("-0.0", "0"),
        ("0.1", "0.1"),
        ("100", "100"),
        ("1e2", "100"),
        ("1E+2", "100"),
        ("123.456", "123.456"),
        ("0.000001", "0.000001"),
        ("0.0000001", "1e-7"),
        ("1.5e-7", "1.5e-7"),
        ("1e20", "100000000000000000000"),
        ("1e21", "1e+21"),
        ("1.5e21", "1.5e+21"),
        ("1000000000000000000000", "1e+21"),
        ("9007199254740993", "9007199254740992"),
        ("12345678901234567890", "12345678901234567000"),
        ("-9223372036854775808", "-9223372036854776000"),
        ("18446744073709551615", "18446744073709552000"),
        ("1.7976931348623157e308", "1.7976931348623157e+308"),
        ("5e-324", "5e-324"),
        ("2.5e-10", "2.5e-10"),
        ("0.30000000000000004", "0.30000000000000004"),
        ("4.35", "4.35"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            only(parse(&raw_result_frame(&format!(r#"{{"n":{input}}}"#))))
                .output
                .unwrap(),
            format!(r#"{{"n":{expected}}}"#),
            "{input}"
        );
    }
}

/// The JSON of an unknown result keeps the input's key order, nested values and escapes,
/// with no spaces.
#[test]
fn the_json_fallback_keeps_key_order_and_escapes() {
    let content = r#"{ "zeta": [1, 2.50, null, true, false, []], "alpha": { "b": "q\"\\\/\n\t\u0001é ", "a": {} }, "mid": "x" }"#;
    assert_eq!(
        only(parse(&raw_result_frame(content))).output.unwrap(),
        "{\"zeta\":[1,2.5,null,true,false,[]],\"alpha\":{\"b\":\"q\\\"\\\\/\\n\\t\\u0001é\u{2028}\",\"a\":{}},\"mid\":\"x\"}"
    );
}

/// JavaScript lists the keys that are array indices first, in ascending order; the JSON of
/// an unknown result does the same.
#[test]
fn the_json_fallback_puts_array_index_keys_first() {
    let content = r#"{"b":1,"10":"ten","2":"two","a":2,"01":"not an index","4294967295":"too big","0":"zero","-1":"negative"}"#;
    assert_eq!(
        only(parse(&raw_result_frame(content))).output.unwrap(),
        r#"{"0":"zero","2":"two","10":"ten","b":1,"a":2,"01":"not an index","4294967295":"too big","-1":"negative"}"#
    );
}

// ---------------------------------------------------------------------------
// Queue (outbox) rows
// ---------------------------------------------------------------------------

fn outbox(payload: Option<&str>) -> StoredOutboxMessage {
    StoredOutboxMessage {
        message_id: "queued-1".to_owned(),
        delivery_payload: payload.map(str::to_owned),
        created_at: Some("2026-09-03T12:01:01.000Z".to_owned()),
    }
}

#[test]
fn an_outbox_row_becomes_a_queued_user_entry() {
    let entry = parse_outbox_message(&outbox(Some(
        r#"{"message":"  first\n","model":"any","attachments":[]}"#,
    )))
    .unwrap();

    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({
            "id": "queued-1",
            "rowid": 0,
            "role": "user",
            "text": "first",
            "ts": "2026-09-03T12:01:01.000Z",
            "queued": true
        })
    );
}

/// An outbox prompt is a prompt even when it starts with a brace: it is never read as a frame.
#[test]
fn an_outbox_prompt_that_looks_like_a_frame_is_still_a_prompt() {
    let entry = parse_outbox_message(&outbox(Some(r#"{"message":"{\"type\":\"system\"}"}"#)));
    assert_eq!(entry.unwrap().text, r#"{"type":"system"}"#);
}

#[test]
fn an_outbox_row_that_cannot_be_rendered_is_nothing() {
    for payload in [
        None,
        Some(""),
        Some("not json"),
        Some(r#"{"message":"#),
        Some("{}"),
        Some(r#"{"message":"   "}"#),
        Some(r#"{"message":42}"#),
        Some(r#"{"message":null}"#),
        Some(r#"{"message":{"text":"nested"}}"#),
        Some(r#"["message"]"#),
        Some(r#""message""#),
        Some("null"),
    ] {
        assert_eq!(parse_outbox_message(&outbox(payload)), None, "{payload:?}");
    }
}

// ---------------------------------------------------------------------------
// The golden chat shared with the phone app's contract tests
// ---------------------------------------------------------------------------

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../web/tests/contract/fixtures/transcript-entries.json"
);
const GOLDEN_WORKTREE: &str = "/Users/example/conductor/workspaces/atlas/lisbon";

/// One invented chat, row by row, that produces every kind of entry.
fn golden_rows() -> Vec<StoredMessage> {
    let contents: Vec<String> = vec![
        // 1: the user's prompt.
        "Rename the `slug` helper to `slugify` and make sure the tests still pass.".to_owned(),
        // 2: bookkeeping, not shown.
        json!({ "type": "system", "subtype": "init", "model": "example-model", "tools": ["Bash", "Edit"] }).to_string(),
        // 3: thinking, prose and a described command in one frame.
        assistant_frame(json!([
            { "type": "thinking", "thinking": "The helper is probably used in more than one module. Search before editing.", "signature": "c2lnbmF0dXJl" },
            { "type": "text", "text": "I'll find every use of `slug` first." },
            {
                "type": "tool_use",
                "id": "toolu_search",
                "name": "Bash",
                "input": {
                    "command": format!("cd {GOLDEN_WORKTREE} && rg -n \"slug\\(\" src"),
                    "description": "Find uses of slug"
                }
            }
        ]))
        .to_string(),
        // 4: a plain-string result.
        result_frame(
            "toolu_search",
            json!("src/text.rs:14:pub fn slug(input: &str) -> String {\nsrc/routes.rs:52:    let id = slug(&title);\n"),
            false,
        ),
        // 5: a call with no description: the tool name is the title.
        assistant_frame(json!([
            { "type": "text", "text": "Two places. Renaming the definition and the call site." },
            {
                "type": "tool_use",
                "id": "toolu_edit",
                "name": "Edit",
                "input": {
                    "file_path": format!("{GOLDEN_WORKTREE}/src/text.rs"),
                    "old_string": "pub fn slug(",
                    "new_string": "pub fn slugify("
                }
            }
        ]))
        .to_string(),
        // 6: an edit result, shown as a diff under its status line.
        result_frame(
            "toolu_edit",
            json!({
                "status": format!("update {GOLDEN_WORKTREE}/src/text.rs"),
                "diffString": "@@ -14,1 +14,1 @@\n-pub fn slug(input: &str) -> String {\n+pub fn slugify(input: &str) -> String {\n"
            }),
            false,
        ),
        // 7: a call that spawns a subagent.
        assistant_frame(json!([{
            "type": "tool_use",
            "id": "toolu_agent",
            "name": "Agent",
            "input": {
                "description": "Check the docs for slug",
                "subagent_type": "Explore",
                "prompt": "Look through docs/ for mentions of the slug helper and report them."
            }
        }]))
        .to_string(),
        // 8: the subagent's own frame, pointing back at the call that spawned it.
        json!({
            "type": "assistant",
            "parent_tool_use_id": "toolu_agent",
            "message": { "role": "assistant", "content": [
                { "type": "text", "text": "Reading the helper guide." },
                { "type": "tool_use", "id": "toolu_read", "name": "Read", "input": { "file_path": format!("{GOLDEN_WORKTREE}/docs/helpers.md") } }
            ] }
        })
        .to_string(),
        // 9: the subagent's failed result.
        json!({
            "type": "user",
            "parent_tool_use_id": "toolu_agent",
            "message": { "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_read",
                "content": "<tool_use_error>File does not exist.</tool_use_error>",
                "is_error": true
            }] }
        })
        .to_string(),
        // 10: a call with no primary input.
        assistant_frame(json!([{
            "type": "tool_use",
            "id": "toolu_shot",
            "name": "mcp__browser__screenshot",
            "input": { "full_page": true }
        }]))
        .to_string(),
        // 11: a result with text and an image; the bytes stay behind.
        result_frame(
            "toolu_shot",
            json!([
                { "type": "text", "text": "Captured the settings page." },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgoAAAANSUhEUg==" } }
            ]),
            false,
        ),
        // 12: a result in a shape nobody planned for, shown as its own JSON.
        raw_result_frame(r#"{"matches":3,"ratio":0.50,"kind":"summary"}"#),
        // 13: bookkeeping, not shown.
        json!({ "type": "result", "subtype": "success", "result": "Renamed.", "num_turns": 6 }).to_string(),
        // 14: how a stopped turn ends.
        json!({ "type": "error", "content": "aborted by user" }).to_string(),
        // 15: a row cut short while it was written.
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"The rename is".to_owned(),
        // 16: a prompt queued the old way, in the row itself.
        "Also update the changelog.".to_owned(),
    ];

    contents
        .into_iter()
        .zip(1_i64..)
        .map(|(content, rowid)| StoredMessage {
            rowid,
            id: format!("msg-{rowid:02}"),
            content: Some(content),
            created_at: Some(format!("2026-09-14T09:30:{rowid:02}.000Z")),
            sent_at: (rowid != 16).then(|| format!("2026-09-14T09:30:{rowid:02}.000Z")),
            queue_order: (rowid == 16).then_some(1),
        })
        .collect()
}

/// The prompt still waiting in the queue table of the same chat.
fn golden_outbox_row() -> StoredOutboxMessage {
    StoredOutboxMessage {
        message_id: "msg-17".to_owned(),
        delivery_payload: Some(
            json!({ "message": "Then open a pull request.", "attachments": [] }).to_string(),
        ),
        created_at: Some("2026-09-14T09:31:05.000Z".to_owned()),
    }
}

fn golden_entries() -> Vec<TranscriptEntry> {
    let mut entries: Vec<TranscriptEntry> = golden_rows()
        .iter()
        .flat_map(|stored| parse_message(stored, Some(GOLDEN_WORKTREE)))
        .collect();
    entries.extend(parse_outbox_message(&golden_outbox_row()));
    entries
}

#[test]
fn the_golden_chat_matches_the_fixture_shared_with_the_phone_app() {
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(GOLDEN).expect("the golden fixture is in the repository"),
    )
    .expect("the golden fixture is JSON");

    assert_eq!(serde_json::to_value(golden_entries()).unwrap(), fixture);
}

/// The fixture is only a contract if the chat behind it shows every kind of entry.
#[test]
fn the_golden_chat_exercises_every_entry_kind() {
    let entries = golden_entries();
    let any = |test: &dyn Fn(&TranscriptEntry) -> bool| entries.iter().any(test);

    for role in [
        TranscriptRole::User,
        TranscriptRole::Assistant,
        TranscriptRole::Tool,
        TranscriptRole::Thinking,
        TranscriptRole::System,
    ] {
        assert!(any(&|e| e.role == role), "{role:?}");
    }
    assert!(any(&|e| e.tool.is_some() && e.detail.is_some()));
    assert!(any(&|e| e.tool.is_some() && e.detail.is_none()));
    assert!(any(&|e| e.subagent_label.is_some()));
    assert!(any(&|e| e.parent_tool_use_id.is_some()));
    assert!(any(&|e| e.tool.is_none() && e.output.is_some() && !e.error));
    assert!(any(&|e| e.diff));
    assert!(any(&|e| !e.images.is_empty()));
    assert!(any(&|e| e.error));
    assert!(any(&|e| e.queued && e.rowid > 0));
    assert!(any(&|e| e.queued && e.rowid == 0));
}

#[test]
fn a_frame_with_a_lone_surrogate_escape_is_still_read() {
    let output = |escaped: &str| {
        only(parse(&raw_result_frame(&format!("\"{escaped}\""))))
            .output
            .unwrap()
    };
    assert_eq!(output(r"cut \ud83d"), "cut \u{FFFD}");
    assert_eq!(output(r"cut \uD83D"), "cut \u{FFFD}");
    assert_eq!(output(r"\ude00 tail"), "\u{FFFD} tail");
    assert_eq!(output(r"\ud83d x \ud83d\ude00"), "\u{FFFD} x \u{1F600}");
    assert_eq!(output(r"ok \ud83d\ude00"), "ok \u{1F600}");
    assert_eq!(output(r"ok \uD83D\uDE00"), "ok \u{1F600}");
    assert_eq!(output(r"a \\ud83d"), r"a \ud83d");
    assert_eq!(output(r"a \\\ud83d"), "a \\\u{FFFD}");

    // Broken for another reason: still the raw dump.
    let broken = r#"{"type":"user","message":{"content":"\ud83d"#;
    let entry = only(parse(broken));
    assert_eq!(entry.role, TranscriptRole::System);
    assert_eq!(entry.text, broken);
}
