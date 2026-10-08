//! Added and removed lines of a worktree, computed in the background.

use std::sync::Arc;
use std::time::Duration;

use super::commands::{CommandError, Commands, Limits, Output};
use super::swr::Swr;
use super::Shared;
use crate::reads::workspaces::ChangeStats;

/// A workspace that is working is looked at again after this long.
const WORKING_STALE: Duration = Duration::from_secs(5);
/// Any other workspace, after this long: a safety net for edits made outside its agent.
const IDLE_STALE: Duration = Duration::from_secs(60);
/// How many untracked files are counted; a tree with more is cut off here.
const MAX_UNTRACKED_FILES: usize = 500;
/// What every git call may take and print.
const GIT_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(15),
    max_stdout: 8 * 1024 * 1024,
};

/// `(worktree, base branch)`.
type Key = (String, String);
/// The `updated_at` of the workspace the refresh was started for, and what it found: `None` when
/// a command could not run.
type Stored = (String, Option<ChangeStats>);

/// Source of change stats.
pub struct ChangeStatsSource {
    shared: Arc<Shared>,
    cache: Swr<Key, Stored>,
}

impl ChangeStatsSource {
    pub fn new(shared: Arc<Shared>) -> Self {
        let cache = Swr::new(shared.pool.clone(), shared.revision.clone());
        Self { shared, cache }
    }

    /// Last known added and removed lines of a worktree against its base branch: `None` before the
    /// first refresh finished and when it failed. A stale value is returned at once while one
    /// refresh runs in the background.
    pub fn get(
        &self,
        worktree: &str,
        base_branch: &str,
        workspace_updated_at: &str,
        working: bool,
    ) -> Option<ChangeStats> {
        let key = (worktree.to_owned(), base_branch.to_owned());
        let commands = self.shared.commands.clone();
        let updated_at = workspace_updated_at.to_owned();
        let (worktree, base) = key.clone();
        self.cache
            .get(
                &key,
                |entry| {
                    is_stale(
                        &entry.value.0,
                        workspace_updated_at,
                        entry.at.elapsed(),
                        working,
                    )
                },
                move || {
                    let stats = compute(commands.as_ref(), &worktree, &base).ok();
                    (updated_at, stats)
                },
            )
            .and_then(|(_, stats)| stats)
    }
}

/// Whether an entry must be refreshed: its workspace changed since, or it is older than 5 seconds
/// while `working`, or older than 60 seconds otherwise.
fn is_stale(
    stored_updated_at: &str,
    workspace_updated_at: &str,
    age: Duration,
    working: bool,
) -> bool {
    let limit = if working { WORKING_STALE } else { IDLE_STALE };
    stored_updated_at != workspace_updated_at || age > limit
}

fn git(commands: &dyn Commands, worktree: &str, args: &[&str]) -> Result<Output, CommandError> {
    let mut full = vec!["-C", worktree];
    full.extend_from_slice(args);
    commands.run("git", &full, None, GIT_LIMITS)
}

/// The base ref: `origin/<base>` or `<base>`, whichever names a commit first; else `<base>`.
fn resolve_base(
    commands: &dyn Commands,
    worktree: &str,
    base: &str,
) -> Result<String, CommandError> {
    for candidate in [format!("origin/{base}"), base.to_owned()] {
        let spec = format!("{candidate}^{{commit}}");
        let out = git(
            commands,
            worktree,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
        )?;
        if out.code == Some(0) {
            return Ok(candidate);
        }
    }
    Ok(base.to_owned())
}

/// Lines added and removed against the merge base, in tracked and untracked files. A command that
/// exits non-zero only loses its own lines; one that cannot run fails the whole computation.
fn compute(
    commands: &dyn Commands,
    worktree: &str,
    base: &str,
) -> Result<ChangeStats, CommandError> {
    let reference = resolve_base(commands, worktree, base)?;
    let merge_base = git(
        commands,
        worktree,
        &["merge-base", "--end-of-options", &reference, "HEAD"],
    )?;
    let printed = String::from_utf8_lossy(&merge_base.stdout);
    let printed = printed.trim();
    let against = if merge_base.code == Some(0) && !printed.is_empty() {
        printed.to_owned()
    } else {
        reference
    };

    let tracked = git(
        commands,
        worktree,
        &["diff", "--numstat", "--end-of-options", &against],
    )?;
    let (mut added, mut removed) = if tracked.code == Some(0) {
        sum_numstat(&tracked.stdout)
    } else {
        (0, 0)
    };

    let listing = git(
        commands,
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    if listing.code == Some(0) {
        let files = listing
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .take(MAX_UNTRACKED_FILES);
        for file in files {
            let file = String::from_utf8_lossy(file);
            // `--no-index` exits 1 for an ordinary difference and 0 for an empty file; what it
            // printed counts either way.
            let out = git(
                commands,
                worktree,
                &["diff", "--no-index", "--numstat", "--", "/dev/null", &file],
            )?;
            let (a, r) = sum_numstat(&out.stdout);
            added = added.saturating_add(a);
            removed = removed.saturating_add(r);
        }
    }
    Ok(ChangeStats { added, removed })
}

/// The sums of the first two columns of `git diff --numstat` output; a `-` (binary) counts as 0.
fn sum_numstat(numstat: &[u8]) -> (i64, i64) {
    let text = String::from_utf8_lossy(numstat);
    let mut added = 0i64;
    let mut removed = 0i64;
    for line in text.lines() {
        let mut columns = line.split('\t');
        let (Some(first), Some(second)) = (columns.next(), columns.next()) else {
            continue;
        };
        added = added.saturating_add(lines_in(first));
        removed = removed.saturating_add(lines_in(second));
    }
    (added, removed)
}

/// A numstat column as a number; `-` and anything else that is not one count as 0.
fn lines_in(column: &str) -> i64 {
    column.trim().parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn a_fresh_entry_of_the_same_workspace_state_is_not_stale() {
        assert!(!is_stale("t1", "t1", Duration::ZERO, true));
        assert!(!is_stale("t1", "t1", Duration::ZERO, false));
    }

    #[test]
    fn a_changed_updated_at_is_stale_at_any_age() {
        assert!(is_stale("t1", "t2", Duration::ZERO, false));
        assert!(is_stale("t1", "t2", Duration::ZERO, true));
    }

    #[test]
    fn a_working_entry_goes_stale_after_5_seconds() {
        assert!(!is_stale("t", "t", 5 * SECOND, true));
        assert!(is_stale(
            "t",
            "t",
            5 * SECOND + Duration::from_millis(1),
            true
        ));
    }

    #[test]
    fn an_idle_entry_goes_stale_after_60_seconds() {
        assert!(!is_stale("t", "t", 59 * SECOND, false));
        assert!(!is_stale("t", "t", 60 * SECOND, false));
        assert!(is_stale(
            "t",
            "t",
            60 * SECOND + Duration::from_millis(1),
            false
        ));
    }

    #[test]
    fn numstat_sums_the_first_two_columns_and_counts_binary_as_zero() {
        let text = b"12\t3\tsrc/a.rs\n-\t-\tlogo.png\n1\t0\tnew file.txt\n\nnot a stat line\n";
        assert_eq!(sum_numstat(text), (13, 3));
    }
}
