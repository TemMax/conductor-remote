#[path = "support/seed_review.rs"]
#[allow(dead_code)]
mod seed_review;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use conductor_remote::reads::extras::commands::SystemCommands;
use conductor_remote::reads::review::basis::DiffFile;
use conductor_remote::reads::review::diff::{
    list_source_files, workspace_diff, workspace_file_diff, WorkspaceDiff, WorkspaceFileDiff,
};
use conductor_remote::reads::review::live::LiveTarget;
use conductor_remote::reads::Reads;
use conductor_remote::testing::FakeCommands;
use serde_json::Value;
use support::TestDb;

// ------------------------------------------------------------------ helpers

/// Every git call of these tests commits and dates as the same person, at the same time.
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

fn git(dir: &Path, args: &[&str]) {
    git_output(dir, args);
}

/// A repository on `main` in a canonical temporary path.
struct Repo {
    _dir: tempfile::TempDir,
    path: PathBuf,
    text: String,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("work");
        std::fs::create_dir(&path).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        let text = path.to_str().unwrap().to_owned();
        Self {
            _dir: dir,
            path,
            text,
        }
    }

    fn git(&self, args: &[&str]) {
        git(&self.path, args);
    }

    fn write(&self, file: &str, content: impl AsRef<[u8]>) {
        let target = self.path.join(file);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }

    /// Stages everything and commits it; the new commit's id.
    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        git_output(&self.path, &["rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }

    fn rename(&self, from: &str, to: &str) {
        std::fs::create_dir_all(self.path.join(to).parent().unwrap()).unwrap();
        self.git(&["mv", "--", from, to]);
    }

    fn diff(&self, base: &str) -> WorkspaceDiff {
        workspace_diff(&SystemCommands, &self.text, base)
    }

    fn file_diff(&self, base: &str, requested: &str) -> Option<WorkspaceFileDiff> {
        workspace_file_diff(&SystemCommands, &self.text, base, requested)
    }
}

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

/// The per-file sections of a patch, in order.
fn sections(patch: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = patch
        .match_indices("diff --git ")
        .map(|(at, _)| at)
        .filter(|at| *at == 0 || patch.as_bytes()[at - 1] == b'\n')
        .collect();
    starts.push(patch.len());
    starts.windows(2).map(|w| &patch[w[0]..w[1]]).collect()
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Twenty numbered lines.
fn twenty_lines() -> String {
    (0..20).map(|i| format!("const line{i} = {i}\n")).collect()
}

// ------------------------------------------------------------------ renames

#[test]
fn keeps_real_paths_and_rename_patches() {
    let cases = [
        ("old.ts", "new.ts"),
        ("src/search.ts", "src/search/coordinator.ts"),
        ("src/old/file.ts", "src/new/file.ts"),
        ("src/old.ts", "src/{literal => name}.ts"),
        ("src/ældre\t\"old\"\nname.ts", "src/nye\t\"new\"\nname.ts"),
        (":old[1].ts", ":new[1].ts"),
    ];
    for (old, new) in cases {
        let repo = Repo::new();
        repo.write(old, twenty_lines());
        let base = repo.commit("fixture");
        repo.rename(old, new);

        // Both an indexed rename and a committed rename compare against the same base.
        for committed in [false, true] {
            if committed {
                repo.commit("fixture");
            }
            let diff = repo.diff(&base);
            assert_eq!(diff.files, vec![moved(old, new, 0, 0)], "{old} -> {new}");
            let selected = repo
                .file_diff(&base, new)
                .unwrap_or_else(|| panic!("{new}: no file diff"));
            assert_eq!(selected.path, new);
            assert_eq!(selected.patch, diff.patch, "{old} -> {new}");
            assert!(selected.patch.contains("rename from "));
            assert!(selected.patch.contains("rename to "));
            assert!(!selected.patch.contains("new file mode"));
        }
    }
}

#[test]
fn groups_an_edited_move_under_its_destination_and_keeps_the_selected_patch_aligned() {
    let old = "src/search.ts";
    let new = "src/search/coordinator.ts";
    let repo = Repo::new();
    repo.write(old, twenty_lines());
    repo.write("a-first.txt", "first before\n");
    repo.write("z-last.txt", "last before\n");
    let base = repo.commit("fixture");
    repo.rename(old, new);
    repo.write(
        new,
        twenty_lines().replace("const line10 = 10", "const line10 = 42"),
    );
    repo.write("a-first.txt", "first after\n");
    repo.write("z-last.txt", "last after\n");

    let diff = repo.diff(&base);
    assert!(diff.files.contains(&moved(old, new, 1, 1)));
    let position = diff.files.iter().position(|f| f.path == new).unwrap();
    let aggregate = sections(&diff.patch)[position];
    let selected = repo.file_diff(&base, new).unwrap();
    assert_eq!(selected.patch.trim(), aggregate.trim());
    assert!(selected.patch.contains(&format!("rename from {old}")));
    assert!(selected.patch.contains("-const line10 = 10"));
    assert!(selected.patch.contains("+const line10 = 42"));
    assert!(!selected.patch.contains("a-first.txt"));
    assert!(!selected.patch.contains("z-last.txt"));
}

#[test]
fn keeps_binary_renames_as_a_single_zero_line_change() {
    let mut binary = b"\0".to_vec();
    binary.extend("unchanged binary content".repeat(32).bytes());
    let repo = Repo::new();
    repo.write("old.bin", &binary);
    let base = repo.commit("fixture");
    repo.rename("old.bin", "new.bin");
    binary.extend(b"new chunk");
    repo.write("new.bin", &binary);

    let diff = repo.diff(&base);
    assert_eq!(diff.files, vec![moved("old.bin", "new.bin", 0, 0)]);
    assert_eq!(repo.file_diff(&base, "new.bin").unwrap().patch, diff.patch);
}

#[test]
fn preserves_literal_tracked_and_untracked_names_alongside_a_rename() {
    let tracked = "tracked\t\"æ\"\n{a => b}.ts";
    let untracked = "untracked\t\"ø\"\n{a => b}.ts";
    let repo = Repo::new();
    repo.write("old.ts", twenty_lines());
    repo.write(tracked, "before\n");
    let base = repo.commit("fixture");
    repo.rename("old.ts", "new.ts");
    repo.write(tracked, "after\n");
    repo.write(untracked, "new source\n");

    let diff = repo.diff(&base);
    assert_eq!(diff.files.len(), 3);
    assert!(diff.files.contains(&file(tracked, 1, 1)));
    assert!(diff.files.contains(&file(untracked, 1, 0)));
    let aggregate = sections(&diff.patch);
    assert_eq!(aggregate.len(), 3);
    for (position, changed) in diff.files.iter().enumerate() {
        let selected = repo.file_diff(&base, &changed.path).unwrap();
        assert_eq!(selected.path, changed.path);
        assert_eq!(selected.patch.trim(), aggregate[position].trim());
    }
}

// ------------------------------------------------------------------ one file

#[test]
fn reads_a_selected_file_beyond_the_aggregate_patch_limit() {
    let repo = Repo::new();
    repo.write("tracked.txt", "alpha\nbeta\ngamma\n");
    repo.commit("initial");
    repo.write("a-large.txt", "before\n");
    repo.write("z-selected.txt", "before\n");
    repo.commit("more files");

    repo.write("a-large.txt", "changed line\n".repeat(40_000));
    repo.write("z-selected.txt", "after\n");

    let aggregate = repo.diff("HEAD");
    assert!(aggregate.truncated);
    assert!(aggregate.files.iter().any(|f| f.path == "z-selected.txt"));
    assert!(!aggregate
        .patch
        .contains("diff --git a/z-selected.txt b/z-selected.txt"));

    let selected = repo.file_diff("HEAD", "z-selected.txt").unwrap();
    assert_eq!(selected.path, "z-selected.txt");
    assert!(selected
        .patch
        .contains("diff --git a/z-selected.txt b/z-selected.txt"));
    assert!(selected.patch.contains("-before"));
    assert!(selected.patch.contains("+after"));
}

#[test]
fn reads_an_exact_untracked_file_without_allowing_paths_outside_the_worktree() {
    let repo = Repo::new();
    repo.write("tracked.txt", "alpha\n");
    repo.commit("initial");
    repo.write("new file.txt", "one\ntwo\n");

    let selected = repo.file_diff("HEAD", "new file.txt").unwrap();
    assert_eq!(selected.path, "new file.txt");
    assert!(selected
        .patch
        .contains("diff --git a/new file.txt b/new file.txt"));
    assert!(selected.patch.contains("+one"));
    assert_eq!(repo.file_diff("HEAD", "tracked.txt"), None);
    assert_eq!(repo.file_diff("HEAD", "../outside.txt"), None);
}

#[test]
fn rejects_a_path_that_is_empty_absolute_or_outside_the_worktree() {
    let repo = Repo::new();
    repo.write("tracked.txt", "alpha\n");
    repo.commit("initial");
    repo.write("new.txt", "one\n");
    let outside = std::fs::canonicalize(&repo.path).unwrap();
    let absolute = format!("{}/new.txt", outside.display());

    for requested in [
        "",
        "new.txt\0",
        &absolute,
        "/new.txt",
        ".",
        "./",
        "..",
        "../",
        "../new.txt",
        "a/../../new.txt",
        "../other/new.txt",
    ] {
        assert_eq!(repo.file_diff("HEAD", requested), None, "{requested:?}");
    }
}

#[test]
fn normalises_the_requested_path_lexically() {
    let repo = Repo::new();
    repo.write("tracked.txt", "alpha\n");
    repo.commit("initial");
    repo.write("b", "one\n");
    repo.write("a/b", "two\n");
    let name = repo.path.file_name().unwrap().to_str().unwrap().to_owned();

    for (requested, expected) in [
        (format!("../{name}/b"), "b"),
        ("a/../b".to_owned(), "b"),
        ("./a//b".to_owned(), "a/b"),
        ("a/./b".to_owned(), "a/b"),
        ("a/b/".to_owned(), "a/b"),
    ] {
        let selected = repo
            .file_diff("HEAD", &requested)
            .unwrap_or_else(|| panic!("{requested}: no file diff"));
        assert_eq!(selected.path, expected, "{requested}");
        assert!(selected.patch.contains(&format!("b/{expected}")));
    }
}

#[test]
fn an_ignored_or_unchanged_file_names_no_changed_file() {
    let repo = Repo::new();
    repo.write(".gitignore", "ignored.txt\n");
    repo.write("tracked.txt", "alpha\n");
    repo.commit("initial");
    repo.write("ignored.txt", "secret\n");

    assert_eq!(repo.file_diff("HEAD", "ignored.txt"), None);
    assert_eq!(repo.file_diff("HEAD", "tracked.txt"), None);
    assert_eq!(repo.file_diff("HEAD", "missing.txt"), None);
    // A pathspec is no path: the literal form keeps it from matching anything.
    assert_eq!(repo.file_diff("HEAD", "*.txt"), None);
}

// ------------------------------------------------------------------ the aggregate

#[test]
fn dirty_and_unpushed() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    repo.commit("initial");

    // No remote-tracking branch: nothing can be unpushed.
    let diff = repo.diff("main");
    assert!(!diff.dirty);
    assert!(!diff.unpushed);

    let origin = tempfile::tempdir().unwrap();
    let origin_path = origin.path().canonicalize().unwrap();
    git(&origin_path, &["init", "-q", "--bare", "-b", "main"]);
    repo.git(&["remote", "add", "origin", origin_path.to_str().unwrap()]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    let diff = repo.diff("main");
    assert_eq!(diff.base, "origin/main");
    assert!(!diff.dirty);
    assert!(!diff.unpushed);

    repo.write("a.txt", "two\n");
    let diff = repo.diff("main");
    assert!(diff.dirty);
    assert!(!diff.unpushed);

    repo.commit("local");
    let diff = repo.diff("main");
    assert!(!diff.dirty);
    assert!(diff.unpushed);

    repo.write("untracked.txt", "x\n");
    let diff = repo.diff("main");
    assert!(diff.dirty);
    assert!(diff.unpushed);
}

#[test]
fn counts_committed_worktree_and_untracked_lines() {
    let repo = Repo::new();
    repo.write("tracked.txt", "alpha\nbeta\ngamma\n");
    repo.commit("initial");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("tracked.txt", "alpha changed\nbeta\ngamma\n");
    repo.commit("edit");
    repo.write("tracked.txt", "alpha changed\nbeta\ngamma\ndelta\n");
    repo.write("new.txt", "one\ntwo\n");

    let diff = repo.diff("main");
    assert_eq!(diff.base, "main");
    assert!(diff.merge_base.is_some());
    assert_eq!(
        diff.files,
        vec![file("tracked.txt", 2, 1), file("new.txt", 2, 0)]
    );
    assert!(!diff.truncated);
    assert!(diff
        .patch
        .contains("diff --git a/tracked.txt b/tracked.txt"));
    assert!(diff.patch.contains("diff --git a/new.txt b/new.txt"));
}

#[test]
fn an_empty_untracked_file_is_listed_with_no_added_lines() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    repo.write("empty.txt", "");
    repo.write("full.txt", "x\n");

    let diff = repo.diff("HEAD");
    assert_eq!(
        diff.files,
        vec![file("empty.txt", 0, 0), file("full.txt", 1, 0)]
    );
    assert!(repo.file_diff("HEAD", "empty.txt").is_some());
}

/// A new file of `lines` lines of 50 emoji each (102 UTF-16 units a line, 200 bytes), preceded
/// by a line of `pad` ASCII characters.
fn write_big(repo: &Repo, lines: usize, pad: usize) {
    let mut content = format!("{}\n", "a".repeat(pad));
    let line = format!("{}\n", "\u{1F600}".repeat(50));
    for _ in 0..lines {
        content.push_str(&line);
    }
    repo.write("big.txt", content);
}

#[test]
fn cuts_the_patch_at_400000_utf16_units() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    repo.commit("initial");

    // One more ASCII character moves every emoji one unit along; one of the two paddings puts
    // the cut between the two halves of a surrogate pair.
    let mut cut_in_a_pair = None;
    for pad in [0, 1] {
        write_big(&repo, 5000, pad);
        let full = repo.file_diff("HEAD", "big.txt").unwrap().patch;
        let all: Vec<u16> = full.encode_utf16().collect();
        assert!(all.len() > 400_000);
        if (0xD800..0xDC00).contains(&all[399_999]) {
            cut_in_a_pair = Some(full);
            break;
        }
    }
    let full = cut_in_a_pair.expect("one padding cuts a pair");
    let all: Vec<u16> = full.encode_utf16().collect();
    assert_ne!(all.len(), full.len(), "units and bytes differ");

    let diff = repo.diff("HEAD");
    assert!(diff.truncated);
    let kept = String::from_utf16_lossy(&all[..400_000]);
    assert!(kept.ends_with('\u{FFFD}'));
    assert_eq!(
        diff.patch,
        format!("{kept}\n\n… diff truncated ({} bytes) …", all.len())
    );
    // The patch of one file is never cut.
    assert_eq!(repo.file_diff("HEAD", "big.txt").unwrap().patch, full);
}

