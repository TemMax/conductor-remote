use std::time::{Duration, Instant};

use conductor_remote::notify::watcher::{
    clip_exact, one_line, parked_message, turn_message, Due, TurnKind, TurnWatcher, Viewing,
};
use conductor_remote::reads::states::SessionStateRow;

const T1: &str = "2026-08-27T10:17:11.000Z";
const T2: &str = "2026-08-27T10:41:30.000Z";

fn chat_in(
    session_id: &str,
    status: Option<&str>,
    turn_started_at: Option<&str>,
    last_user_message_at: Option<&str>,
) -> SessionStateRow {
    SessionStateRow {
        session_id: session_id.into(),
        workspace_id: "ws-1".into(),
        status: status.map(str::to_owned),
        turn_started_at: turn_started_at.map(str::to_owned),
        last_user_message_at: last_user_message_at.map(str::to_owned),
        workspace_title: "Build photo window".into(),
        repo_name: Some("auk".into()),
        session_title: Some("Manage Chat Context".into()),
    }
}

/// `chat-1` whose last user message is its turn head, as in the reference's `chat`.
fn chat(status: Option<&str>, turn_started_at: Option<&str>) -> SessionStateRow {
    chat_in("chat-1", status, turn_started_at, turn_started_at)
}

fn chat_with_user_message(
    status: &str,
    turn_started_at: &str,
    last_user_message_at: &str,
) -> SessionStateRow {
    chat_in(
        "chat-1",
        Some(status),
        Some(turn_started_at),
        Some(last_user_message_at),
    )
}

fn fired(watcher: &mut TurnWatcher, states: &[SessionStateRow]) -> Vec<String> {
    watcher
        .step(states)
        .iter()
        .map(|due| {
            let kind = match due.kind {
                TurnKind::Done => "done",
                TurnKind::Error => "error",
            };
            format!("{}:{kind}", due.state.session_id)
        })
        .collect()
}

fn none() -> Vec<String> {
    Vec::new()
}

fn done() -> Vec<String> {
    vec!["chat-1:done".to_owned()]
}

#[test]
fn uses_its_first_poll_as_a_baseline_and_never_announces_unchanged_idle_chats() {
    let mut watcher = TurnWatcher::new();
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), none());
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), none());
}

#[test]
fn announces_a_completed_turn_once_its_idle_state_is_confirmed() {
    let mut watcher = TurnWatcher::new();
    assert_eq!(
        fired(&mut watcher, &[chat(Some("working"), Some(T1))]),
        none()
    );
    assert_eq!(
        fired(&mut watcher, &[chat(Some("working"), Some(T1))]),
        none()
    );
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), none());
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), done());
}

#[test]
fn announces_confirmed_needs_plan_response_turns() {
    announces_confirmed_turns("needs_plan_response");
}

#[test]
fn announces_confirmed_needs_user_input_turns() {
    announces_confirmed_turns("needs_user_input");
}

fn announces_confirmed_turns(status: &str) {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    assert_eq!(fired(&mut watcher, &[chat(Some(status), Some(T1))]), none());
    assert_eq!(fired(&mut watcher, &[chat(Some(status), Some(T1))]), done());
}

#[test]
fn does_not_announce_an_idle_state_that_flickers_back_to_working() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    assert_eq!(
        fired(&mut watcher, &[chat(Some("working"), Some(T1))]),
        none()
    );
}

#[test]
fn announces_the_requested_turn_but_keeps_self_started_loop_laps_quiet() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), done());

    for _ in 0..5 {
        fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
        fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
        assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), none());
    }

    fired(&mut watcher, &[chat(Some("working"), Some(T2))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T2))]);
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T2))]), done());
}

#[test]
fn announces_a_human_steering_message_inside_a_running_loop() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat_with_user_message("idle", T1, T2)]);
    assert_eq!(
        fired(&mut watcher, &[chat_with_user_message("idle", T1, T2)]),
        done()
    );

    fired(&mut watcher, &[chat_with_user_message("working", T1, T2)]);
    fired(&mut watcher, &[chat_with_user_message("idle", T1, T2)]);
    assert_eq!(
        fired(&mut watcher, &[chat_with_user_message("idle", T1, T2)]),
        none()
    );
}

#[test]
fn always_announces_an_error_including_in_a_self_started_lap() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("error"), Some(T1))]);
    assert_eq!(
        fired(&mut watcher, &[chat(Some("error"), Some(T1))]),
        vec!["chat-1:error".to_owned()]
    );
}

#[test]
fn announces_every_completion_when_legacy_chats_have_no_turn_timestamp() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("idle"), None)]);
    for _ in 0..3 {
        fired(&mut watcher, &[chat(Some("working"), None)]);
        fired(&mut watcher, &[chat(Some("idle"), None)]);
        assert_eq!(fired(&mut watcher, &[chat(Some("idle"), None)]), done());
    }
}

