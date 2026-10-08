use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use conductor_remote::reads::extras::commands::{Limits, SystemCommands};
use conductor_remote::reads::review::basis::{
    diff_basis, parse_numstat_z, tracked_diff_files, untracked_files, untracked_patch, DiffBasis,
    DiffFile, GIT_LIMITS, MAX_UNTRACKED_FILES, UNTRACKED_LIMITS,
};
use conductor_remote::testing::{CommandCall, FakeCommands};

fn file(path: &str, added: i64, removed: i64) -> DiffFile {
    DiffFile {
        path: path.to_owned(),
        old_path: None,
        added,
        removed,
    }
}

fn moved(old: &str, new: &str, added: i64, removed: i64) -> DiffFile {
    DiffFile {
        path: new.to_owned(),
        old_path: Some(old.to_owned()),
        added,
        removed,
    }
}

// ------------------------------------------------------------------ the limits

#[test]
fn the_limits_are_the_documented_ones() {
    assert_eq!(
        GIT_LIMITS,
        Limits {
            timeout: Duration::from_secs(15),
            max_stdout: 8 * 1024 * 1024
        }
    );
    assert_eq!(
        UNTRACKED_LIMITS,
        Limits {
            timeout: Duration::from_secs(10),
            max_stdout: 8 * 1024 * 1024
        }
    );
    assert_eq!(MAX_UNTRACKED_FILES, 500);
}

// ------------------------------------------------------------------ parse_numstat_z

#[test]
fn plain_records() {
    let out = b"1\t2\tsrc/a.rs\x0010\t0\tb.txt\x00";
    assert_eq!(
        parse_numstat_z(out),
        vec![file("src/a.rs", 1, 2), file("b.txt", 10, 0)]
    );
}

#[test]
fn empty_output_and_empty_records_give_nothing() {
    assert_eq!(parse_numstat_z(b""), vec![]);
    assert_eq!(parse_numstat_z(b"\x00\x00\x00"), vec![]);
    assert_eq!(
        parse_numstat_z(b"\x001\t1\ta\x00\x00\x002\t2\tb\x00"),
        vec![file("a", 1, 1), file("b", 2, 2)]
    );
}

#[test]
fn a_record_without_a_trailing_nul_still_counts() {
    assert_eq!(parse_numstat_z(b"3\t4\ta"), vec![file("a", 3, 4)]);
}

#[test]
fn binary_dashes_count_as_zero() {
    let out = b"-\t-\timage.png\x005\t-\tmixed\x00";
    assert_eq!(
        parse_numstat_z(out),
        vec![file("image.png", 0, 0), file("mixed", 5, 0)]
    );
}

#[test]
fn renames_and_copies_take_two_more_records() {
    let out = b"3\t1\t\x00old/name.rs\x00new/name.rs\x002\t0\tplain.rs\x000\t0\t\x00orig.txt\x00copy.txt\x00";
    assert_eq!(
        parse_numstat_z(out),
        vec![
            moved("old/name.rs", "new/name.rs", 3, 1),
            file("plain.rs", 2, 0),
            moved("orig.txt", "copy.txt", 0, 0),
        ]
    );
}

#[test]
fn a_binary_rename_is_one_zero_line_change() {
    assert_eq!(
        parse_numstat_z(b"-\t-\t\x00a.png\x00b.png\x00"),
        vec![moved("a.png", "b.png", 0, 0)]
    );
}

#[test]
fn a_rename_cut_short_ends_the_parsing() {
    // Neither path.
    assert_eq!(
        parse_numstat_z(b"1\t1\tkept\x003\t1\t\x00"),
        vec![file("kept", 1, 1)]
    );
    // Only the old path.
    assert_eq!(
        parse_numstat_z(b"1\t1\tkept\x003\t1\t\x00old\x00"),
        vec![file("kept", 1, 1)]
    );
    // An empty new path.
    assert_eq!(
        parse_numstat_z(b"1\t1\tkept\x003\t1\t\x00old\x00\x00"),
        vec![file("kept", 1, 1)]
    );
    // Nothing after the cut-short rename is read, even a well-formed record.
    assert_eq!(
        parse_numstat_z(b"3\t1\t\x00old\x00\x009\t9\tlost\x00"),
        vec![]
    );
}