#[test]
fn a_patch_of_exactly_400000_units_is_kept_whole() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    repo.commit("initial");

    write_big(&repo, 3900, 0);
    let short = units(&repo.file_diff("HEAD", "big.txt").unwrap().patch);
    assert!(short < 400_000);

    write_big(&repo, 3900, 400_000 - short);
    let diff = repo.diff("HEAD");
    assert_eq!(units(&diff.patch), 400_000);
    assert!(!diff.truncated);

    write_big(&repo, 3900, 400_001 - short);
    let diff = repo.diff("HEAD");
    assert!(diff.truncated);
    assert!(diff
        .patch
        .ends_with("\n\n… diff truncated (400001 bytes) …"));
}

#[test]
fn reads_the_patches_of_the_first_500_untracked_files_only() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    for n in 0..600 {
        repo.write(&format!("f{n:03}.txt"), "x\n");
    }

    let diff = repo.diff("HEAD");
    assert_eq!(diff.files.len(), 500);
    assert_eq!(diff.files[0], file("f000.txt", 1, 0));
    assert_eq!(diff.files[499], file("f499.txt", 1, 0));
    assert!(!diff.patch.contains("f500.txt"));
    // A file beyond the limit can still be read on its own.
    assert!(repo.file_diff("HEAD", "f599.txt").is_some());
}

