//! Process facts: the fold of task frames, the Run-task matcher, the agent listing and its cache.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use conductor_remote::reads::background::{open_background_tasks, timestamp_ms, TaskFrameRow};
use conductor_remote::reads::extras::commands::Commands;
use conductor_remote::reads::extras::processes::{agent_process, parse_etime, run_task_key};
use conductor_remote::reads::extras::Extras;
use conductor_remote::testing::FakeCommands;

const HOME: &str = "/Users/tester";
const CHAT: &str = "3c2353fc-ce46-4133-a8ad-0e8d395c11b6";
const OTHER_CHAT: &str = "0f111f3d-2e65-4f10-8d65-8decc5ef5d24";
const AGENT: &str = "/Users/tester/Library/Application Support/com.conductor.app/agent-binaries/claude/2.1.257/claude";
const NOW: i64 = 1_800_000_000_000;

// ---------------------------------------------------------------- the fold

fn row(created_at: &str, content: &str) -> TaskFrameRow {
    TaskFrameRow {
        created_at: created_at.to_owned(),
        content: content.to_owned(),
    }
}

fn started(task_id: &str, created_at: &str, description: &str) -> TaskFrameRow {
    row(
        created_at,
        &serde_json::json!({
            "type": "system",
            "subtype": "task_started",
            "session_id": "s",
            "task_id": task_id,
            "tool_use_id": format!("toolu_{task_id}"),
            "description": description,
            "task_type": "local_bash",
        })
        .to_string(),
    )
}

fn notified(task_id: &str, created_at: &str, status: &str) -> TaskFrameRow {
    row(
        created_at,
        &serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "status": status,
            "task_id": task_id,
            "tool_use_id": format!("toolu_{task_id}"),
        })
        .to_string(),
    )
}

fn ids(rows: &[TaskFrameRow], process_start: i64) -> Vec<String> {
    open_background_tasks(rows, process_start)
        .into_iter()
        .map(|task| task.task_id)
        .collect()
}

fn t0() -> i64 {
    timestamp_ms("2026-09-02T10:00:00.000Z").unwrap()
}

#[test]
fn a_started_task_without_notification_is_open_with_its_description_and_start() {
    let rows = [started(
        "a",
        "2026-09-02T10:48:47.856Z",
        "Wait until noon before the next poll",
    )];
    let tasks = open_background_tasks(&rows, t0());
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task.task_id, "a");
    assert_eq!(task.tool_use_id.as_deref(), Some("toolu_a"));
    assert_eq!(task.description, "Wait until noon before the next poll");
    assert_eq!(task.task_type, "local_bash");
    assert_eq!(task.since, "2026-09-02T10:48:47.856Z");
}

#[test]
fn a_notification_closes_its_own_task_and_no_other_whatever_its_status() {
    let rows = [
        started("a", "2026-09-02T10:01:00.000Z", "a"),
        started("b", "2026-09-02T10:02:00.000Z", "b"),
        started("c", "2026-09-02T10:03:00.000Z", "c"),
        notified("a", "2026-09-02T10:05:00.000Z", "completed"),
        notified("c", "2026-09-02T10:06:00.000Z", "failed"),
    ];
    assert_eq!(ids(&rows, t0()), ["b"]);
}

#[test]
fn a_task_started_before_its_process_is_dead_and_one_started_after_is_not() {
    let process = timestamp_ms("2026-09-02T10:05:00.000Z").unwrap();
    let rows = [
        started("old", "2026-09-02T10:01:00.000Z", "old"),
        started("new", "2026-09-02T10:06:00.000Z", "new"),
    ];
    assert_eq!(ids(&rows, process), ["new"]);
}

#[test]
fn an_abort_frame_is_not_a_close() {
    let rows = [
        started("a", "2026-09-02T10:01:00.000Z", "a"),
        row(
            "2026-09-02T10:02:00.000Z",
            r#"{"type":"error","content":"aborted by user"}"#,
        ),
    ];
    assert_eq!(ids(&rows, t0()), ["a"]);
}