#[test]
fn a_record_without_two_tabs_is_skipped() {
    assert_eq!(
        parse_numstat_z(b"garbage\x001\ttoo-few\x001\t2\tok\x00"),
        vec![file("ok", 1, 2)]
    );
}

#[test]
fn a_count_that_is_not_a_number_counts_as_zero() {
    assert_eq!(
        parse_numstat_z(b"x\t7\ta\x003\t+4\tb\x001.5\t2\tc\x00 1\t2\td\x00"),
        vec![
            file("a", 0, 7),
            file("b", 3, 0),
            file("c", 0, 2),
            file("d", 0, 2)
        ]
    );
}

#[test]
fn paths_with_spaces_tabs_and_non_ascii_stay_literal() {
    let out = "1\t0\twith space.txt\x002\t0\ttab\there.txt\x003\t0\tдом/日本語.rs\x00".as_bytes();
    assert_eq!(
        parse_numstat_z(out),
        vec![
            file("with space.txt", 1, 0),
            file("tab\there.txt", 2, 0),
            file("дом/日本語.rs", 3, 0),
        ]
    );
}

#[test]
fn a_renamed_path_may_hold_tabs_and_non_ascii() {
    let out = "4\t4\t\x00a\tb ü\x00c\td ö\x00".as_bytes();
    assert_eq!(parse_numstat_z(out), vec![moved("a\tb ü", "c\td ö", 4, 4)]);
}

#[test]
fn paths_that_are_not_utf8_are_read_lossily() {
    let out = b"1\t1\tbad\xff.txt\x00";
    assert_eq!(parse_numstat_z(out), vec![file("bad\u{fffd}.txt", 1, 1)]);
}

#[test]
fn a_diff_file_serialises_like_the_web_type() {
    assert_eq!(
        serde_json::to_value(file("a.rs", 1, 2)).unwrap(),
        serde_json::json!({"path": "a.rs", "added": 1, "removed": 2})
    );
    assert_eq!(
        serde_json::to_value(moved("a.rs", "b.rs", 0, 0)).unwrap(),
        serde_json::json!({"path": "b.rs", "oldPath": "a.rs", "added": 0, "removed": 0})
    );
}

// ------------------------------------------------------------------ diff_basis with FakeCommands

const WT: &str = "/work/tree";

fn call(args: &[&str], limits: Limits) -> CommandCall {
    let mut full = vec!["-C".to_owned(), WT.to_owned()];
    full.extend(args.iter().map(|a| (*a).to_owned()));
    CommandCall {
        program: "git".to_owned(),
        args: full,
        cwd: None,
        limits,
    }
}

fn basis(base: &str, merge_base: Option<&str>, against: &str) -> DiffBasis {
    DiffBasis {
        base: base.to_owned(),
        merge_base: merge_base.map(str::to_owned),
        against: against.to_owned(),
    }
}

#[test]
fn the_origin_ref_is_preferred() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "rev-parse"], 0, "abc\n");
    fake.on("git", &["-C", WT, "merge-base"], 0, "def\n");
    assert_eq!(
        diff_basis(&fake, WT, "main"),
        basis("origin/main", Some("def"), "def")
    );
    assert_eq!(
        fake.calls(),
        vec![
            call(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    "origin/main^{commit}"
                ],
                GIT_LIMITS
            ),
            call(
                &["merge-base", "--end-of-options", "origin/main", "HEAD"],
                GIT_LIMITS
            ),
        ]
    );
}

