use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use conductor_remote::reads::extras::commands::Limits;
use conductor_remote::reads::extras::pr::PrFacts;
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::workspaces::PrStatus;
use conductor_remote::testing::FakeCommands;
use serde_json::{json, Value};

const ROOT: &str = "/repos/app";
const WORKTREE: &str = "/worktrees/app/feature";
const BRANCH: &str = "feature";
const BASE: &str = "main";

fn pr(branch: &str, number: i64, state: &str, draft: bool, updated: &str, checks: Value) -> Value {
    json!({
        "headRefName": branch,
        "number": number,
        "url": format!("https://example.test/pull/{number}"),
        "state": state,
        "isDraft": draft,
        "updatedAt": updated,
        "statusCheckRollup": checks,
    })
}

fn open_pr(checks: Value) -> Value {
    pr(BRANCH, 7, "OPEN", false, "2026-01-01T00:00:00Z", checks)
}

fn setup(list: &[Value]) -> (Arc<FakeCommands>, Extras) {
    let commands = Arc::new(FakeCommands::new());
    commands.on("gh", &["pr", "list"], 0, &Value::from(list).to_string());
    let extras = Extras::new(commands.clone(), "/home/test");
    (commands, extras)
}

/// The facts once the background refresh has finished.
fn settled(extras: &Extras, worktree: Option<&str>) -> PrFacts {
    extras.pr.get(Some(ROOT), Some(BRANCH), worktree, BASE);
    extras.wait_idle();
    extras.pr.get(Some(ROOT), Some(BRANCH), worktree, BASE)
}

fn status_of(list: &[Value]) -> Option<PrStatus> {
    let (_commands, extras) = setup(list);
    settled(&extras, None).status
}

fn calls_of(commands: &FakeCommands, program: &str) -> usize {
    commands
        .calls()
        .iter()
        .filter(|call| call.program == program)
        .count()
}

fn git_calls(commands: &FakeCommands) -> usize {
    calls_of(commands, "git")
}

#[test]
fn a_merged_pull_request_is_merged() {
    let list = [pr(
        BRANCH,
        7,
        "MERGED",
        false,
        "2026-01-01T00:00:00Z",
        json!([]),
    )];
    assert_eq!(status_of(&list), Some(PrStatus::Merged));
}

#[test]
fn an_open_draft_is_a_draft() {
    let list = [pr(
        BRANCH,
        7,
        "OPEN",
        true,
        "2026-01-01T00:00:00Z",
        json!([]),
    )];
    assert_eq!(status_of(&list), Some(PrStatus::Draft));
}

#[test]
fn a_failed_check_makes_checks_failed() {
    let failed = json!([
        {"conclusion": "SUCCESS", "status": "COMPLETED"},
        {"conclusion": "TIMED_OUT", "status": "COMPLETED"},
        {"status": "IN_PROGRESS"},
    ]);
    assert_eq!(status_of(&[open_pr(failed)]), Some(PrStatus::ChecksFailed));
    let legacy = json!([{"state": "ERROR"}]);
    assert_eq!(status_of(&[open_pr(legacy)]), Some(PrStatus::ChecksFailed));
}

#[test]
fn a_running_check_makes_checks_pending() {
    let running = json!([
        {"conclusion": "SUCCESS", "status": "COMPLETED"},
        {"conclusion": null, "status": "QUEUED"},
    ]);
    assert_eq!(
        status_of(&[open_pr(running)]),
        Some(PrStatus::ChecksPending)
    );
    let legacy = json!([{"state": "PENDING"}]);
    assert_eq!(status_of(&[open_pr(legacy)]), Some(PrStatus::ChecksPending));
}

#[test]
fn passing_or_absent_checks_make_it_mergeable() {
    let passing = json!([
        {"conclusion": "SUCCESS", "status": "COMPLETED"},
        {"conclusion": "SKIPPED", "status": "COMPLETED"},
        {"state": "SUCCESS"},
    ]);
    assert_eq!(status_of(&[open_pr(passing)]), Some(PrStatus::Mergeable));
    assert_eq!(status_of(&[open_pr(json!([]))]), Some(PrStatus::Mergeable));
    assert_eq!(
        status_of(&[open_pr(Value::Null)]),
        Some(PrStatus::Mergeable)
    );
}