#[test]
fn frames_without_a_task_id_other_frames_and_unparseable_rows_are_ignored() {
    let rows = [
        row(
            "2026-09-02T10:01:00.000Z",
            r#"{"type":"system","subtype":"background_tasks_changed"}"#,
        ),
        row(
            "2026-09-02T10:01:01.000Z",
            r#"{"type":"system","subtype":"task_started"}"#,
        ),
        row(
            "2026-09-02T10:01:01.500Z",
            r#"{"type":"system","subtype":"task_started","task_id":"  "}"#,
        ),
        row("2026-09-02T10:01:02.000Z", "not json"),
        row("2026-09-02T10:01:02.500Z", "null"),
        row(
            "2026-09-02T10:01:03.000Z",
            r#"{"type":"assistant","message":{"content":[]}}"#,
        ),
        row(
            "2026-09-02T10:01:04.000Z",
            r#"{"type":"user","subtype":"task_started","task_id":"x"}"#,
        ),
    ];
    assert!(open_background_tasks(&rows, t0()).is_empty());
}

#[test]
fn a_task_without_description_is_named_by_its_type_and_without_a_type_by_task() {
    let rows = [
        row(
            "2026-09-02T10:01:00.000Z",
            r#"{"type":"system","subtype":"task_started","task_id":"x","task_type":"local_agent"}"#,
        ),
        row(
            "2026-09-02T10:01:01.000Z",
            r#"{"type":"system","subtype":"task_started","task_id":"y","tool_use_id":"  toolu_y  "}"#,
        ),
    ];
    let tasks = open_background_tasks(&rows, 0);
    assert_eq!(tasks[0].description, "local_agent");
    assert_eq!(tasks[0].task_type, "local_agent");
    assert_eq!(tasks[0].tool_use_id, None);
    assert_eq!(tasks[1].description, "task");
    assert_eq!(tasks[1].task_type, "task");
    assert_eq!(tasks[1].tool_use_id.as_deref(), Some("toolu_y"));
}

