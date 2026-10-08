//! A branch name from Conductor's database must never become a `git` option.
//!
//! Each test runs the real `git` in a temporary repository with a base branch that spells an
//! option, `--output=<temporary dir>/written`, which would make `git diff` write that file if it
//! were taken as one. No such file may appear, and the answer must be the one an unknown branch
//! gets.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use conductor_remote::reads::extras::commands::{
    CommandError, Commands, Limits, Output, SystemCommands,
};
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::review::diff::{workspace_diff, workspace_file_diff};
use conductor_remote::reads::workspaces::PrStatus;
use conductor_remote::testing::FakeCommands;
use serde_json::json;

const UNKNOWN: &str = "no-such-branch";
const BRANCH: &str = "feature";

fn git_output(dir: &Path, args: &[&str]) -> String {
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
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository whose `feature` branch is checked out and conflicts with `main`: one tracked
/// file changed in a commit, one changed in the worktree, one untracked file.
struct Fixture {
    _dir: tempfile::TempDir,
    worktree: String,
    /// A base branch that is an option, and the file that option would write.
    evil: String,
    written: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("work");
        std::fs::create_dir(&path).unwrap();
        let git = |args: &[&str]| {
            git_output(&path, args);
        };
        let write = |file: &str, content: &str| std::fs::write(path.join(file), content).unwrap();
        let commit = |message: &str| {
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", message]);
        };

        git(&["init", "-q", "-b", "main"]);
        write("a.txt", "one\ntwo\nthree\n");
        write("c.txt", "base\n");
        commit("base");
        git(&["checkout", "-q", "-b", BRANCH]);
        write("c.txt", "feature\n");
        commit("feature");
        git(&["checkout", "-q", "main"]);
        write("c.txt", "main\n");
        commit("main");
        git(&["checkout", "-q", BRANCH]);
        write("a.txt", "one\nTWO\nthree\nfour\n");
        write("u.txt", "new\nfile\n");

        let written = root.join("written");
        Self {
            evil: format!("--output={}", written.display()),
            written,
            worktree: path.to_str().unwrap().to_owned(),
            _dir: dir,
        }
    }

    fn assert_nothing_written(&self) {
        assert!(
            !self.written.exists(),
            "a base branch was taken as a git option: {} exists",
            self.written.display()
        );
    }
}

#[test]
fn the_fixture_is_sensitive_to_a_real_base() {
    let fixture = Fixture::new();
    let diff = workspace_diff(&SystemCommands, &fixture.worktree, "main");
    assert_eq!(diff.base, "main");
    assert!(diff.merge_base.is_some());
    let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "c.txt", "u.txt"]);
}

#[test]
fn a_workspace_diff_ignores_an_option_as_a_base_branch() {
    let fixture = Fixture::new();
    let evil = workspace_diff(&SystemCommands, &fixture.worktree, &fixture.evil);
    fixture.assert_nothing_written();
    let unknown = workspace_diff(&SystemCommands, &fixture.worktree, UNKNOWN);

    assert_eq!(evil.base, fixture.evil);
    assert_eq!(evil.merge_base, None);
    assert_eq!(
        evil.files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["u.txt"]
    );
    assert_eq!(evil.merge_base, unknown.merge_base);
    assert_eq!(evil.files, unknown.files);
    assert_eq!(evil.patch, unknown.patch);
    assert_eq!(evil.truncated, unknown.truncated);
    assert_eq!(evil.dirty, unknown.dirty);
    assert_eq!(evil.unpushed, unknown.unpushed);
}

#[test]
fn a_file_diff_ignores_an_option_as_a_base_branch() {
    let fixture = Fixture::new();
    for requested in ["a.txt", "u.txt"] {
        let evil =
            workspace_file_diff(&SystemCommands, &fixture.worktree, &fixture.evil, requested);
        fixture.assert_nothing_written();
        let unknown = workspace_file_diff(&SystemCommands, &fixture.worktree, UNKNOWN, requested);
        assert_eq!(evil, unknown, "{requested}");
    }
}

#[test]
fn change_stats_ignore_an_option_as_a_base_branch() {
    let fixture = Fixture::new();
    let stats_for = |base: &str| {
        let extras = Extras::new(Arc::new(SystemCommands), "/home/test");
        extras
            .change_stats
            .get(&fixture.worktree, base, "t1", false);
        extras.wait_idle();
        extras
            .change_stats
            .get(&fixture.worktree, base, "t1", false)
    };
    let evil = stats_for(&fixture.evil);
    fixture.assert_nothing_written();
    let unknown = stats_for(UNKNOWN);
    assert!(evil.is_some());
    assert_eq!(evil, unknown);
}

/// Runs `git` for real and answers `gh` from a script.
struct RealGit {
    gh: FakeCommands,
}

impl Commands for RealGit {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        if program == "git" {
            SystemCommands.run(program, args, cwd, limits)
        } else {
            self.gh.run(program, args, cwd, limits)
        }
    }
}

#[test]
fn a_conflict_check_ignores_an_option_as_a_base_branch() {
    let fixture = Fixture::new();
    let status_for = |base: &str| {
        let gh = FakeCommands::new();
        let listed = json!([{
            "headRefName": BRANCH,
            "number": 7,
            "url": "https://example.test/pull/7",
            "state": "OPEN",
            "isDraft": false,
            "updatedAt": "2026-01-01T00:00:00Z",
            "statusCheckRollup": [],
        }]);
        gh.on("gh", &["pr", "list"], 0, &listed.to_string());
        let extras = Extras::new(Arc::new(RealGit { gh }), "/home/test");
        let ask = || {
            extras.pr.get(
                Some("/repos/app"),
                Some(BRANCH),
                Some(&fixture.worktree),
                base,
            )
        };
        // The pull requests are fetched first, then the conflict check runs for the one found.
        ask();
        extras.wait_idle();
        ask();
        extras.wait_idle();
        ask().status
    };
    // The check does see the conflict with a real base.
    assert_eq!(status_for("main"), Some(PrStatus::Conflicts));

    let evil = status_for(&fixture.evil);
    fixture.assert_nothing_written();
    // `git merge-tree` exits 1 for a revision it cannot merge too, so both read as a conflict.
    assert_eq!(evil, status_for(UNKNOWN));
}