#[test]
fn an_open_pull_request_with_a_conflict_is_conflicts_only_after_the_check_ran() {
    let (commands, extras) = setup(&[open_pr(json!([]))]);
    commands.on("git", &["-C", WORKTREE, "merge-tree"], 1, "");

    let first = extras
        .pr
        .get(Some(ROOT), Some(BRANCH), Some(WORKTREE), BASE);
    assert_eq!(first, PrFacts::default());
    extras.wait_idle();

    // The pull request is known; the verdict is asked for now and counts as no conflict.
    let second = extras
        .pr
        .get(Some(ROOT), Some(BRANCH), Some(WORKTREE), BASE);
    assert_eq!(second.status, Some(PrStatus::Mergeable));
    let before = extras.revision();
    extras.wait_idle();
    assert!(extras.revision() > before);

    let third = extras
        .pr
        .get(Some(ROOT), Some(BRANCH), Some(WORKTREE), BASE);
    assert_eq!(third.status, Some(PrStatus::Conflicts));
    assert_eq!(third.number, Some(7));
}

#[test]
fn a_changed_base_branch_asks_for_a_new_conflict_verdict() {
    let (commands, extras) = setup(&[open_pr(json!([]))]);
    commands.on("git", &["-C", WORKTREE, "merge-tree"], 1, "");
    settled(&extras, Some(WORKTREE));
    extras.wait_idle();
    let facts = extras
        .pr
        .get(Some(ROOT), Some(BRANCH), Some(WORKTREE), BASE);
    assert_eq!(facts.status, Some(PrStatus::Conflicts));

    // The same worktree against another base is a new question: not known yet, so no conflict.
    let other = extras
        .pr
        .get(Some(ROOT), Some(BRANCH), Some(WORKTREE), "develop");
    assert_eq!(other.status, Some(PrStatus::Mergeable));
    extras.wait_idle();
    let merges: Vec<Vec<String>> = commands
        .calls()
        .into_iter()
        .filter(|call| call.args.contains(&"merge-tree".to_owned()))
        .map(|call| call.args)
        .collect();
    assert_eq!(merges.len(), 2);
    assert!(merges[1].contains(&"develop".to_owned()));
}

#[test]
fn a_clean_merge_tree_leaves_the_status_to_the_checks() {
    let failed = json!([{"conclusion": "FAILURE"}]);
    let (commands, extras) = setup(&[open_pr(failed)]);
    commands.on("git", &["-C", WORKTREE, "merge-tree"], 0, "");
    settled(&extras, Some(WORKTREE));
    extras.wait_idle();
    let facts = extras
        .pr
        .get(Some(ROOT), Some(BRANCH), Some(WORKTREE), BASE);
    assert_eq!(facts.status, Some(PrStatus::ChecksFailed));
    assert!(git_calls(&commands) > 0);
}

#[test]
fn the_conflict_check_prefers_origin_and_falls_back_to_the_base() {
    let (commands, extras) = setup(&[open_pr(json!([]))]);
    commands.on("git", &["-C", WORKTREE, "rev-parse"], 1, "");
    commands.on("git", &["-C", WORKTREE, "merge-tree"], 0, "");
    settled(&extras, Some(WORKTREE));
    extras.wait_idle();
    let merge = commands
        .calls()
        .into_iter()
        .find(|call| call.args.contains(&"merge-tree".to_owned()))
        .expect("merge-tree ran");
    assert_eq!(
        merge.args,
        [
            "-C",
            WORKTREE,
            "merge-tree",
            "--write-tree",
            "--end-of-options",
            BASE,
            "HEAD"
        ]
    );
    let probed: Vec<String> = commands
        .calls()
        .into_iter()
        .filter(|call| call.args.contains(&"rev-parse".to_owned()))
        .map(|call| call.args.last().unwrap().clone())
        .collect();
    assert_eq!(probed, ["origin/main^{commit}", "main^{commit}"]);

    let (commands, extras) = setup(&[open_pr(json!([]))]);
    commands.on("git", &["-C", WORKTREE, "rev-parse"], 0, "abc");
    commands.on("git", &["-C", WORKTREE, "merge-tree"], 0, "");
    settled(&extras, Some(WORKTREE));
    extras.wait_idle();
    let merge = commands
        .calls()
        .into_iter()
        .find(|call| call.args.contains(&"merge-tree".to_owned()))
        .expect("merge-tree ran");
    assert_eq!(
        merge.args,
        [
            "-C",
            WORKTREE,
            "merge-tree",
            "--write-tree",
            "--end-of-options",
            "origin/main",
            "HEAD"
        ]
    );
}

#[test]
fn a_closed_pull_request_keeps_its_number_and_url() {
    let list = [pr(
        BRANCH,
        12,
        "CLOSED",
        false,
        "2026-01-01T00:00:00Z",
        json!([]),
    )];
    let (_commands, extras) = setup(&list);
    let facts = settled(&extras, Some(WORKTREE));
    assert_eq!(
        facts,
        PrFacts {
            status: None,
            number: Some(12),
            url: Some("https://example.test/pull/12".to_owned()),
        }
    );
}