// ------------------------------------------------------------------ scripted git

#[test]
fn runs_these_git_commands_in_this_order() {
    let wt = "/wt";
    let commands = FakeCommands::new();
    let on = |args: &[&str], code: i32, stdout: &str| {
        let mut prefix = vec!["-C", wt];
        prefix.extend_from_slice(args);
        commands.on("git", &prefix, code, stdout);
    };
    on(&["rev-parse"], 0, "abc\n");
    on(&["merge-base"], 0, "def\n");
    on(&["diff", "--numstat"], 0, "1\t0\ta.txt\0");
    on(&["diff", "--end-of-options", "def"], 0, "TRACKED\n");
    on(&["ls-files"], 0, "u.txt\0v.txt\0");
    on(
        &[
            "diff",
            "--no-index",
            "--no-color",
            "--",
            "/dev/null",
            "u.txt",
        ],
        1,
        "+++ b/u.txt\n+one\n+two\n",
    );
    on(
        &[
            "diff",
            "--no-index",
            "--no-color",
            "--",
            "/dev/null",
            "v.txt",
        ],
        1,
        "",
    );
    on(&["status"], 0, " M a.txt\n");
    on(&["rev-list"], 0, "2\n");

    let diff = workspace_diff(&commands, wt, "main");
    assert_eq!(
        diff,
        WorkspaceDiff {
            base: "origin/main".to_owned(),
            merge_base: Some("def".to_owned()),
            files: vec![file("a.txt", 1, 0), file("u.txt", 2, 0)],
            patch: "TRACKED\n+++ b/u.txt\n+one\n+two\n".to_owned(),
            truncated: false,
            dirty: true,
            unpushed: true,
        }
    );
    let calls: Vec<Vec<String>> = commands.calls().into_iter().map(|c| c.args).collect();
    let expected: Vec<Vec<&str>> = vec![
        vec![
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            "origin/main^{commit}",
        ],
        vec!["merge-base", "--end-of-options", "origin/main", "HEAD"],
        vec!["diff", "--numstat", "-z", "--end-of-options", "def"],
        vec!["diff", "--end-of-options", "def"],
        vec!["ls-files", "--others", "--exclude-standard", "-z"],
        vec![
            "diff",
            "--no-index",
            "--no-color",
            "--",
            "/dev/null",
            "u.txt",
        ],
        vec![
            "diff",
            "--no-index",
            "--no-color",
            "--",
            "/dev/null",
            "v.txt",
        ],
        vec!["status", "--porcelain"],
        vec!["rev-list", "--count", "@{upstream}..HEAD"],
    ];
    assert_eq!(calls.len(), expected.len());
    for (call, expected) in calls.iter().zip(&expected) {
        assert_eq!(&call[..2], ["-C", wt]);
        assert_eq!(&call[2..], expected.as_slice());
    }
}

