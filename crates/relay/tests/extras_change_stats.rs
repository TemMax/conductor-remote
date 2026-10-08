use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use conductor_remote::reads::extras::commands::{Limits, SystemCommands};
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::workspaces::ChangeStats;
use conductor_remote::testing::{CommandCall, FakeCommands};

fn stats(added: i64, removed: i64) -> Option<ChangeStats> {
    Some(ChangeStats { added, removed })
}

// ------------------------------------------------------------------ a real repository

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.org",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An empty repository on `main`, in a canonical temporary path.
fn new_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    (dir, repo)
}

fn commit_file(repo: &Path, name: &str, content: &str) {
    std::fs::write(repo.join(name), content).unwrap();
    git(repo, &["add", name]);
    git(repo, &["commit", "-q", "-m", name]);
}

/// The stats of `repo` against `main`, once the first refresh has finished.
fn real_stats(repo: &Path) -> Option<ChangeStats> {
    let extras = Extras::new(Arc::new(SystemCommands), repo);
    let worktree = repo.to_str().unwrap();
    assert_eq!(extras.change_stats.get(worktree, "main", "t", false), None);
    extras.wait_idle();
    extras.change_stats.get(worktree, "main", "t", false)
}

#[test]
fn counts_committed_base_worktree_and_untracked_lines() {
    let (_dir, repo) = new_repo();
    commit_file(&repo, "tracked.txt", "alpha\nbeta\n");
    git(&repo, &["checkout", "-q", "-b", "feature"]);
    // Committed on the branch: 3 added.
    commit_file(&repo, "feature.txt", "1\n2\n3\n");
    // Unstaged edit of a tracked file: 2 added, 1 removed.
    std::fs::write(repo.join("tracked.txt"), "alpha changed\nbeta\ngamma\n").unwrap();
    // Untracked: 2 added, though the last line is unterminated.
    std::fs::write(repo.join("new.txt"), "one\ntwo").unwrap();
    // A binary file counts as 0, and so does an empty one, without voiding the result.
    std::fs::write(repo.join("binary.bin"), [0u8, 1, 2]).unwrap();
    std::fs::write(repo.join("empty.txt"), "").unwrap();

    assert_eq!(real_stats(&repo), stats(7, 1));
}

#[test]
fn an_empty_untracked_file_alone_counts_as_zero() {
    let (_dir, repo) = new_repo();
    commit_file(&repo, "tracked.txt", "alpha\n");
    std::fs::write(repo.join("empty.txt"), "").unwrap();

    assert_eq!(real_stats(&repo), stats(0, 0));
}

#[test]
fn origin_base_is_preferred_when_it_exists() {
    let (_dir, repo) = new_repo();
    commit_file(&repo, "a.txt", "1\n");
    git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    // Local main moved on: against `main` this is 0 lines, against `origin/main` it is 2.
    commit_file(&repo, "b.txt", "1\n2\n");

    assert_eq!(real_stats(&repo), stats(2, 0));
}

#[test]
fn a_base_that_exists_nowhere_still_counts_the_worktree() {
    let (_dir, repo) = new_repo();
    commit_file(&repo, "a.txt", "1\n");
    std::fs::write(repo.join("new.txt"), "x\ny\n").unwrap();

    let extras = Extras::new(Arc::new(SystemCommands), &repo);
    let worktree = repo.to_str().unwrap();
    extras
        .change_stats
        .get(worktree, "no-such-branch", "t", false);
    extras.wait_idle();
    // The tracked diff against a ref that does not exist fails and loses its lines only.
    assert_eq!(
        extras
            .change_stats
            .get(worktree, "no-such-branch", "t", false),
        stats(2, 0)
    );
}

// ------------------------------------------------------------------ scripted commands

const W: &str = "/work/tree";

fn extras_with(fake: &Arc<FakeCommands>) -> Extras {
    Extras::new(fake.clone(), "/home")
}