#[test]
fn the_local_base_is_used_when_origin_does_not_name_a_commit() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "rev-parse"], 0, "abc\n");
    fake.on(
        "git",
        &[
            "-C",
            WT,
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            "origin/main^{commit}",
        ],
        1,
        "",
    );
    fake.on("git", &["-C", WT, "merge-base"], 0, "def\n");
    assert_eq!(
        diff_basis(&fake, WT, "main"),
        basis("main", Some("def"), "def")
    );
    assert_eq!(
        fake.calls(),
        vec![
            call(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    "origin/main^{commit}"
                ],
                GIT_LIMITS
            ),
            call(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    "main^{commit}"
                ],
                GIT_LIMITS
            ),
            call(
                &["merge-base", "--end-of-options", "main", "HEAD"],
                GIT_LIMITS
            ),
        ]
    );
}

#[test]
fn a_rev_parse_that_cannot_run_counts_as_not_a_commit() {
    let fake = FakeCommands::new();
    fake.fail(
        "git",
        &[
            "-C",
            WT,
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            "origin/main^{commit}",
        ],
    );
    fake.on(
        "git",
        &[
            "-C",
            WT,
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            "main^{commit}",
        ],
        0,
        "abc\n",
    );
    fake.on("git", &["-C", WT, "merge-base"], 0, "def\n");
    assert_eq!(
        diff_basis(&fake, WT, "main"),
        basis("main", Some("def"), "def")
    );
}

#[test]
fn the_base_as_given_is_used_when_neither_names_a_commit() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "rev-parse"], 1, "");
    fake.on("git", &["-C", WT, "merge-base"], 1, "");
    assert_eq!(diff_basis(&fake, WT, "dev"), basis("dev", None, "dev"));
    assert_eq!(
        fake.calls(),
        vec![
            call(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    "origin/dev^{commit}"
                ],
                GIT_LIMITS
            ),
            call(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    "dev^{commit}"
                ],
                GIT_LIMITS
            ),
            call(
                &["merge-base", "--end-of-options", "dev", "HEAD"],
                GIT_LIMITS
            ),
        ]
    );
}

#[test]
fn a_merge_base_is_trimmed() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "rev-parse"], 0, "abc\n");
    fake.on("git", &["-C", WT, "merge-base"], 0, "  0123abcd \n\n");
    let got = diff_basis(&fake, WT, "main");
    assert_eq!(got.merge_base.as_deref(), Some("0123abcd"));
    assert_eq!(got.against, "0123abcd");
}

#[test]
fn a_failing_merge_base_leaves_the_base_to_diff_against() {
    for failure in 0..3 {
        let fake = FakeCommands::new();
        fake.on("git", &["-C", WT, "rev-parse"], 0, "abc\n");
        match failure {
            0 => fake.on("git", &["-C", WT, "merge-base"], 1, ""),
            1 => fake.fail("git", &["-C", WT, "merge-base"]),
            // Exit 0 and nothing printed.
            _ => fake.on("git", &["-C", WT, "merge-base"], 0, "\n"),
        }
        assert_eq!(
            diff_basis(&fake, WT, "main"),
            basis("origin/main", None, "origin/main"),
            "case {failure}"
        );
    }
}

#[test]
fn a_merge_base_printed_with_a_failing_exit_is_not_used() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "rev-parse"], 0, "abc\n");
    fake.on("git", &["-C", WT, "merge-base"], 1, "def\n");
    assert_eq!(
        diff_basis(&fake, WT, "main"),
        basis("origin/main", None, "origin/main")
    );
}

#[test]
fn nothing_scripted_gives_the_base_as_given() {
    let fake = FakeCommands::new();
    assert_eq!(diff_basis(&fake, WT, "main"), basis("main", None, "main"));
}

// ------------------------------------------------------------------ the other calls with FakeCommands

#[test]
fn tracked_diff_files_runs_numstat_z_with_the_git_limits() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "diff"], 0, "1\t2\ta.rs\0");
    assert_eq!(
        tracked_diff_files(&fake, WT, "abc"),
        vec![file("a.rs", 1, 2)]
    );
    assert_eq!(
        fake.calls(),
        vec![call(
            &["diff", "--numstat", "-z", "--end-of-options", "abc"],
            GIT_LIMITS
        )]
    );
}