#[test]
fn a_failed_git_call_gives_its_empty_answer() {
    // Nothing runs at all.
    let nothing = FakeCommands::new();
    assert_eq!(
        workspace_diff(&nothing, "/wt", "main"),
        WorkspaceDiff {
            base: "main".to_owned(),
            merge_base: None,
            files: vec![],
            patch: String::new(),
            truncated: false,
            dirty: false,
            unpushed: false,
        }
    );

    // The tracked patch fails (a timeout, as too much output does): the files stay.
    let commands = FakeCommands::new();
    commands.on("git", &["-C", "/wt", "rev-parse"], 0, "");
    commands.on("git", &["-C", "/wt", "merge-base"], 0, "def\n");
    commands.on(
        "git",
        &["-C", "/wt", "diff", "--numstat"],
        0,
        "1\t0\ta.txt\0",
    );
    commands.fail("git", &["-C", "/wt", "diff", "--end-of-options", "def"]);
    commands.on("git", &["-C", "/wt", "status"], 0, "  \n");
    commands.on("git", &["-C", "/wt", "rev-list"], 0, "0\n");
    let diff = workspace_diff(&commands, "/wt", "main");
    assert_eq!(diff.files, vec![file("a.txt", 1, 0)]);
    assert_eq!(diff.patch, "");
    assert!(!diff.dirty);
    assert!(!diff.unpushed);

    // A count that is not a number above zero is no unpushed commit.
    commands.on("git", &["-C", "/wt", "rev-list"], 0, "many\n");
    assert!(!workspace_diff(&commands, "/wt", "main").unpushed);
    commands.on("git", &["-C", "/wt", "rev-list"], 128, "3\n");
    assert!(!workspace_diff(&commands, "/wt", "main").unpushed);
}