#[test]
fn starts_from_a_new_baseline_after_reset() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    watcher.reset();
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), none());
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), done());
}

#[test]
fn drops_a_pending_notification_when_its_chat_is_archived() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    assert_eq!(fired(&mut watcher, &[]), none());
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), none());
}

#[test]
fn tracks_loop_suppression_independently_for_each_chat() {
    let mut watcher = TurnWatcher::new();
    let both = |status: &str, yours_turn: &str| {
        vec![
            chat_in("looper", Some(status), Some(T1), Some(T1)),
            chat_in("yours", Some(status), Some(yours_turn), Some(yours_turn)),
        ]
    };
    fired(&mut watcher, &both("working", T1));
    fired(&mut watcher, &both("idle", T1));
    fired(&mut watcher, &both("idle", T1));
    fired(&mut watcher, &both("working", T1));
    fired(&mut watcher, &both("idle", T1));
    let notifications = fired(&mut watcher, &both("idle", T2));
    assert!(!notifications.contains(&"looper:done".to_owned()));
    assert!(notifications.contains(&"yours:done".to_owned()));
}

// Further watcher rules the reference states in its code rather than in its tests.

#[test]
fn a_chat_first_seen_on_a_step_is_not_news() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat_in("other", Some("idle"), None, None)]);
    // `chat-1` appears already finished, and one that appears in error is no news either.
    let states = [
        chat_in("other", Some("idle"), None, None),
        chat(Some("idle"), Some(T1)),
        chat_in("broken", Some("error"), None, None),
    ];
    assert_eq!(fired(&mut watcher, &states), none());
    assert_eq!(fired(&mut watcher, &states), none());
}

#[test]
fn an_error_that_flaps_back_is_not_announced_and_an_error_after_an_error_is_not_new() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("error"), Some(T1))]);
    assert_eq!(
        fired(&mut watcher, &[chat(Some("working"), Some(T1))]),
        none()
    );

    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("error"), Some(T1))]);
    assert_eq!(
        fired(&mut watcher, &[chat(Some("error"), Some(T1))]),
        none()
    );
    assert_eq!(
        fired(&mut watcher, &[chat(Some("error"), Some(T1))]),
        none()
    );
}

#[test]
fn a_vanished_chat_forgets_its_remembered_turn() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), done());
    // The chat disappears for a step, then comes back and ends a turn on the same head.
    fired(&mut watcher, &[]);
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), done());
}

#[test]
fn a_status_less_chat_that_starts_working_is_a_known_chat() {
    let mut watcher = TurnWatcher::new();
    fired(&mut watcher, &[chat(None, Some(T1))]);
    fired(&mut watcher, &[chat(Some("working"), Some(T1))]);
    fired(&mut watcher, &[chat(Some("idle"), Some(T1))]);
    assert_eq!(fired(&mut watcher, &[chat(Some("idle"), Some(T1))]), done());
}

// The reading claims.

const FRESH: Duration = Duration::from_secs(10);

#[test]
fn covers_only_the_chat_that_device_has_on_screen() {
    let at = Instant::now();
    let mut viewing = Viewing::new(FRESH);
    viewing.note("device-a", "chat-1", at);
    assert!(viewing.is_reading("device-a", "chat-1", at));
    assert!(!viewing.is_reading("device-a", "chat-2", at));
}

#[test]
fn leaves_every_other_device_alone() {
    let at = Instant::now();
    let mut viewing = Viewing::new(FRESH);
    viewing.note("device-a", "chat-1", at);
    assert!(!viewing.is_reading("device-b", "chat-1", at));
}

#[test]
fn expires_once_the_poll_that_refreshes_it_has_stopped() {
    let at = Instant::now();
    let mut viewing = Viewing::new(FRESH);
    viewing.note("device-c", "chat-1", at);
    assert!(viewing.is_reading("device-c", "chat-1", at + Duration::from_secs(9)));
    assert!(!viewing.is_reading("device-c", "chat-1", at + Duration::from_secs(10)));
    assert!(!viewing.is_reading("device-c", "chat-1", at + Duration::from_secs(30)));
}

#[test]
fn moves_with_the_reader_rather_than_accumulating_chats() {
    let at = Instant::now();
    let mut viewing = Viewing::new(FRESH);
    viewing.note("device-d", "chat-1", at);
    viewing.note("device-d", "chat-2", at);
    assert!(!viewing.is_reading("device-d", "chat-1", at));
    assert!(viewing.is_reading("device-d", "chat-2", at));
}

#[test]
fn never_claims_a_chat_for_a_device_that_has_not_polled() {
    let viewing = Viewing::new(FRESH);
    assert!(!viewing.is_reading("device-never", "chat-1", Instant::now()));
}

