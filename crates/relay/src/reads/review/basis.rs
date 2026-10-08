//! The git helpers every review read is built on: what a diff is taken against, the changed
//! files git lists, and the patch of an untracked file. None of them returns an error: a git
//! call that cannot run, or that exits with an unexpected code, gives the empty result.

use std::time::Duration;

use super::super::extras::commands::{CommandError, Commands, Limits, Output};

/// What a git call may take and print.
pub const GIT_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(15),
    max_stdout: 8 * 1024 * 1024,
};

/// What the patch of one untracked file may take and print.
pub const UNTRACKED_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(10),
    max_stdout: 8 * 1024 * 1024,
};

/// How many untracked files a caller reads patches for; the cut is the caller's to make.
pub const MAX_UNTRACKED_FILES: usize = 500;

/// What a diff is taken against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffBasis {
    /// The resolved base ref: `origin/<base>` or `<base>` when it names a commit, else `<base>` as given.
    pub base: String,
    /// `git merge-base <base> HEAD`, trimmed; `None` when it fails or prints nothing.
    pub merge_base: Option<String>,
    /// The merge base when there is one, else `base`.
    pub against: String,
}

/// One changed file: the web app's `DiffFile` (`oldPath` only for a rename or copy).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DiffFile {
    pub path: String,
    #[serde(rename = "oldPath", skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub added: i64,
    pub removed: i64,
}

fn git(
    commands: &dyn Commands,
    worktree: &str,
    args: &[&str],
    limits: Limits,
) -> Result<Output, CommandError> {
    let mut full = vec!["-C", worktree];
    full.extend_from_slice(args);
    commands.run("git", &full, None, limits)
}

/// The output of a git call that exited 0; `None` for any other outcome.
fn git_ok(commands: &dyn Commands, worktree: &str, args: &[&str]) -> Option<Vec<u8>> {
    match git(commands, worktree, args, GIT_LIMITS) {
        Ok(Output {
            code: Some(0),
            stdout,
            ..
        }) => Some(stdout),
        _ => None,
    }
}

/// The base ref (`origin/<base>` or `<base>`, whichever names a commit first, else `<base>`), its
/// merge base with `HEAD`, and the commit a diff is taken against.
pub fn diff_basis(commands: &dyn Commands, worktree: &str, base_branch: &str) -> DiffBasis {
    let base = [format!("origin/{base_branch}"), base_branch.to_owned()]
        .into_iter()
        .find(|candidate| {
            let spec = format!("{candidate}^{{commit}}");
            git_ok(
                commands,
                worktree,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &spec,
                ],
            )
            .is_some()
        })
        .unwrap_or_else(|| base_branch.to_owned());
    let merge_base = git_ok(
        commands,
        worktree,
        &["merge-base", "--end-of-options", &base, "HEAD"],
    )
    .map(|stdout| String::from_utf8_lossy(&stdout).trim().to_owned())
    .filter(|printed| !printed.is_empty());
    let against = merge_base.clone().unwrap_or_else(|| base.clone());
    DiffBasis {
        base,
        merge_base,
        against,
    }
}

/// Parses `git diff --numstat -z` output.
pub fn parse_numstat_z(output: &[u8]) -> Vec<DiffFile> {
    let mut records = output.split(|byte| *byte == 0);
    let text = |record: &[u8]| String::from_utf8_lossy(record).into_owned();
    let count = |column: &str| -> i64 {
        if column.is_empty() || !column.bytes().all(|b| b.is_ascii_digit()) {
            0
        } else {
            column.parse().unwrap_or(0)
        }
    };
    let mut files = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let record = text(record);
        let mut columns = record.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) =
            (columns.next(), columns.next(), columns.next())
        else {
            continue;
        };
        let (added, removed) = (count(added), count(removed));
        if !path.is_empty() {
            files.push(DiffFile {
                path: path.to_owned(),
                old_path: None,
                added,
                removed,
            });
            continue;
        }
        // A rename or copy: the old and the new path follow as records of their own.
        let (Some(old_path), Some(new_path)) = (records.next(), records.next()) else {
            break;
        };
        if old_path.is_empty() || new_path.is_empty() {
            break;
        }
        files.push(DiffFile {
            path: text(new_path),
            old_path: Some(text(old_path)),
            added,
            removed,
        });
    }
    files
}

/// `git diff --numstat -z <against>`, parsed; empty on any failure.
pub fn tracked_diff_files(commands: &dyn Commands, worktree: &str, against: &str) -> Vec<DiffFile> {
    git_ok(
        commands,
        worktree,
        &["diff", "--numstat", "-z", "--end-of-options", against],
    )
    .map(|stdout| parse_numstat_z(&stdout))
    .unwrap_or_default()
}

/// `git ls-files --others --exclude-standard -z`, all of them, in git's order; empty on any failure.
pub fn untracked_files(commands: &dyn Commands, worktree: &str) -> Vec<String> {
    git_ok(
        commands,
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .map(|stdout| {
        stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect()
    })
    .unwrap_or_default()
}

/// The patch git gives an untracked file and the number of lines it adds, or `None` when the patch is empty.
pub fn untracked_patch(
    commands: &dyn Commands,
    worktree: &str,
    path: &str,
) -> Option<(String, i64)> {
    // `--no-index` exits 1 whenever the two sides differ, so what it printed counts whatever the code.
    let output = git(
        commands,
        worktree,
        &["diff", "--no-index", "--no-color", "--", "/dev/null", path],
        UNTRACKED_LIMITS,
    )
    .ok()?;
    if output.stdout.is_empty() {
        return None;
    }
    let patch = String::from_utf8_lossy(&output.stdout).into_owned();
    let added = patch
        .split('\n')
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .count();
    Some((patch, i64::try_from(added).unwrap_or(i64::MAX)))
}