// ------------------------------------------------------------------ source files

#[test]
fn lists_previewable_source_files() {
    let repo = Repo::new();
    repo.write(".gitignore", "ignored.txt\n.context/\nbuild/\n");
    repo.write("src/a.rs", "fn main() {}\n");
    repo.write("data.bin", "binary\n");
    repo.commit("initial");
    repo.write("notes.md", "notes\n");
    repo.write("image.png", "png\n");
    repo.write("ignored.txt", "ignored\n");
    repo.write("build/out.js", "out\n");
    repo.write(".context/plan.md", "plan\n");
    repo.write(".context/deep/more.md", "more\n");
    repo.write("sub/.context/nested.md", "nested\n");

    let listed = list_source_files(&SystemCommands, &repo.text);
    assert!(!listed.truncated);
    let mut files = listed.files;
    files.sort();
    assert_eq!(
        files,
        vec![
            ".context/deep/more.md",
            ".context/plan.md",
            "notes.md",
            "src/a.rs",
        ]
    );
}

#[test]
fn lists_at_most_20000_source_files() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    for n in 0..20_000 {
        repo.write(&format!("f{n:05}.txt"), "x\n");
    }

    let listed = list_source_files(&SystemCommands, &repo.text);
    assert!(listed.truncated);
    assert_eq!(listed.files.len(), 20_000);

    std::fs::remove_file(repo.path.join("f19999.txt")).unwrap();
    let listed = list_source_files(&SystemCommands, &repo.text);
    assert!(!listed.truncated);
    assert_eq!(listed.files.len(), 20_000);
}