#[test]
fn a_restarted_task_is_replaced_in_place_and_a_removed_one_goes_to_the_end() {
    let rows = [
        started("a", "2026-09-02T10:01:00.000Z", "first"),
        started("b", "2026-09-02T10:02:00.000Z", "second"),
        started("a", "2026-09-02T10:03:00.000Z", "again"),
    ];
    let tasks = open_background_tasks(&rows, t0());
    assert_eq!(
        tasks.iter().map(|t| t.task_id.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(tasks[0].description, "again");
    assert_eq!(tasks[0].since, "2026-09-02T10:03:00.000Z");

    let rows = [
        started("a", "2026-09-02T10:01:00.000Z", "a"),
        started("b", "2026-09-02T10:02:00.000Z", "b"),
        notified("a", "2026-09-02T10:03:00.000Z", "completed"),
        started("a", "2026-09-02T10:04:00.000Z", "a"),
    ];
    assert_eq!(ids(&rows, t0()), ["b", "a"]);
}

#[test]
fn a_task_whose_start_cannot_be_read_is_skipped() {
    let rows = [
        started("a", "yesterday", "a"),
        started("b", "2026-09-02T10:01:00", "no zone"),
    ];
    assert!(open_background_tasks(&rows, 0).is_empty());
}

#[test]
fn both_timestamp_forms_are_read() {
    // The stored form is UTC without a zone, with or without fractions.
    assert_eq!(timestamp_ms("1970-01-01 00:00:00"), Some(0));
    assert_eq!(timestamp_ms("2026-09-02 10:00:00"), Some(1_788_343_200_000));
    assert_eq!(
        timestamp_ms("2026-09-02 10:00:00.250"),
        Some(1_788_343_200_250)
    );
    assert_eq!(
        timestamp_ms("2026-09-02 10:00:00.5"),
        Some(1_788_343_200_500)
    );
    // The ISO form is read as written.
    assert_eq!(
        timestamp_ms("2026-09-02T10:00:00Z"),
        Some(1_788_343_200_000)
    );
    assert_eq!(
        timestamp_ms("2026-09-02T10:48:47.856Z"),
        Some(1_788_343_200_000 + 48 * 60_000 + 47_856)
    );
    assert_eq!(
        timestamp_ms("2026-09-02T12:00:00+02:00"),
        Some(1_788_343_200_000)
    );
    assert_eq!(
        timestamp_ms("2026-09-02T05:30:00-04:30"),
        Some(1_788_343_200_000)
    );
    assert_eq!(
        timestamp_ms("2026-09-02T12:00:00+0200"),
        Some(1_788_343_200_000)
    );
    // Leap days and dates before the epoch.
    assert_eq!(timestamp_ms("2024-02-29 00:00:00"), Some(1_709_164_800_000));
    assert_eq!(timestamp_ms("1969-12-31 23:59:59"), Some(-1_000));
    // Neither form.
    assert_eq!(timestamp_ms(""), None);
    assert_eq!(timestamp_ms("2026-09-02"), None);
    assert_eq!(timestamp_ms("2026-09-02T10:00:00"), None);
    assert_eq!(timestamp_ms("2026-09-02 10:00:00Z"), None);
    assert_eq!(timestamp_ms("2026-09-02 10:00:00+02:00"), None);
    assert_eq!(timestamp_ms("2026-13-02 10:00:00"), None);
    assert_eq!(timestamp_ms("2026-02-30 10:00:00"), None);
    assert_eq!(timestamp_ms("2026-09-02 25:00:00"), None);
    assert_eq!(timestamp_ms("2026-09-02 10:00:00."), None);
    assert_eq!(timestamp_ms("2026-09-02T10:00:00+2"), None);
    assert_eq!(timestamp_ms("2026-09-02 10:00:00 "), None);
}

// ------------------------------------------------------- Run-task matcher

fn key(line: &str) -> Option<String> {
    run_task_key(line, Path::new(HOME))
}

const WRAPPER: &str = "/Users/tester/.conductor/projects/--Users--tester--conductor--workspaces--repo--praia/run-run:12.sh";

#[test]
fn a_bare_zsh_names_a_run_task() {
    assert_eq!(
        key(&format!("zsh {WRAPPER}")).as_deref(),
        Some("--Users--tester--conductor--workspaces--repo--praia")
    );
}

#[test]
fn a_path_before_zsh_and_surrounding_white_space_are_accepted() {
    assert_eq!(
        key(&format!("  /bin/zsh   {WRAPPER}  \r")).as_deref(),
        Some("--Users--tester--conductor--workspaces--repo--praia")
    );
    assert_eq!(
        key(&format!("/opt/homebrew/bin/zsh\t{WRAPPER}")).as_deref(),
        Some("--Users--tester--conductor--workspaces--repo--praia")
    );
}

#[test]
fn trailing_arguments_are_accepted_after_white_space_only() {
    assert!(key(&format!("zsh {WRAPPER} --flag value")).is_some());
    assert_eq!(key(&format!("zsh {WRAPPER}x")), None);
    assert_eq!(key(&format!("zsh {WRAPPER}.bak")), None);
}

#[test]
fn another_shell_or_a_zsh_flag_in_front_is_not_a_run_task() {
    assert_eq!(key(&format!("bash {WRAPPER}")), None);
    assert_eq!(key(&format!("/bin/sh {WRAPPER}")), None);
    assert_eq!(key(&format!("fakezsh {WRAPPER}")), None);
    assert_eq!(key(&format!("zsh -c {WRAPPER}")), None);
    assert_eq!(key(&format!("zsh -c 'echo {WRAPPER}'")), None);
    assert_eq!(key("zsh"), None);
    assert_eq!(key(""), None);
}

#[test]
fn another_directory_script_or_home_is_not_a_run_task() {
    assert_eq!(
        key("zsh /Users/other/.conductor/projects/--a/run-run:1.sh"),
        None
    );
    assert_eq!(
        key("zsh /Users/tester/.conductor/projects/--a/run-setup:1.sh"),
        None
    );
    assert_eq!(
        key("zsh /Users/tester/.conductor/projects/--a/run-run:.sh"),
        None
    );
    assert_eq!(
        key("zsh /Users/tester/.conductor/projects/--a/run-run:1x.sh"),
        None
    );
    assert_eq!(
        key("zsh /Users/tester/.conductor/projects/--a/sub/run-run:1.sh"),
        None
    );
    assert_eq!(
        key("zsh /Users/tester/.conductor/projects//run-run:1.sh"),
        None
    );
    assert_eq!(
        key("zsh /Users/tester/.conductor/projects/run-run:1.sh"),
        None
    );
    // A key with a space is kept whole.
    assert_eq!(
        key("zsh /Users/tester/.conductor/projects/--my repo/run-run:3.sh").as_deref(),
        Some("--my repo")
    );
}

// ----------------------------------------------------------- agent listing

fn agent_line(etime: &str, flags: &str) -> String {
    format!("54874 {etime} {AGENT} --output-format stream-json --verbose {flags}")
}

#[test]
fn an_agent_whose_executable_path_holds_a_space_is_read() {
    let line = agent_line("02:05:23", &format!("--resume={CHAT} --disallowedTools x"));
    assert_eq!(
        agent_process(&line, NOW),
        Some((CHAT.to_owned(), NOW - (2 * 3_600 + 5 * 60 + 23) * 1_000))
    );
}

#[test]
fn a_space_separated_flag_a_session_id_flag_and_any_case_are_read() {
    assert_eq!(
        agent_process(&format!("1 05:00 /opt/claude --resume {CHAT}"), NOW),
        Some((CHAT.to_owned(), NOW - 300_000))
    );
    assert_eq!(
        agent_process(
            &format!("2 00:10 claude --session-id={OTHER_CHAT} --verbose"),
            NOW
        ),
        Some((OTHER_CHAT.to_owned(), NOW - 10_000))
    );
    assert_eq!(
        agent_process(&format!("3 00:10 claude --RESUME={CHAT}"), NOW),
        Some((CHAT.to_owned(), NOW - 10_000))
    );
    assert_eq!(
        agent_process(
            &format!(
                "4 00:10 claude --verbose --Session-Id  {}",
                CHAT.to_uppercase()
            ),
            NOW
        ),
        Some((CHAT.to_uppercase(), NOW - 10_000))
    );
}

#[test]
fn processes_that_are_not_a_chat_are_skipped() {
    let chrome = format!("19323 10:00:05 {AGENT} --chrome-native-host");
    let conductor = "5401 1-02:03:04 /Applications/Conductor.app/Contents/MacOS/conductor";
    let grep = format!("9 00:01 /usr/bin/grep --resume={CHAT}");
    let not_claude = format!("10 00:01 /usr/bin/claudette --resume={CHAT}");
    let short_id = "11 00:01 claude --resume=3c2353fc-ce46-4133";
    let long_id = format!("12 00:01 claude --resume={CHAT}0");
    let no_args = "13 00:01";
    let no_pid = format!("claude 00:01 claude --resume={CHAT}");
    for line in [
        chrome.as_str(),
        conductor,
        grep.as_str(),
        not_claude.as_str(),
        short_id,
        long_id.as_str(),
        no_args,
        no_pid.as_str(),
        "",
    ] {
        assert_eq!(agent_process(line, NOW), None, "{line}");
    }
}

#[test]
fn etime_has_three_shapes_and_an_unreadable_one_is_none() {
    assert_eq!(parse_etime("05:23"), Some(323));
    assert_eq!(parse_etime("00:00"), Some(0));
    assert_eq!(parse_etime("2:05:23"), Some(7_523));
    assert_eq!(parse_etime("02:05:23"), Some(7_523));
    assert_eq!(parse_etime("3-02:05:23"), Some(3 * 86_400 + 7_523));
    assert_eq!(parse_etime("  05:23  "), Some(323));
    for bad in [
        "",
        "-",
        "5",
        "1:2:3:4",
        "ab:cd",
        "05:",
        ":05",
        "1-05:23",
        "1-",
        "-1:00:00",
        "+5:00",
        "1.5:00",
        "99999999999999999999:00",
    ] {
        assert_eq!(parse_etime(bad), None, "{bad:?}");
    }
}

#[test]
fn an_unreadable_etime_gives_start_zero() {
    let line = format!("7 ??? claude --resume={CHAT}");
    assert_eq!(agent_process(&line, NOW), Some((CHAT.to_owned(), 0)));
}

// -------------------------------------------- the snapshots and their cache

/// A cache entry is stale after five seconds; a test waits this long between two refreshes.
const STALE: Duration = Duration::from_millis(5_200);

fn extras(fake: &Arc<FakeCommands>) -> Extras {
    let commands: Arc<dyn Commands> = fake.clone();
    Extras::new(commands, HOME)
}

fn run_listing() -> String {
    [
        "/sbin/launchd",
        &format!("zsh {WRAPPER}"),
        "zsh -c echo /Users/tester/.conductor/projects/--other/run-run:1.sh",
        "/bin/zsh /Users/tester/.conductor/projects/--Users--tester--wt--b/run-setup:1.sh",
    ]
    .join("\n")
}

#[test]
fn run_active_follows_the_listing_and_maps_a_worktree_to_its_key() {
    let fake = Arc::new(FakeCommands::new());
    fake.on("ps", &["-axww"], 0, &run_listing());
    let extras = extras(&fake);
    let processes = &extras.processes;

    // The first answer is empty; the listing lands in the background.
    assert!(!processes.run_active(Some("/Users/tester/conductor/workspaces/repo/praia")));
    extras.wait_idle();

    assert!(processes.run_active(Some("/Users/tester/conductor/workspaces/repo/praia")));
    assert!(!processes.run_active(Some("/Users/tester/conductor/workspaces/repo/other")));
    assert!(!processes.run_active(Some("/Users/tester/wt/b")));
    assert!(!processes.run_active(Some("")));
    assert!(!processes.run_active(None));
}

#[test]
fn agents_are_listed_with_the_latest_start_of_each_chat() {
    let fake = Arc::new(FakeCommands::new());
    let listing = [
        format!("100 10:00 claude --resume={CHAT}"),
        format!("101 01:00 claude --resume={CHAT}"),
        format!("102 00:30 {AGENT} --verbose --session-id {OTHER_CHAT}"),
        format!("103 05:00 {AGENT} --chrome-native-host"),
    ]
    .join("\n");
    fake.on("ps", &["-axo"], 0, &listing);
    let extras = extras(&fake);
    let processes = &extras.processes;

    assert_eq!(processes.agent_started_at(CHAT), None);
    extras.wait_idle();

    let chat = processes.agent_started_at(CHAT).unwrap();
    let other = processes.agent_started_at(OTHER_CHAT).unwrap();
    assert!((chat - (real_now_ms() - 60_000)).abs() < 5_000);
    assert!((other - (real_now_ms() - 30_000)).abs() < 5_000);
    assert_eq!(processes.agent_started_at("no-such-chat"), None);
}

fn real_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn an_unchanged_process_list_one_refresh_later_does_not_move_the_revision() {
    let fake = Arc::new(FakeCommands::new());
    let line = |etime: &str| format!("100 {etime} claude --resume={CHAT}");
    fake.on("ps", &["-axo"], 0, &line("10:00"));
    let extras = extras(&fake);
    let processes = &extras.processes;

    processes.agent_started_at(CHAT);
    extras.wait_idle();
    let start = processes.agent_started_at(CHAT).unwrap();
    let revision = extras.revision();

    // The process lives on: `ps` now says five seconds more, as it would after five seconds.
    std::thread::sleep(STALE);
    fake.on("ps", &["-axo"], 0, &line("10:05"));
    processes.agent_started_at(CHAT);
    extras.wait_idle();

    let ps_calls = fake
        .calls()
        .iter()
        .filter(|call| call.args.first().map(String::as_str) == Some("-axo"))
        .count();
    assert_eq!(ps_calls, 2, "the second listing must have run");
    assert_eq!(processes.agent_started_at(CHAT), Some(start));
    assert_eq!(extras.revision(), revision);
}

#[test]
fn a_restarted_process_moves_the_start_and_the_revision() {
    let fake = Arc::new(FakeCommands::new());
    fake.on(
        "ps",
        &["-axo"],
        0,
        &format!("100 10:00:00 claude --resume={CHAT}"),
    );
    let extras = extras(&fake);
    let processes = &extras.processes;

    processes.agent_started_at(CHAT);
    extras.wait_idle();
    let start = processes.agent_started_at(CHAT).unwrap();
    let revision = extras.revision();

    std::thread::sleep(STALE);
    fake.on(
        "ps",
        &["-axo"],
        0,
        &format!("200 00:05 claude --resume={CHAT}"),
    );
    processes.agent_started_at(CHAT);
    extras.wait_idle();

    let moved = processes.agent_started_at(CHAT).unwrap();
    assert!(moved > start + 3_600_000);
    assert!(extras.revision() > revision);
}

#[test]
fn a_failing_ps_keeps_the_previous_snapshots() {
    let fake = Arc::new(FakeCommands::new());
    fake.on("ps", &["-axww"], 0, &run_listing());
    fake.on(
        "ps",
        &["-axo"],
        0,
        &format!("100 10:00 claude --resume={CHAT}"),
    );
    let extras = extras(&fake);
    let processes = &extras.processes;
    let worktree = Some("/Users/tester/conductor/workspaces/repo/praia");

    processes.run_active(worktree);
    processes.agent_started_at(CHAT);
    extras.wait_idle();
    let start = processes.agent_started_at(CHAT).unwrap();
    assert!(processes.run_active(worktree));
    let revision = extras.revision();

    std::thread::sleep(STALE);
    fake.fail("ps", &[]);
    processes.run_active(worktree);
    processes.agent_started_at(CHAT);
    extras.wait_idle();

    assert_eq!(
        fake.calls().len(),
        4,
        "both listings must have been tried again"
    );
    assert!(processes.run_active(worktree));
    assert_eq!(processes.agent_started_at(CHAT), Some(start));
    assert_eq!(extras.revision(), revision);
}

#[test]
fn a_ps_that_exits_with_an_error_keeps_the_previous_snapshots() {
    let fake = Arc::new(FakeCommands::new());
    fake.on("ps", &["-axww"], 0, &run_listing());
    fake.on(
        "ps",
        &["-axo"],
        0,
        &format!("100 10:00 claude --resume={CHAT}"),
    );
    let extras = extras(&fake);
    let processes = &extras.processes;
    let worktree = Some("/Users/tester/conductor/workspaces/repo/praia");

    processes.run_active(worktree);
    processes.agent_started_at(CHAT);
    extras.wait_idle();
    let start = processes.agent_started_at(CHAT).unwrap();
    let revision = extras.revision();

    std::thread::sleep(STALE);
    fake.on("ps", &[], 1, "");
    processes.run_active(worktree);
    processes.agent_started_at(CHAT);
    extras.wait_idle();

    assert!(processes.run_active(worktree));
    assert_eq!(processes.agent_started_at(CHAT), Some(start));
    assert_eq!(extras.revision(), revision);
}

#[test]
fn ps_is_asked_for_arguments_only_and_never_for_the_environment() {
    let fake = Arc::new(FakeCommands::new());
    fake.on("ps", &["-axww"], 0, "");
    fake.on("ps", &["-axo"], 0, "");
    let extras = extras(&fake);

    extras.processes.run_active(None);
    extras.processes.agent_started_at(CHAT);
    extras.wait_idle();

    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    let mut lists: Vec<Vec<String>> = calls.iter().map(|call| call.args.clone()).collect();
    lists.sort();
    assert_eq!(
        lists,
        [
            vec!["-axo", "pid=,etime=,args="],
            vec!["-axww", "-o", "args="],
        ]
    );
    for call in &calls {
        assert_eq!(call.program, "ps");
        assert_eq!(call.cwd, None);
        assert_eq!(call.limits.timeout, Duration::from_secs(10));
        assert_eq!(call.limits.max_stdout, 16 * 1024 * 1024);
        // The flags: no `e` or `E` (BSD "show the environment"), and no `-E` either. The values of
        // `-o` are column names and may hold the letter.
        let mut args = call.args.iter();
        while let Some(arg) = args.next() {
            if arg == "-o" {
                args.next();
                continue;
            }
            if let Some(flags) = arg.strip_prefix('-') {
                assert!(!flags.contains(['e', 'E']), "environment flag in {arg}");
            }
        }
        assert!(
            call.args.iter().all(|arg| !arg.contains("command")),
            "no environment-bearing column"
        );
    }
}
