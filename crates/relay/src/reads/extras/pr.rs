//! Pull requests of branches, asked of GitHub in the background.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use super::commands::{Commands, Limits};
use super::swr::Swr;
use super::Shared;
use crate::reads::workspaces::PrStatus;

/// What is known of the pull request of a branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrFacts {
    pub status: Option<PrStatus>,
    pub number: Option<i64>,
    pub url: Option<String>,
}

/// How long the pull requests of a repository are fresh.
const REPO_TTL: Duration = Duration::from_secs(60);
/// How long a conflict verdict is fresh.
const CONFLICT_TTL: Duration = Duration::from_secs(30);
/// The time-out of every `gh` and `git` call.
const TIMEOUT: Duration = Duration::from_secs(15);
/// The most output `gh` and `git merge-tree` may write.
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
/// The most output `git rev-parse` may write.
const MAX_REV_PARSE_OUTPUT: usize = 64 * 1024;

const GH_FIELDS: &str = "headRefName,number,url,state,isDraft,updatedAt,statusCheckRollup";

const FAILED_CONCLUSIONS: [&str; 6] = [
    "ACTION_REQUIRED",
    "CANCELLED",
    "FAILURE",
    "STALE",
    "STARTUP_FAILURE",
    "TIMED_OUT",
];
const FAILED_STATES: [&str; 2] = ["ERROR", "FAILURE"];
const PENDING_STATES: [&str; 2] = ["EXPECTED", "PENDING"];

/// One row of GitHub's check rollup: a check run reports progress in `status` and its verdict in
/// `conclusion`; a legacy status context carries both in `state`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
struct Check {
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
enum PrState {
    Open,
    Closed,
    Merged,
    #[serde(other)]
    Other,
}

/// One pull request as `gh pr list` prints it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct PullRequest {
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    number: i64,
    url: String,
    state: PrState,
    #[serde(rename = "isDraft", default)]
    is_draft: bool,
    #[serde(rename = "updatedAt", default)]
    updated_at: String,
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Option<Vec<Check>>,
}

impl PullRequest {
    /// Whether the conflict check is worth asking for: only an open pull request that is not a draft.
    fn wants_conflict_check(&self) -> bool {
        self.state == PrState::Open && !self.is_draft
    }
}

/// The pull requests of a repository by branch; empty when none are known.
type Branches = Arc<HashMap<String, PullRequest>>;

/// Source of pull request facts.
pub struct PrSource {
    shared: Arc<Shared>,
    repos: Swr<String, Branches>,
    conflicts: Swr<(String, String), bool>,
}

impl PrSource {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            repos: Swr::new(shared.pool.clone(), shared.revision.clone()),
            conflicts: Swr::new(shared.pool.clone(), shared.revision.clone()),
            shared,
        }
    }

    /// Last known pull request of a branch. Never waits: what is not known yet is left out, and
    /// the revision moves when it arrives.
    pub fn get(
        &self,
        repo_root: Option<&str>,
        branch: Option<&str>,
        worktree: Option<&str>,
        base_branch: &str,
    ) -> PrFacts {
        let (Some(repo_root), Some(branch)) = (
            repo_root.filter(|s| !s.is_empty()),
            branch.filter(|s| !s.is_empty()),
        ) else {
            return PrFacts::default();
        };

        let commands = self.shared.commands.clone();
        let root = repo_root.to_owned();
        let branches = self.repos.get(
            &root.clone(),
            |entry| entry.at.elapsed() > REPO_TTL,
            move || fetch_pull_requests(commands.as_ref(), &root),
        );
        let Some(pr) = branches.and_then(|map| map.get(branch).cloned()) else {
            return PrFacts::default();
        };

        let worktree = worktree.filter(|s| !s.is_empty());
        let conflict = match worktree {
            Some(worktree) if pr.wants_conflict_check() => {
                let commands = self.shared.commands.clone();
                let path = worktree.to_owned();
                let base = base_branch.to_owned();
                self.conflicts
                    .get(
                        &(worktree.to_owned(), base_branch.to_owned()),
                        |entry| entry.at.elapsed() > CONFLICT_TTL,
                        move || has_conflict(commands.as_ref(), &path, &base),
                    )
                    // Not known yet counts as no conflict.
                    .unwrap_or(false)
            }
            _ => false,
        };

        PrFacts {
            status: decide_status(&pr, conflict),
            number: Some(pr.number),
            url: Some(pr.url),
        }
    }
}