#[test]
fn a_failed_listing_gives_no_files() {
    let nothing = FakeCommands::new();
    let listed = list_source_files(&nothing, "/wt");
    assert!(listed.files.is_empty());
    assert!(!listed.truncated);

    let outside = tempfile::tempdir().unwrap();
    let listed = list_source_files(&SystemCommands, outside.path().to_str().unwrap());
    assert!(listed.files.is_empty());
    assert!(!listed.truncated);
}

// ------------------------------------------------------------------ live_target

fn live_setup() -> (TestDb, Reads) {
    let test = TestDb::new();
    let repo_root = test.root().join("_repos").join(seed_review::REPO);
    seed_review::seed(&test.conn(), repo_root.to_str().unwrap());
    let worktree = test.root().join(seed_review::REPO).join("rev-live-dir");
    std::fs::create_dir_all(&worktree).unwrap();
    git(&worktree, &["init", "-q", "-b", "main"]);
    let reads = Reads::new(test.db(), test.root());
    (test, reads)
}

#[test]
fn a_live_workspace_has_a_target() {
    let (test, reads) = live_setup();
    let worktree = test.root().join("rev-repo").join("rev-live-dir");
    assert_eq!(
        reads.live_target(seed_review::LIVE).unwrap(),
        Some(LiveTarget {
            worktree: Some(worktree.to_string_lossy().into_owned()),
            base_branch: "main".to_owned(),
        })
    );
}