#[test]
fn forgets_a_device_on_request() {
    let at = Instant::now();
    let mut viewing = Viewing::new(FRESH);
    viewing.note("device-a", "chat-1", at);
    viewing.note("device-b", "chat-1", at);
    viewing.forget("device-a");
    assert!(!viewing.is_reading("device-a", "chat-1", at));
    assert!(viewing.is_reading("device-b", "chat-1", at));
}

#[test]
fn prunes_stale_stamps_once_more_than_sixteen_are_kept() {
    let at = Instant::now();
    let mut viewing = Viewing::new(FRESH);
    for n in 0..17 {
        viewing.note(&format!("old-{n}"), "chat-1", at);
    }
    let later = at + Duration::from_secs(20);
    // Seventeen stamps are kept and all of them are stale, but only a `note` prunes.
    assert!(!viewing.is_reading("old-0", "chat-1", later));
    viewing.note("new", "chat-1", later);
    viewing.note("newer", "chat-1", later);
    assert!(viewing.is_reading("new", "chat-1", later));
    assert!(viewing.is_reading("newer", "chat-1", later));
    // The first `note` above found more than 16 kept and pruned the stale ones: the stamp is
    // gone, so it no longer reads as fresh even at the instant it was made.
    assert!(!viewing.is_reading("old-0", "chat-1", at));
}

#[test]
fn does_not_prune_fresh_stamps_even_past_sixteen() {
    let at = Instant::now();
    let mut viewing = Viewing::new(FRESH);
    for n in 0..20 {
        viewing.note(&format!("device-{n}"), "chat-1", at);
    }
    for n in 0..20 {
        assert!(viewing.is_reading(&format!("device-{n}"), "chat-1", at));
    }
}

// clip_exact and one_line.

#[test]
fn clip_exact_returns_text_that_fits_unchanged() {
    assert_eq!(clip_exact("hello world", 11), "hello world");
    assert_eq!(clip_exact("hello world", 20), "hello world");
    assert_eq!(clip_exact("", 5), "");
}

#[test]
fn clip_exact_edge_caps() {
    assert_eq!(clip_exact("hello", 0), "");
    assert_eq!(clip_exact("hello", 1), "…");
    assert_eq!(clip_exact("hello", 2), "h…");
}

#[test]
fn clip_exact_counts_chars_not_bytes() {
    let text = "ééééééééééé";
    assert_eq!(clip_exact(text, 5), "éééé…");
    assert_eq!(clip_exact(text, 5).chars().count(), 5);
    // Eleven chars fit a cap of eleven although they are twenty-two bytes.
    assert_eq!(clip_exact(text, 11), text);
    let kana = "日本語のテキストです";
    assert_eq!(clip_exact(kana, 4), "日本語…");
    // An emoji is one char and is never cut in half.
    assert_eq!(clip_exact("😀😀😀😀😀😀", 4), "😀😀😀…");
}

#[test]
fn clip_exact_backs_off_to_a_word_when_the_cut_is_inside_a_word() {
    // The cut lands inside "jumps"; the space before it is within the last 20 % of the cut.
    assert_eq!(
        clip_exact("the quick brown fox jumps", 22),
        "the quick brown fox…"
    );
}

#[test]
fn clip_exact_keeps_the_hard_cut_when_the_last_space_is_too_far_back() {
    assert_eq!(clip_exact("ab cdefghijklmnopqrstuvwxyz", 10), "ab cdefgh…");
}

#[test]
fn clip_exact_does_not_back_off_when_the_cut_falls_on_a_space() {
    assert_eq!(clip_exact("aaaa bbbb cccc", 10), "aaaa bbbb…");
}

#[test]
fn clip_exact_trims_the_space_before_the_ellipsis() {
    assert_eq!(clip_exact("aaaa   bbbbbbbbbbbbbb", 8), "aaaa…");
}

#[test]
fn one_line_turns_a_fenced_block_into_an_ellipsis() {
    assert_eq!(
        one_line(
            "Fixed it.\n\n```rust\nfn main() {}\n```\n\nAll   tests pass.",
            180
        ),
        "Fixed it. … All tests pass."
    );
    assert_eq!(one_line("a ```x``` b ```y``` c", 180), "a … b … c");
}

#[test]
fn one_line_leaves_an_unclosed_fence_alone() {
    assert_eq!(one_line("see ```code here", 180), "see ```code here");
}

#[test]
fn one_line_collapses_whitespace_and_trims() {
    assert_eq!(
        one_line("  first\tline\r\n\r\n second   line \n", 180),
        "first line second line"
    );
    assert_eq!(one_line("\n  \t ", 180), "");
}

