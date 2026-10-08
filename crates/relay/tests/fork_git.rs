//! The git layer of a fork, against real git in temporary directories.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use conductor_remote::fork::{capture, materialize, on_branch, ready, release, Snapshot};
use conductor_remote::reads::extras::commands::{
    CommandError, Commands, Limits, Output, SystemCommands,
};

const NOW: i64 = 1_800_000_000_000;

/// Runs git for the fixture itself, apart from the code under test.
fn run_git(dir: &Path, args: &[&str]) -> (bool, String) {
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, out) = run_git(dir, args);
    assert!(ok, "git {args:?} failed in {dir:?}");
    out
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).expect("write");
}

fn read(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).expect("read")
}

/// A repository whose main checkout is on `main`, with a source worktree on `user/src` and a
/// destination worktree on `user/new`.
struct Fixture {
    _root: tempfile::TempDir,
    repo: PathBuf,
    src: PathBuf,
    dst: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let base = std::fs::canonicalize(root.path()).expect("canonical");
        let repo = base.join("repo");
        std::fs::create_dir(&repo).expect("repo dir");
        git(&repo, &["init", "-q", "-b", "main"]);
        write(&repo, ".gitignore", "*.log\n");
        write(&repo, "tracked.txt", "tracked base\n");
        write(&repo, "staged.txt", "staged base\n");
        write(&repo, "removed.txt", "removed base\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let src = base.join("src");
        let dst = base.join("dst");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "user/src",
                src.to_str().unwrap(),
            ],
        );
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "user/new",
                dst.to_str().unwrap(),
            ],
        );
        Self {
            _root: root,
            repo,
            src,
            dst,
        }
    }

    /// One more commit on the source, then every kind of change on top of it.
    fn dirty(self) -> Self {
        write(&self.src, "extra.txt", "extra\n");
        git(&self.src, &["add", "extra.txt"]);
        git(&self.src, &["commit", "-q", "-m", "extra"]);
        write(&self.src, "staged.txt", "staged edit\n");
        git(&self.src, &["add", "staged.txt"]);
        write(&self.src, "tracked.txt", "unstaged edit\n");
        std::fs::remove_file(self.src.join("removed.txt")).expect("remove");
        write(&self.src, "new.txt", "new\n");
        write(&self.src, "build.log", "log\n");
        self
    }
}

fn status(dir: &Path) -> String {
    git(dir, &["status", "--porcelain=v1"])
}