#[test]
fn an_archived_or_unknown_workspace_has_none() {
    let (_test, reads) = live_setup();
    assert_eq!(reads.live_target(seed_review::ARCHIVED).unwrap(), None);
    assert_eq!(reads.live_target("rev-unknown").unwrap(), None);
    assert_eq!(reads.live_target("").unwrap(), None);
}

#[test]
fn a_workspace_without_a_worktree_has_a_target_without_one() {
    let (_test, reads) = live_setup();
    assert_eq!(
        reads.live_target(seed_review::NO_WORKTREE).unwrap(),
        Some(LiveTarget {
            worktree: None,
            base_branch: "main".to_owned(),
        })
    );
}

#[test]
fn only_ready_and_setting_up_are_live_states() {
    let (test, reads) = live_setup();
    let conn = test.conn();
    for (state, live) in [
        ("ready", true),
        ("setting_up", true),
        ("active", false),
        ("archived", false),
    ] {
        conn.execute(
            "UPDATE workspaces SET state = ?1 WHERE id = ?2",
            rusqlite::params![state, seed_review::ARCHIVED],
        )
        .unwrap();
        assert_eq!(
            reads.live_target(seed_review::ARCHIVED).unwrap().is_some(),
            live,
            "{state}"
        );
    }
}

#[test]
fn the_base_branch_is_the_intended_target_then_the_default_then_main() {
    let (test, reads) = live_setup();
    let conn = test.conn();
    let base = |reads: &Reads| {
        reads
            .live_target(seed_review::LIVE)
            .unwrap()
            .unwrap()
            .base_branch
    };
    let set = |target: Option<&str>, default: Option<&str>| {
        conn.execute(
            "UPDATE workspaces SET intended_target_branch = ?1 WHERE id = ?2",
            rusqlite::params![target, seed_review::LIVE],
        )
        .unwrap();
        conn.execute(
            "UPDATE repos SET default_branch = ?1 WHERE id = ?2",
            rusqlite::params![default, seed_review::REPO],
        )
        .unwrap();
    };

    set(Some("release/2"), Some("trunk"));
    assert_eq!(base(&reads), "release/2");
    set(Some(""), Some("trunk"));
    assert_eq!(base(&reads), "trunk");
    set(None, Some("trunk"));
    assert_eq!(base(&reads), "trunk");
    set(None, Some(""));
    assert_eq!(base(&reads), "main");
    set(None, None);
    assert_eq!(base(&reads), "main");
}

// ------------------------------------------------------------------ the golden

fn golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/tests/contract/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// A moved and edited file, an edited file and an untracked file, on top of a commit of `main`.
fn golden_repo() -> Repo {
    let repo = Repo::new();
    repo.write("src/search.ts", twenty_lines());
    repo.write("keep.txt", "keep\n");
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.rename("src/search.ts", "src/search/coordinator.ts");
    repo.write(
        "src/search/coordinator.ts",
        twenty_lines().replace("const line10 = 10", "const line10 = 42"),
    );
    repo.commit("move");
    repo.write("keep.txt", "keep\nmore\n");
    repo.write("notes.txt", "one\ntwo\n");
    repo
}

#[test]
fn golden_review_diff() {
    let repo = golden_repo();
    let actual = serde_json::to_value(repo.diff("main")).unwrap();
    assert_eq!(actual, golden("review-diff.json"));
}
