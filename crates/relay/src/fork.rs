//! The git layer of a fork: snapshot a worktree without touching it, install the snapshot into a
//! fresh worktree of the same repository, drop the snapshot.
//!
//! Every git call goes through [`Commands`] as `env -u GIT_DIR -u GIT_WORK_TREE -u GIT_COMMON_DIR
//! -u GIT_INDEX_FILE [GIT_INDEX_FILE=<path>] git -C <directory> …`, so a relay started from inside
//! another git command still addresses the directory it was given. All functions block.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::reads::extras::commands::{Commands, Limits};

const LIMITS: Limits = Limits {
    timeout: Duration::from_secs(60),
    max_stdout: 8 * 1024 * 1024,
};

const REF_ROOT: &str = "refs/conductor-remote/forks";

/// A snapshot left by a killed relay is pruned once it is this much older than the next one.
const STALE_MS: i64 = 86_400_000;

/// Why a fork's git step did not happen; the text is shown to the user after
/// "Could not snapshot the source workspace: " or "… its code fork failed: ".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ForkError(pub String);

/// The captured layers of a worktree, kept alive by three private refs until `release`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The source's commit: where the destination's branch is moved to.
    pub head: String,
    /// The tree of the source's index (its staged state).
    pub index_tree: String,
    /// The tree of the source's files: tracked files as they are on disk plus untracked files Git does not ignore.
    pub tree: String,
    /// `refs/conductor-remote/forks/<now_ms>-<12 hex digits>`.
    pub ref_prefix: String,
    /// The canonical common git directory; the destination must share it.
    pub common_dir: PathBuf,
    /// The canonical source worktree; never a destination.
    pub source: PathBuf,
}

fn fail(text: &str) -> ForkError {
    ForkError(text.to_owned())
}

fn path_str(path: &Path) -> Result<&str, ForkError> {
    path.to_str()
        .ok_or_else(|| fail("a path is not valid UTF-8"))
}

fn canonical(path: &Path) -> Result<PathBuf, ForkError> {
    std::fs::canonicalize(path).map_err(|_| fail("could not resolve a workspace path"))
}

/// The first argument that is not `--git-dir` or its value.
fn sub_command<'a>(args: &[&'a str]) -> &'a str {
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if arg == "--git-dir" {
            iter.next();
        } else {
            return arg;
        }
    }
    "git"
}