#[test]
fn tracked_diff_files_is_empty_on_any_failure() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "diff"], 128, "1\t2\ta.rs\0");
    assert_eq!(tracked_diff_files(&fake, WT, "abc"), vec![]);
    let fake = FakeCommands::new();
    fake.fail("git", &["-C", WT, "diff"]);
    assert_eq!(tracked_diff_files(&fake, WT, "abc"), vec![]);
    assert_eq!(tracked_diff_files(&FakeCommands::new(), WT, "abc"), vec![]);
}

#[test]
fn untracked_files_lists_all_of_them_in_order_with_the_git_limits() {
    let fake = FakeCommands::new();
    let listing: String = (0..MAX_UNTRACKED_FILES + 20)
        .map(|n| format!("f{n}\0"))
        .collect();
    fake.on("git", &["-C", WT, "ls-files"], 0, &format!("{listing}\0"));
    let got = untracked_files(&fake, WT);
    assert_eq!(got.len(), MAX_UNTRACKED_FILES + 20);
    assert_eq!(got[0], "f0");
    assert_eq!(got[519], "f519");
    assert_eq!(
        fake.calls(),
        vec![call(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            GIT_LIMITS
        )]
    );
}

#[test]
fn untracked_files_is_empty_on_any_failure() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "ls-files"], 128, "a\0");
    assert_eq!(untracked_files(&fake, WT), Vec::<String>::new());
    let fake = FakeCommands::new();
    fake.fail("git", &["-C", WT, "ls-files"]);
    assert_eq!(untracked_files(&fake, WT), Vec::<String>::new());
    assert_eq!(
        untracked_files(&FakeCommands::new(), WT),
        Vec::<String>::new()
    );
}

#[test]
fn untracked_patch_takes_the_output_whatever_the_exit_code_and_carries_its_limits() {
    let patch = "diff --git a/x b/x\nnew file mode 100644\n--- /dev/null\n+++ b/x\n@@ -0,0 +1,2 @@\n+one\n+++two\n";
    for code in [0, 1, 2] {
        let fake = FakeCommands::new();
        fake.on("git", &["-C", WT, "diff"], code, patch);
        // `+++two` is an added line that starts like a header; as in the reference it is not counted.
        assert_eq!(
            untracked_patch(&fake, WT, "x"),
            Some((patch.to_owned(), 1)),
            "exit {code}"
        );
        assert_eq!(
            fake.calls(),
            vec![call(
                &["diff", "--no-index", "--no-color", "--", "/dev/null", "x"],
                UNTRACKED_LIMITS
            )]
        );
    }
}

#[test]
fn untracked_patch_is_none_for_no_output_or_a_command_that_cannot_run() {
    let fake = FakeCommands::new();
    fake.on("git", &["-C", WT, "diff"], 1, "");
    assert_eq!(untracked_patch(&fake, WT, "x"), None);
    let fake = FakeCommands::new();
    fake.fail("git", &["-C", WT, "diff"]);
    assert_eq!(untracked_patch(&fake, WT, "x"), None);
    assert_eq!(untracked_patch(&FakeCommands::new(), WT, "x"), None);
}

// ------------------------------------------------------------------ real repositories

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

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A repository on `main` with one committed file, in a canonical temporary path.
fn new_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    (dir, repo)
}

#[test]
fn diff_basis_in_a_real_repository() {
    let (_dir, repo) = new_repo();
    let fork = git_stdout(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["switch", "-q", "-c", "feature"]);
    std::fs::write(repo.join("feature.txt"), "feature\n").unwrap();
    git(&repo, &["add", "feature.txt"]);
    git(&repo, &["commit", "-q", "-m", "feature"]);
    // `main` moves on, so the merge base is the fork point and not the tip of `main`.
    git(&repo, &["switch", "-q", "main"]);
    std::fs::write(repo.join("later.txt"), "later\n").unwrap();
    git(&repo, &["add", "later.txt"]);
    git(&repo, &["commit", "-q", "-m", "later"]);
    git(&repo, &["switch", "-q", "feature"]);

    let wt = repo.to_str().unwrap();
    assert_eq!(
        diff_basis(&SystemCommands, wt, "main"),
        basis("main", Some(&fork), &fork)
    );
    // A base that names nothing is kept as given, and nothing is merged with it.
    assert_eq!(
        diff_basis(&SystemCommands, wt, "nope"),
        basis("nope", None, "nope")
    );
    // The same against a directory that is no repository.
    let elsewhere = tempfile::tempdir().unwrap();
    assert_eq!(
        diff_basis(&SystemCommands, elsewhere.path().to_str().unwrap(), "main"),
        basis("main", None, "main")
    );
}