/// The status of a pull request, from its state, its checks and the conflict verdict.
fn decide_status(pr: &PullRequest, conflict: bool) -> Option<PrStatus> {
    match pr.state {
        PrState::Merged => Some(PrStatus::Merged),
        PrState::Open if pr.is_draft => Some(PrStatus::Draft),
        PrState::Open => {
            let checks = pr.status_check_rollup.as_deref().unwrap_or_default();
            Some(if conflict {
                PrStatus::Conflicts
            } else if checks.iter().any(check_failed) {
                PrStatus::ChecksFailed
            } else if checks.iter().any(check_pending) {
                PrStatus::ChecksPending
            } else {
                PrStatus::Mergeable
            })
        }
        PrState::Closed | PrState::Other => None,
    }
}

fn check_failed(check: &Check) -> bool {
    check
        .conclusion
        .as_deref()
        .is_some_and(|c| FAILED_CONCLUSIONS.contains(&c))
        || check
            .state
            .as_deref()
            .is_some_and(|s| FAILED_STATES.contains(&s))
}

fn check_pending(check: &Check) -> bool {
    check
        .status
        .as_deref()
        .is_some_and(|s| !s.is_empty() && s != "COMPLETED")
        || check
            .state
            .as_deref()
            .is_some_and(|s| PENDING_STATES.contains(&s))
}

/// One `gh` call per repository. Any failure is "no pull requests known".
fn fetch_pull_requests(commands: &dyn Commands, repo_root: &str) -> Branches {
    let limits = Limits {
        timeout: TIMEOUT,
        max_stdout: MAX_OUTPUT,
    };
    let args = [
        "pr", "list", "--state", "all", "--limit", "100", "--json", GH_FIELDS,
    ];
    let listed = commands
        .run("gh", &args, Some(Path::new(repo_root)), limits)
        .ok()
        .filter(|output| output.code == Some(0))
        .and_then(|output| serde_json::from_slice::<Vec<PullRequest>>(&output.stdout).ok());
    let Some(mut list) = listed else {
        return Arc::default();
    };
    // Oldest first, so the newest pull request wins when a branch was used for several.
    list.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
    Arc::new(
        list.into_iter()
            .map(|pr| (pr.head_ref_name.clone(), pr))
            .collect(),
    )
}

/// Would merging the worktree into its base conflict? `git merge-tree --write-tree` merges in
/// memory and exits 1 on a conflict; anything else counts as no conflict.
fn has_conflict(commands: &dyn Commands, worktree: &str, base: &str) -> bool {
    let rev_limits = Limits {
        timeout: TIMEOUT,
        max_stdout: MAX_REV_PARSE_OUTPUT,
    };
    let origin = format!("origin/{base}");
    let reference = [origin.as_str(), base]
        .into_iter()
        .find(|candidate| {
            let spec = format!("{candidate}^{{commit}}");
            commands
                .run(
                    "git",
                    &[
                        "-C",
                        worktree,
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        "--end-of-options",
                        &spec,
                    ],
                    None,
                    rev_limits,
                )
                .is_ok_and(|output| output.code == Some(0))
        })
        .unwrap_or(base);
    let limits = Limits {
        timeout: TIMEOUT,
        max_stdout: MAX_OUTPUT,
    };
    commands
        .run(
            "git",
            &[
                "-C",
                worktree,
                "merge-tree",
                "--write-tree",
                "--end-of-options",
                reference,
                "HEAD",
            ],
            None,
            limits,
        )
        .is_ok_and(|output| output.code == Some(1))
}