#[test]
fn one_line_clips_after_collapsing() {
    assert_eq!(
        one_line("alpha\n\nbeta\n\ngamma\n\ndelta epsilon zeta", 22),
        "alpha beta gamma…"
    );
}

#[test]
fn one_line_handles_multi_byte_text() {
    assert_eq!(one_line("Привет,\n\nмир!  Это   тест", 12), "Привет, мир…");
    assert_eq!(one_line("日本語の\nテキストです", 7), "日本語の…");
}

// The notification texts.

fn due(kind: TurnKind, session_title: Option<&str>, repo: Option<&str>) -> Due {
    let mut state = chat_in("chat 1/é", Some("idle"), Some(T1), Some(T1));
    state.session_title = session_title.map(str::to_owned);
    state.repo_name = repo.map(str::to_owned);
    Due { state, kind }
}

#[test]
fn turn_message_title_has_the_workspace_chat_and_repo() {
    let full = turn_message(
        &due(TurnKind::Done, Some("Manage Chat Context"), Some("auk")),
        None,
        7,
    );
    assert_eq!(full.title, "Build photo window · Manage Chat Context — auk");
    let no_chat = turn_message(&due(TurnKind::Done, None, Some("auk")), None, 7);
    assert_eq!(no_chat.title, "Build photo window — auk");
    let no_repo = turn_message(
        &due(TurnKind::Done, Some("Manage Chat Context"), None),
        None,
        7,
    );
    assert_eq!(no_repo.title, "Build photo window · Manage Chat Context");
    let bare = turn_message(&due(TurnKind::Done, None, None), None, 7);
    assert_eq!(bare.title, "Build photo window");
}

#[test]
fn turn_message_done_without_text_says_it_finished() {
    let message = turn_message(&due(TurnKind::Done, None, None), None, 1_772_000_000_000);
    assert_eq!(message.body, "Finished its turn.");
    assert_eq!(message.kind, "done");
    assert_eq!(message.ts, 1_772_000_000_000);
    assert_eq!(message.tag, "chat 1/é");
    assert_eq!(message.url, "/w/ws-1?session=chat%201%2F%C3%A9");
    // An empty reply is no reply.
    assert_eq!(
        turn_message(&due(TurnKind::Done, None, None), Some(""), 1).body,
        "Finished its turn."
    );
}

#[test]
fn turn_message_done_with_text_is_the_one_line_of_it() {
    let said = "All done.\n\n```sh\ncargo test\n```\n\nEverything   passes.";
    let message = turn_message(&due(TurnKind::Done, None, None), Some(said), 1);
    assert_eq!(message.body, "All done. … Everything passes.");
    assert_eq!(message.kind, "done");

    let long = "word ".repeat(100);
    let message = turn_message(&due(TurnKind::Done, None, None), Some(&long), 1);
    assert_eq!(message.body.chars().count(), 180);
    assert!(message.body.ends_with("word…"));
}

#[test]
fn turn_message_error_without_text_says_the_agent_stopped() {
    let message = turn_message(&due(TurnKind::Error, None, None), None, 1);
    assert_eq!(message.body, "The agent stopped with an error.");
    assert_eq!(message.kind, "error");
}

#[test]
fn turn_message_error_with_text_prefixes_it() {
    let message = turn_message(
        &due(TurnKind::Error, None, None),
        Some("The build\nfailed."),
        1,
    );
    assert_eq!(message.body, "Stopped with an error. The build failed.");
    assert_eq!(message.kind, "error");
}

#[test]
fn parked_message_sent_after_unlock() {
    let message = parked_message(
        "Build photo window",
        "ws-1",
        "chat 1",
        "run the tests",
        None,
        99,
    );
    assert_eq!(message.title, "Build photo window");
    assert_eq!(message.body, "Sent after unlock: run the tests");
    assert_eq!(message.tag, "parked-chat 1");
    assert_eq!(message.url, "/w/ws-1?session=chat%201");
    assert_eq!(message.kind, "done");
    assert_eq!(message.ts, 99);
}

#[test]
fn parked_message_clips_a_long_prompt_to_140_chars() {
    let text = "ы".repeat(300);
    let message = parked_message("Conductor", "ws-1", "chat-1", &text, None, 1);
    let preview = message.body.strip_prefix("Sent after unlock: ").unwrap();
    assert_eq!(preview.chars().count(), 140);
    assert!(preview.ends_with('…'));
}

#[test]
fn parked_message_failed() {
    let message = parked_message(
        "Conductor",
        "ws-1",
        "chat-1",
        "run the tests",
        Some("Conductor is locked"),
        5,
    );
    assert_eq!(message.body, "Parked prompt failed: Conductor is locked");
    assert_eq!(message.tag, "parked-chat-1");
    assert_eq!(message.kind, "error");
}