#[test]
fn diff_basis_prefers_a_remote_tracking_ref_in_a_real_repository() {
    let (_dir, repo) = new_repo();
    let fork = git_stdout(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["update-ref", "refs/remotes/origin/main", &fork]);
    git(&repo, &["switch", "-q", "-c", "feature"]);
    let wt = repo.to_str().unwrap();
    assert_eq!(
        diff_basis(&SystemCommands, wt, "main"),
        basis("origin/main", Some(&fork), &fork)
    );
}

#[test]
fn tracked_and_untracked_files_in_a_real_repository() {
    let (_dir, repo) = new_repo();
    std::fs::write(repo.join("base.txt"), "base\nmore\n").unwrap();
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    std::fs::write(repo.join("a b.txt"), "a\n").unwrap();
    std::fs::create_dir(repo.join("dir")).unwrap();
    std::fs::write(repo.join("dir/c.txt"), "c\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(repo.join("ignored.txt"), "x\n").unwrap();
    let wt = repo.to_str().unwrap();

    assert_eq!(
        tracked_diff_files(&SystemCommands, wt, "HEAD"),
        vec![file("base.txt", 1, 0)]
    );
    assert_eq!(
        untracked_files(&SystemCommands, wt),
        vec![".gitignore", "a b.txt", "b.txt", "dir/c.txt"]
    );
    // Not a repository, or an unknown commit: empty.
    let elsewhere = tempfile::tempdir().unwrap();
    let elsewhere = elsewhere.path().to_str().unwrap();
    assert_eq!(
        untracked_files(&SystemCommands, elsewhere),
        Vec::<String>::new()
    );
    assert_eq!(
        tracked_diff_files(&SystemCommands, wt, "no-such-ref"),
        vec![]
    );
}

#[test]
fn a_rename_in_a_real_repository_carries_its_old_path() {
    let (_dir, repo) = new_repo();
    let body: String = (0..20).map(|n| format!("line {n}\n")).collect();
    std::fs::write(repo.join("old name.txt"), &body).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "add"]);
    std::fs::create_dir(repo.join("moved")).unwrap();
    git(&repo, &["mv", "old name.txt", "moved/new name.txt"]);
    let wt = repo.to_str().unwrap();
    assert_eq!(
        tracked_diff_files(&SystemCommands, wt, "HEAD"),
        vec![moved("old name.txt", "moved/new name.txt", 0, 0)]
    );
}

#[test]
fn untracked_patch_in_a_real_repository() {
    let (_dir, repo) = new_repo();
    std::fs::write(repo.join("lines.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(repo.join("empty.txt"), "").unwrap();
    let wt = repo.to_str().unwrap();

    let (patch, added) = untracked_patch(&SystemCommands, wt, "lines.txt").unwrap();
    assert_eq!(added, 3);
    assert!(
        patch.starts_with("diff --git a/lines.txt b/lines.txt\n"),
        "{patch}"
    );
    assert!(patch.contains("+one\n+two\n+three\n"), "{patch}");
    assert!(!patch.contains('\u{1b}'), "no colour codes");

    // An empty file differs from /dev/null: git prints a header and adds no line.
    let (patch, added) = untracked_patch(&SystemCommands, wt, "empty.txt").unwrap();
    assert_eq!(added, 0);
    assert!(
        patch.starts_with("diff --git a/empty.txt b/empty.txt\n"),
        "{patch}"
    );

    // A missing file prints nothing.
    assert_eq!(untracked_patch(&SystemCommands, wt, "missing.txt"), None);
}