fn forks(dir: &Path) -> Vec<String> {
    git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/conductor-remote/forks",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

#[test]
fn materialize_makes_the_destination_match_the_source() {
    let f = Fixture::new().dirty();
    let snapshot = capture(&SystemCommands, &f.src, NOW).expect("capture");
    materialize(&SystemCommands, &snapshot, &f.dst, "user/new").expect("materialize");

    assert_eq!(status(&f.dst), status(&f.src));
    for name in [
        "tracked.txt",
        "staged.txt",
        "extra.txt",
        "new.txt",
        ".gitignore",
    ] {
        assert_eq!(read(&f.dst, name), read(&f.src, name), "{name}");
    }
    assert!(!f.dst.join("removed.txt").exists());
    assert!(!f.dst.join("build.log").exists());
    assert!(status(&f.dst).contains("?? new.txt"));
    assert_eq!(
        git(&f.dst, &["rev-parse", "HEAD"]),
        git(&f.src, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&f.dst, &["symbolic-ref", "HEAD"]),
        "refs/heads/user/new"
    );
}

#[test]
fn capture_leaves_the_source_alone() {
    let f = Fixture::new().dirty();
    let state = |dir: &Path| {
        (
            status(dir),
            git(dir, &["write-tree"]),
            git(dir, &["diff", "--cached", "--name-only"]),
        )
    };
    let before = state(&f.src);
    capture(&SystemCommands, &f.src, NOW).expect("capture");
    assert_eq!(state(&f.src), before);
}

#[test]
fn capture_keeps_three_refs_and_release_removes_them() {
    let f = Fixture::new().dirty();
    let snapshot = capture(&SystemCommands, &f.src, NOW).expect("capture");
    assert!(snapshot
        .ref_prefix
        .starts_with(&format!("refs/conductor-remote/forks/{NOW}-")));
    let mut expected: Vec<String> = ["head", "index", "worktree"]
        .iter()
        .map(|s| format!("{}/{s}", snapshot.ref_prefix))
        .collect();
    expected.sort();
    assert_eq!(forks(&f.repo), expected);
    release(&SystemCommands, &snapshot).expect("release");
    assert_eq!(forks(&f.repo), Vec::<String>::new());
}

#[test]
fn capture_prunes_old_refs_only() {
    let f = Fixture::new();
    let head = git(&f.src, &["rev-parse", "HEAD"]);
    let young = format!("refs/conductor-remote/forks/{}-young/head", NOW - 1000);
    git(
        &f.src,
        &[
            "update-ref",
            "refs/conductor-remote/forks/1-old/head",
            &head,
        ],
    );
    git(&f.src, &["update-ref", &young, &head]);
    let snapshot = capture(&SystemCommands, &f.src, NOW).expect("capture");
    let listed = forks(&f.repo);
    assert!(!listed.iter().any(|r| r.contains("1-old")), "{listed:?}");
    assert!(listed.contains(&young), "{listed:?}");
    release(&SystemCommands, &snapshot).expect("release");
}

#[test]
fn capture_refuses_a_merge_in_progress() {
    let f = Fixture::new();
    git(&f.src, &["checkout", "-q", "-b", "other"]);
    write(&f.src, "tracked.txt", "other side\n");
    git(&f.src, &["commit", "-q", "-am", "other"]);
    git(&f.src, &["checkout", "-q", "user/src"]);
    write(&f.src, "tracked.txt", "src side\n");
    git(&f.src, &["commit", "-q", "-am", "src"]);
    let (merged, _) = run_git(&f.src, &["merge", "other"]);
    assert!(!merged, "the merge must conflict");
    let error = capture(&SystemCommands, &f.src, NOW).unwrap_err();
    assert_eq!(
        error.0,
        "the source workspace is in the middle of a Git merge"
    );
}

#[test]
fn capture_refuses_unresolved_conflicts_without_a_marker() {
    let f = Fixture::new();
    // A stash applied over a conflicting commit leaves unmerged entries but no MERGE_HEAD.
    write(&f.src, "tracked.txt", "stashed side\n");
    git(&f.src, &["stash", "-q"]);
    write(&f.src, "tracked.txt", "committed side\n");
    git(&f.src, &["commit", "-q", "-am", "committed"]);
    let (applied, _) = run_git(&f.src, &["stash", "apply", "-q"]);
    assert!(!applied, "the stash must conflict");
    assert!(!git(&f.src, &["ls-files", "-u"]).is_empty());
    let error = capture(&SystemCommands, &f.src, NOW).unwrap_err();
    assert_eq!(error.0, "the source workspace has unresolved Git conflicts");
}

#[test]
fn capture_refuses_a_sub_directory() {
    let f = Fixture::new();
    let sub = f.src.join("sub");
    std::fs::create_dir(&sub).expect("sub dir");
    let error = capture(&SystemCommands, &sub, NOW).unwrap_err();
    assert_eq!(
        error.0,
        "Git resolved the workspace to an ancestor repository"
    );
}

#[test]
fn materialize_refuses_a_destination_with_an_untracked_file() {
    let f = Fixture::new().dirty();
    let snapshot = capture(&SystemCommands, &f.src, NOW).expect("capture");
    write(&f.dst, "stray.txt", "stray\n");
    let head = git(&f.dst, &["rev-parse", "HEAD"]);
    let error = materialize(&SystemCommands, &snapshot, &f.dst, "user/new").unwrap_err();
    assert_eq!(
        error.0,
        "the new workspace changed files before the fork snapshot could be installed"
    );
    assert_eq!(read(&f.dst, "stray.txt"), b"stray\n");
    assert_eq!(git(&f.dst, &["rev-parse", "HEAD"]), head);
}

#[test]
fn materialize_refuses_another_branch() {
    let f = Fixture::new().dirty();
    let snapshot = capture(&SystemCommands, &f.src, NOW).expect("capture");
    let error = materialize(&SystemCommands, &snapshot, &f.dst, "user/other").unwrap_err();
    assert_eq!(error.0, "the new workspace is on another branch");
}

#[test]
fn materialize_refuses_another_repository_and_the_source() {
    let f = Fixture::new().dirty();
    let snapshot = capture(&SystemCommands, &f.src, NOW).expect("capture");

    let other = tempfile::tempdir().expect("temp dir");
    let other_dir = std::fs::canonicalize(other.path()).expect("canonical");
    git(&other_dir, &["init", "-q", "-b", "main"]);
    write(&other_dir, "a.txt", "a\n");
    git(&other_dir, &["add", "."]);
    git(&other_dir, &["commit", "-q", "-m", "a"]);
    let error = materialize(&SystemCommands, &snapshot, &other_dir, "main").unwrap_err();
    assert_eq!(
        error.0,
        "the new workspace belongs to a different Git repository"
    );

    let error = materialize(&SystemCommands, &snapshot, &f.src, "user/src").unwrap_err();
    assert_eq!(error.0, "the new workspace is the source workspace");
}

#[test]
fn ready_needs_a_clean_checkout_of_the_branch() {
    let f = Fixture::new();
    assert!(ready(&SystemCommands, &f.dst, "user/new"));
    assert!(!ready(&SystemCommands, &f.dst.join("missing"), "user/new"));
    assert!(!ready(&SystemCommands, &f.dst, "user/other"));
    write(&f.dst, "stray.txt", "stray\n");
    assert!(!ready(&SystemCommands, &f.dst, "user/new"));
}

#[test]
fn on_branch_needs_the_branch_checked_out() {
    let f = Fixture::new();
    assert!(on_branch(&SystemCommands, &f.src, "user/src"));
    assert!(!on_branch(&SystemCommands, &f.src, "user/new"));
    assert!(!on_branch(
        &SystemCommands,
        &f.src.join("missing"),
        "user/src"
    ));
}

/// Delegates to the real programs, except for the calls it is told to fail: those exit with code
/// 1 and `fatal: injected` on the standard error.
struct Failing {
    /// Every `read-tree` made after the failed `update-ref` fails too.
    read_trees_after: bool,
    /// The `update-ref` of `HEAD` was failed.
    failed: AtomicBool,
}

impl Failing {
    fn new(read_trees_after: bool) -> Self {
        Self {
            read_trees_after,
            failed: AtomicBool::new(false),
        }
    }
}

impl Commands for Failing {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        let fails = if args.contains(&"update-ref") && args.contains(&"HEAD") {
            self.failed.store(true, Ordering::SeqCst);
            true
        } else {
            self.read_trees_after
                && args.contains(&"read-tree")
                && self.failed.load(Ordering::SeqCst)
        };
        if fails {
            return Ok(Output {
                code: Some(1),
                stdout: Vec::new(),
                stderr: b"fatal: injected\n".to_vec(),
            });
        }
        SystemCommands.run(program, args, cwd, limits)
    }
}