/// Runs `git -C <dir> <args>` (with `GIT_INDEX_FILE=<index>` when given) and returns the trimmed
/// standard output; a program that did not start, timed out, wrote too much or exited with another
/// code than 0 is an error.
fn git(
    commands: &dyn Commands,
    dir: &Path,
    index: Option<&Path>,
    args: &[&str],
) -> Result<String, ForkError> {
    let dir = path_str(dir)?;
    let index_env = match index {
        Some(path) => Some(format!("GIT_INDEX_FILE={}", path_str(path)?)),
        None => None,
    };
    let mut full: Vec<&str> = vec![
        "-u",
        "GIT_DIR",
        "-u",
        "GIT_WORK_TREE",
        "-u",
        "GIT_COMMON_DIR",
        "-u",
        "GIT_INDEX_FILE",
    ];
    if let Some(env) = &index_env {
        full.push(env);
    }
    full.extend(["git", "-C", dir]);
    full.extend_from_slice(args);
    let sub = sub_command(args);
    let out = commands
        .run("env", &full, None, LIMITS)
        .map_err(|error| ForkError(format!("git {sub} failed: {error}")))?;
    if out.code != Some(0) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr.lines().map(str::trim).find(|line| !line.is_empty());
        return Err(ForkError(match line {
            Some(line) => format!(
                "git {sub} failed: {}",
                line.chars().take(200).collect::<String>()
            ),
            None => format!("git {sub} failed"),
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// An object id Git printed, only when it is 40 or 64 lower-case hexadecimal characters.
fn object_id(text: String) -> Result<String, ForkError> {
    let valid = matches!(text.len(), 40 | 64)
        && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(text)
    } else {
        Err(fail("Git printed an unexpected object id"))
    }
}

fn is_toplevel(
    commands: &dyn Commands,
    dir: &Path,
    canonical_dir: &Path,
) -> Result<bool, ForkError> {
    let top = git(commands, dir, None, &["rev-parse", "--show-toplevel"])?;
    Ok(std::fs::canonicalize(top).is_ok_and(|top| top == canonical_dir))
}

fn common_dir_of(commands: &dyn Commands, dir: &Path) -> Result<PathBuf, ForkError> {
    let out = git(
        commands,
        dir,
        None,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    canonical(Path::new(&out))
}

fn random_hex() -> Result<String, ForkError> {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).map_err(|_| fail("could not get random bytes"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A private temporary directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn create() -> Result<Self, ForkError> {
        let path = std::env::temp_dir().join(format!("conductor-remote-fork-{}", random_hex()?));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| fail("could not create a temporary directory"))?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether `line` is `refs/conductor-remote/forks/<digits>-<name>/(head|index|worktree)` older than
/// a day at `now_ms`.
fn is_stale(line: &str, now_ms: i64) -> bool {
    let Some(rest) = line
        .strip_prefix(REF_ROOT)
        .and_then(|r| r.strip_prefix('/'))
    else {
        return false;
    };
    let Some((name, suffix)) = rest.split_once('/') else {
        return false;
    };
    if !matches!(suffix, "head" | "index" | "worktree") {
        return false;
    }
    let Some((digits, _)) = name.split_once('-') else {
        return false;
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    digits
        .parse::<i64>()
        .is_ok_and(|made| now_ms.saturating_sub(made) > STALE_MS)
}

/// Captures the staged state and the files of `worktree` as two trees and a commit, kept by three
/// refs; the worktree and its index are left as they were.
pub fn capture(
    commands: &dyn Commands,
    worktree: &Path,
    now_ms: i64,
) -> Result<Snapshot, ForkError> {
    let source = canonical(worktree)?;
    if !is_toplevel(commands, worktree, &source)? {
        return Err(fail("Git resolved the workspace to an ancestor repository"));
    }
    let common_dir = common_dir_of(commands, worktree)?;
    let head = git(
        commands,
        worktree,
        None,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .map_err(|_| fail("the source workspace has no commit to fork"))
    .and_then(object_id)?;

    for (name, marker) in [
        ("rebase", "rebase-merge"),
        ("rebase", "rebase-apply"),
        ("merge", "MERGE_HEAD"),
        ("cherry-pick", "CHERRY_PICK_HEAD"),
        ("revert", "REVERT_HEAD"),
    ] {
        let path = git(
            commands,
            worktree,
            None,
            &["rev-parse", "--git-path", marker],
        )?;
        if worktree.join(path).exists() {
            return Err(ForkError(format!(
                "the source workspace is in the middle of a Git {name}"
            )));
        }
    }
    if !git(commands, worktree, None, &["ls-files", "-u"])?.is_empty() {
        return Err(fail("the source workspace has unresolved Git conflicts"));
    }

    let listed = git(
        commands,
        worktree,
        None,
        &["for-each-ref", "--format=%(refname)", REF_ROOT],
    )?;
    for line in listed
        .lines()
        .map(str::trim)
        .filter(|l| is_stale(l, now_ms))
    {
        let _ = git(commands, worktree, None, &["update-ref", "-d", line]);
    }

    let index_tree = object_id(git(commands, worktree, None, &["write-tree"])?)?;

    let temp = TempDir::create()?;
    let alternate = temp.0.join("index");
    git(
        commands,
        worktree,
        Some(&alternate),
        &["read-tree", &index_tree],
    )?;
    git(
        commands,
        worktree,
        Some(&alternate),
        &["add", "-A", "--", "."],
    )?;
    let tree = object_id(git(commands, worktree, Some(&alternate), &["write-tree"])?)?;

    let ref_prefix = format!("{REF_ROOT}/{now_ms}-{}", random_hex()?);
    let mut made: Vec<(String, &str)> = Vec::new();
    for (suffix, object) in [("head", &head), ("index", &index_tree), ("worktree", &tree)] {
        let name = format!("{ref_prefix}/{suffix}");
        match git(commands, worktree, None, &["update-ref", &name, object]) {
            Ok(_) => made.push((name, object)),
            Err(error) => {
                for (name, object) in &made {
                    let _ = git(
                        commands,
                        worktree,
                        None,
                        &["update-ref", "-d", name, object],
                    );
                }
                return Err(error);
            }
        }
    }

    Ok(Snapshot {
        head,
        index_tree,
        tree,
        ref_prefix,
        common_dir,
        source,
    })
}

/// Whether `worktree` has `branch` checked out: `symbolic-ref HEAD` prints `refs/heads/<branch>`.
pub fn on_branch(commands: &dyn Commands, worktree: &Path, branch: &str) -> bool {
    matches!(
        git(commands, worktree, None, &["symbolic-ref", "HEAD"]),
        Ok(on) if on == format!("refs/heads/{branch}")
    )
}

/// Whether `worktree` is the checkout of `branch` that a fork waits for: a clean tree on a commit.
pub fn ready(commands: &dyn Commands, worktree: &Path, branch: &str) -> bool {
    let Ok(dir) = std::fs::canonicalize(worktree) else {
        return false;
    };
    let checks = || -> Result<bool, ForkError> {
        if !is_toplevel(commands, worktree, &dir)? {
            return Ok(false);
        }
        if !on_branch(commands, worktree, branch) {
            return Ok(false);
        }
        git(
            commands,
            worktree,
            None,
            &["rev-parse", "--verify", "HEAD^{commit}"],
        )?;
        Ok(git(commands, worktree, None, &["status", "--porcelain=v1"])?.is_empty())
    };
    checks().unwrap_or(false)
}

/// Installs `snapshot` into `worktree`, a clean fresh worktree of the same repository on `branch`:
/// the files, the staged state and the branch's commit become the source's.
pub fn materialize(
    commands: &dyn Commands,
    snapshot: &Snapshot,
    worktree: &Path,
    branch: &str,
) -> Result<(), ForkError> {
    let dir = canonical(worktree)?;
    if dir == snapshot.source {
        return Err(fail("the new workspace is the source workspace"));
    }
    if !is_toplevel(commands, worktree, &dir)? {
        return Err(fail("Git resolved the workspace to an ancestor repository"));
    }
    if common_dir_of(commands, worktree)? != snapshot.common_dir {
        return Err(fail(
            "the new workspace belongs to a different Git repository",
        ));
    }
    if !on_branch(commands, worktree, branch) {
        return Err(fail("the new workspace is on another branch"));
    }
    if !git(commands, worktree, None, &["status", "--porcelain=v1"])?.is_empty() {
        return Err(fail(
            "the new workspace changed files before the fork snapshot could be installed",
        ));
    }
    let current = object_id(git(
        commands,
        worktree,
        None,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )?)?;

    if let Err(error) = install(commands, snapshot, worktree, &current) {
        return Err(if put_back(commands, snapshot, worktree, &current) {
            error
        } else {
            ForkError(format!(
                "{error}; the new workspace may hold part of the code"
            ))
        });
    }
    let _ = git(
        commands,
        worktree,
        None,
        &["update-index", "-q", "--refresh"],
    );
    Ok(())
}

/// The install itself: the snapshot's files, then its staged state, then the branch moved from
/// `current` to the source's commit. A failure leaves `worktree` part way through.
fn install(
    commands: &dyn Commands,
    snapshot: &Snapshot,
    worktree: &Path,
    current: &str,
) -> Result<(), ForkError> {
    git(
        commands,
        worktree,
        None,
        &["read-tree", "--reset", "-u", &snapshot.tree],
    )?;
    if git(commands, worktree, None, &["write-tree"])? != snapshot.tree {
        return Err(fail("Git did not install the complete fork snapshot"));
    }
    git(
        commands,
        worktree,
        None,
        &["read-tree", "--reset", &snapshot.index_tree],
    )?;
    if git(commands, worktree, None, &["write-tree"])? != snapshot.index_tree {
        return Err(fail("Git did not restore the fork's staged state"));
    }
    git(
        commands,
        worktree,
        None,
        &["update-ref", "HEAD", &snapshot.head, current],
    )?;
    Ok(())
}

/// Puts `worktree` back on `current` after a failed install, best effort: the index is made the
/// snapshot's files first, so that the reset removes the ones `current` does not have. Whether
/// the worktree is a clean checkout again.
fn put_back(commands: &dyn Commands, snapshot: &Snapshot, worktree: &Path, current: &str) -> bool {
    let _ = git(
        commands,
        worktree,
        None,
        &["read-tree", "--reset", &snapshot.tree],
    );
    git(
        commands,
        worktree,
        None,
        &["read-tree", "--reset", "-u", current],
    )
    .is_ok()
        && matches!(
            git(commands, worktree, None, &["status", "--porcelain=v1"]),
            Ok(out) if out.is_empty()
        )
}

/// Removes the snapshot's three refs; all are tried, the first error is returned.
pub fn release(commands: &dyn Commands, snapshot: &Snapshot) -> Result<(), ForkError> {
    let common = path_str(&snapshot.common_dir)?;
    let mut first = None;
    for (suffix, object) in [
        ("head", &snapshot.head),
        ("index", &snapshot.index_tree),
        ("worktree", &snapshot.tree),
    ] {
        let name = format!("{}/{suffix}", snapshot.ref_prefix);
        let result = git(
            commands,
            &snapshot.common_dir,
            None,
            &["--git-dir", common, "update-ref", "-d", &name, object],
        );
        if let Err(error) = result {
            first.get_or_insert(error);
        }
    }
    first.map_or(Ok(()), Err)
}