#[test]
fn the_newest_of_two_pull_requests_of_a_branch_wins() {
    let older = pr(
        BRANCH,
        3,
        "MERGED",
        false,
        "2026-01-01T00:00:00Z",
        json!([]),
    );
    let newer = pr(BRANCH, 9, "OPEN", true, "2026-02-01T00:00:00Z", json!([]));
    for list in [[older.clone(), newer.clone()], [newer, older]] {
        let (_commands, extras) = setup(&list);
        let facts = settled(&extras, None);
        assert_eq!(facts.number, Some(9));
        assert_eq!(facts.status, Some(PrStatus::Draft));
    }
}

#[test]
fn a_branch_without_a_pull_request_has_the_default() {
    let other = pr("other", 4, "OPEN", false, "2026-01-01T00:00:00Z", json!([]));
    let (_commands, extras) = setup(&[other]);
    assert_eq!(settled(&extras, None), PrFacts::default());
}

#[test]
fn a_failing_gh_gives_the_default_and_is_asked_once_within_the_lifetime() {
    let failures: [fn(&FakeCommands); 4] = [
        |_| {},
        |commands| commands.fail("gh", &["pr"]),
        |commands| commands.on("gh", &["pr"], 1, "[]"),
        |commands| commands.on("gh", &["pr"], 0, "not json"),
    ];
    for fail in failures {
        let commands = Arc::new(FakeCommands::new());
        fail(&commands);
        let extras = Extras::new(commands.clone(), "/home/test");
        for _ in 0..3 {
            let facts = settled(&extras, Some(WORKTREE));
            assert_eq!(facts, PrFacts::default());
        }
        assert_eq!(calls_of(&commands, "gh"), 1);
    }
}

#[test]
fn the_conflict_check_is_not_run_for_a_draft_a_merged_pull_request_or_a_missing_worktree() {
    let cases = [
        (
            pr(BRANCH, 7, "OPEN", true, "2026-01-01T00:00:00Z", json!([])),
            Some(WORKTREE),
        ),
        (
            pr(
                BRANCH,
                7,
                "MERGED",
                false,
                "2026-01-01T00:00:00Z",
                json!([]),
            ),
            Some(WORKTREE),
        ),
        (
            pr(
                BRANCH,
                7,
                "CLOSED",
                false,
                "2026-01-01T00:00:00Z",
                json!([]),
            ),
            Some(WORKTREE),
        ),
        (open_pr(json!([])), None),
        (open_pr(json!([])), Some("")),
    ];
    for (pull_request, worktree) in cases {
        let (commands, extras) = setup(&[pull_request]);
        let first = settled(&extras, worktree);
        extras.wait_idle();
        assert_eq!(first.number, Some(7));
        assert_eq!(git_calls(&commands), 0);
    }
}

#[test]
fn the_first_get_is_the_default_and_the_next_has_the_facts() {
    let (_commands, extras) = setup(&[open_pr(json!([]))]);
    let before = extras.revision();
    assert_eq!(
        extras.pr.get(Some(ROOT), Some(BRANCH), None, BASE),
        PrFacts::default()
    );
    extras.wait_idle();
    assert!(extras.revision() > before);
    assert_eq!(
        extras.pr.get(Some(ROOT), Some(BRANCH), None, BASE),
        PrFacts {
            status: Some(PrStatus::Mergeable),
            number: Some(7),
            url: Some("https://example.test/pull/7".to_owned()),
        }
    );
}

#[test]
fn nothing_is_asked_without_a_repository_root_and_a_branch() {
    let (commands, extras) = setup(&[open_pr(json!([]))]);
    let none = PrFacts::default();
    assert_eq!(extras.pr.get(None, Some(BRANCH), None, BASE), none);
    assert_eq!(extras.pr.get(Some(ROOT), None, None, BASE), none);
    assert_eq!(extras.pr.get(Some(""), Some(BRANCH), None, BASE), none);
    assert_eq!(extras.pr.get(Some(ROOT), Some(""), None, BASE), none);
    extras.wait_idle();
    assert!(commands.calls().is_empty());
}

#[test]
fn the_gh_call_carries_the_repository_root_and_the_limits() {
    let (commands, extras) = setup(&[open_pr(json!([]))]);
    settled(&extras, None);
    let calls = commands.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.program, "gh");
    assert_eq!(
        call.args,
        [
            "pr",
            "list",
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "headRefName,number,url,state,isDraft,updatedAt,statusCheckRollup",
        ]
    );
    assert_eq!(call.cwd, Some(PathBuf::from(ROOT)));
    assert_eq!(
        call.limits,
        Limits {
            timeout: Duration::from_secs(15),
            max_stdout: 8 * 1024 * 1024,
        }
    );
}