/// A refresh whose every command succeeds: 3 tracked lines added and 1 removed, two untracked
/// files of 2 lines each.
fn scripted() -> Arc<FakeCommands> {
    let fake = Arc::new(FakeCommands::new());
    fake.on("git", &["-C", W, "rev-parse"], 0, "");
    fake.on("git", &["-C", W, "merge-base"], 0, "abc123\n");
    fake.on(
        "git",
        &["-C", W, "diff", "--numstat"],
        0,
        "3\t1\ta.rs\n-\t-\tb.png\n",
    );
    fake.on("git", &["-C", W, "ls-files"], 0, "x.txt\0y.txt\0");
    fake.on("git", &["-C", W, "diff", "--no-index"], 1, "2\t0\tf\n");
    fake
}

fn get(extras: &Extras, updated_at: &str, working: bool) -> Option<ChangeStats> {
    extras.change_stats.get(W, "main", updated_at, working)
}

/// How many calls ran `git -C <W> <subcommand> <first flag>`.
fn count_calls(calls: &[CommandCall], subcommand: &str, flag: &str) -> usize {
    calls
        .iter()
        .filter(|c| c.args.get(2).map(String::as_str) == Some(subcommand))
        .filter(|c| c.args.get(3).map(String::as_str) == Some(flag))
        .count()
}

#[test]
fn the_first_get_is_nothing_and_the_next_has_the_stats() {
    let fake = scripted();
    let extras = extras_with(&fake);
    assert_eq!(get(&extras, "t1", false), None);
    extras.wait_idle();
    assert_eq!(get(&extras, "t1", false), stats(7, 1));
}

#[test]
fn a_refresh_runs_the_git_commands_in_order_with_the_limits() {
    let fake = scripted();
    let extras = extras_with(&fake);
    get(&extras, "t1", false);
    extras.wait_idle();

    let calls = fake.calls();
    let shape: Vec<Vec<&str>> = calls
        .iter()
        .map(|c| c.args.iter().skip(2).map(String::as_str).collect())
        .collect();
    assert_eq!(
        shape,
        vec![
            vec![
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                "origin/main^{commit}"
            ],
            vec!["merge-base", "--end-of-options", "origin/main", "HEAD"],
            vec!["diff", "--numstat", "--end-of-options", "abc123"],
            vec!["ls-files", "--others", "--exclude-standard", "-z"],
            vec![
                "diff",
                "--no-index",
                "--numstat",
                "--",
                "/dev/null",
                "x.txt"
            ],
            vec![
                "diff",
                "--no-index",
                "--numstat",
                "--",
                "/dev/null",
                "y.txt"
            ],
        ]
    );
    let limits = Limits {
        timeout: Duration::from_secs(15),
        max_stdout: 8 * 1024 * 1024,
    };
    for call in &calls {
        assert_eq!(call.program, "git");
        assert_eq!(call.args[0], "-C");
        assert_eq!(call.args[1], W);
        assert_eq!(call.limits, limits, "{:?}", call.args);
    }
}

#[test]
fn a_changed_updated_at_returns_the_old_stats_and_refreshes_once() {
    let fake = scripted();
    let extras = extras_with(&fake);
    get(&extras, "t1", false);
    extras.wait_idle();
    assert_eq!(get(&extras, "t1", false), stats(7, 1));
    assert_eq!(count_calls(&fake.calls(), "diff", "--numstat"), 1);

    fake.on("git", &["-C", W, "diff", "--numstat"], 0, "10\t0\ta.rs\n");
    // The workspace moved: the old stats come back at once and one refresh starts.
    assert_eq!(get(&extras, "t2", false), stats(7, 1));
    extras.wait_idle();
    assert_eq!(get(&extras, "t2", false), stats(14, 0));
    assert_eq!(get(&extras, "t2", false), stats(14, 0));
    extras.wait_idle();
    assert_eq!(count_calls(&fake.calls(), "diff", "--numstat"), 2);
}