#[test]
fn a_failed_install_puts_the_destination_back() {
    let f = Fixture::new().dirty();
    let snapshot = capture(&SystemCommands, &f.src, NOW).expect("capture");
    let head = git(&f.dst, &["rev-parse", "HEAD"]);
    let failing = Failing::new(false);
    let error = materialize(&failing, &snapshot, &f.dst, "user/new").unwrap_err();
    assert_eq!(error.0, "git update-ref failed: fatal: injected");
    assert!(failing.failed.load(Ordering::SeqCst));

    assert_eq!(status(&f.dst), "");
    assert_eq!(git(&f.dst, &["rev-parse", "HEAD"]), head);
    assert_eq!(read(&f.dst, "removed.txt"), b"removed base\n");
    assert_eq!(read(&f.dst, "tracked.txt"), b"tracked base\n");
    assert_eq!(read(&f.dst, "staged.txt"), b"staged base\n");
    assert!(!f.dst.join("new.txt").exists());
    assert!(!f.dst.join("extra.txt").exists());
}

#[test]
fn a_failed_install_that_cannot_be_put_back_says_so() {
    let f = Fixture::new().dirty();
    let snapshot = capture(&SystemCommands, &f.src, NOW).expect("capture");
    let failing = Failing::new(true);
    let error = materialize(&failing, &snapshot, &f.dst, "user/new").unwrap_err();
    assert!(
        error
            .0
            .ends_with("; the new workspace may hold part of the code"),
        "{}",
        error.0
    );
    assert!(
        error
            .0
            .starts_with("git update-ref failed: fatal: injected"),
        "{}",
        error.0
    );
}

/// Records every call and delegates to the real programs.
struct Recorder {
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

impl Commands for Recorder {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        self.calls.lock().expect("lock").push((
            program.to_owned(),
            args.iter().map(|a| (*a).to_owned()).collect(),
        ));
        SystemCommands.run(program, args, cwd, limits)
    }
}

#[test]
fn every_call_goes_through_env_with_the_git_variables_unset() {
    let f = Fixture::new().dirty();
    let recorder = Recorder {
        calls: Mutex::new(Vec::new()),
    };
    let snapshot: Snapshot = capture(&recorder, &f.src, NOW).expect("capture");
    materialize(&recorder, &snapshot, &f.dst, "user/new").expect("materialize");
    release(&recorder, &snapshot).expect("release");

    let calls = recorder.calls.lock().expect("lock");
    assert!(!calls.is_empty());
    let unset = [
        "-u",
        "GIT_DIR",
        "-u",
        "GIT_WORK_TREE",
        "-u",
        "GIT_COMMON_DIR",
        "-u",
        "GIT_INDEX_FILE",
    ];
    let mut alternate = 0;
    for (program, args) in calls.iter() {
        assert_eq!(program, "env");
        assert_eq!(&args[..8], &unset, "{args:?}");
        for arg in args {
            if let Some(path) = arg.strip_prefix("GIT_INDEX_FILE=") {
                alternate += 1;
                let path = Path::new(path);
                assert!(
                    !path.starts_with(&f.src) && !path.starts_with(&f.dst),
                    "{path:?}"
                );
            }
        }
    }
    assert!(alternate > 0, "the alternate index was never used");
}