#[test]
fn a_command_that_times_out_stores_nothing_and_is_not_retried_while_fresh() {
    let fake = scripted();
    fake.fail("git", &["-C", W, "diff", "--numstat"]);
    let extras = extras_with(&fake);
    assert_eq!(get(&extras, "t1", false), None);
    extras.wait_idle();
    assert_eq!(get(&extras, "t1", false), None);
    extras.wait_idle();
    assert_eq!(get(&extras, "t1", true), None);
    extras.wait_idle();

    let calls = fake.calls();
    assert_eq!(count_calls(&calls, "rev-parse", "--verify"), 1);
    assert_eq!(count_calls(&calls, "diff", "--numstat"), 1);
    // The untracked files were never reached.
    assert_eq!(count_calls(&calls, "ls-files", "--others"), 0);
}

#[test]
fn a_failed_refresh_is_replaced_when_the_workspace_changes() {
    let fake = scripted();
    fake.fail("git", &["-C", W, "ls-files"]);
    let extras = extras_with(&fake);
    get(&extras, "t1", false);
    extras.wait_idle();
    assert_eq!(get(&extras, "t1", false), None);

    fake.on("git", &["-C", W, "ls-files"], 0, "");
    assert_eq!(get(&extras, "t2", false), None);
    extras.wait_idle();
    assert_eq!(get(&extras, "t2", false), stats(3, 1));
}

#[test]
fn exit_codes_never_void_the_result() {
    let fake = Arc::new(FakeCommands::new());
    // Neither ref names a commit, and there is no merge base.
    fake.on("git", &["-C", W, "rev-parse"], 1, "");
    fake.on("git", &["-C", W, "merge-base"], 1, "ignored\n");
    // A failing diff and a failing listing lose their own lines only.
    fake.on("git", &["-C", W, "diff", "--numstat"], 128, "9\t9\tx\n");
    fake.on("git", &["-C", W, "ls-files"], 128, "x.txt\0");
    let extras = extras_with(&fake);
    get(&extras, "t1", false);
    extras.wait_idle();
    assert_eq!(get(&extras, "t1", false), stats(0, 0));

    let calls = fake.calls();
    let shape: Vec<Vec<&str>> = calls
        .iter()
        .map(|c| c.args.iter().skip(2).map(String::as_str).collect())
        .collect();
    assert_eq!(
        shape,
        vec![
            vec![
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                "origin/main^{commit}"
            ],
            vec![
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                "main^{commit}"
            ],
            vec!["merge-base", "--end-of-options", "main", "HEAD"],
            // Without a merge base the diff is against the ref itself.
            vec!["diff", "--numstat", "--end-of-options", "main"],
            vec!["ls-files", "--others", "--exclude-standard", "-z"],
        ]
    );
}

#[test]
fn an_empty_merge_base_falls_back_to_the_ref() {
    let fake = scripted();
    fake.on("git", &["-C", W, "merge-base"], 0, "  \n");
    let extras = extras_with(&fake);
    get(&extras, "t1", false);
    extras.wait_idle();
    assert_eq!(get(&extras, "t1", false), stats(7, 1));
    let calls = fake.calls();
    let diff = calls.iter().find(|c| c.args[3] == "--numstat").unwrap();
    assert_eq!(diff.args[5], "origin/main");
}

#[test]
fn only_the_first_500_untracked_files_are_counted() {
    let fake = scripted();
    let listing: String = (0..501).map(|n| format!("f{n}.txt\0")).collect();
    fake.on("git", &["-C", W, "ls-files"], 0, &listing);
    fake.on("git", &["-C", W, "diff", "--no-index"], 1, "1\t0\tf\n");
    let extras = extras_with(&fake);
    get(&extras, "t1", false);
    extras.wait_idle();

    assert_eq!(get(&extras, "t1", false), stats(3 + 500, 1));
    let calls = fake.calls();
    assert_eq!(count_calls(&calls, "diff", "--no-index"), 500);
    let last = calls.last().unwrap();
    assert_eq!(last.args.last().map(String::as_str), Some("f499.txt"));
}
