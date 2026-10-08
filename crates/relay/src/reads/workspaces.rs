//! Workspace and repository reads.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::{ReadError, Reads};

/// How long a `git worktree list` may run before it counts as failed.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a repository's `git worktree list` result, or its failure, is reused.
const LISTING_TTL: Duration = Duration::from_secs(30);
/// How long the result of an icon file lookup is reused.
const ICON_TTL: Duration = Duration::from_secs(30);

/// Files in a repository root that Conductor shows as the repository's icon, first match wins.
const ICON_CANDIDATES: [&str; 17] = [
    "public/apple-touch-icon.png",
    "apple-touch-icon.png",
    "public/favicon.svg",
    "favicon.svg",
    "public/favicon.png",
    "public/icon.png",
    "public/logo.png",
    "favicon.png",
    "app/icon.png",
    "src/app/icon.png",
    "public/favicon.ico",
    "favicon.ico",
    "app/favicon.ico",
    "static/favicon.ico",
    "src-tauri/icons/icon.png",
    "assets/icon.png",
    "src/assets/icon.png",
];

/// How the phone draws a repository's avatar: the TypeScript `RepoIcon`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RepoIcon {
    Emoji { value: String },
    Named { value: String },
    File,
    Github { owner: String },
}

/// A chat Conductor flags unread. `at` is the session's raw `updated_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnreadSession {
    pub id: String,
    pub at: String,
}

/// Added and removed lines against the base branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeStats {
    pub added: i64,
    pub removed: i64,
}

/// The state of the pull request of a workspace's branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    Merged,
    Draft,
    Conflicts,
    ChecksFailed,
    ChecksPending,
    Mergeable,
}

/// One live workspace: the TypeScript `Workspace`. Field order is the order of the keys.
#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub id: String,
    pub directory_name: Option<String>,
    pub workspace_name: Option<String>,
    pub branch: Option<String>,
    pub pr_title: Option<String>,
    pub derived_status: Option<String>,
    pub manual_status: Option<String>,
    pub state: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub pinned_at: Option<String>,
    pub active_session_id: Option<String>,
    pub intended_target_branch: Option<String>,
    pub repo_name: Option<String>,
    pub repo_root: Option<String>,
    pub repo_icon: Option<String>,
    pub remote_url: Option<String>,
    pub default_branch: Option<String>,
    pub session_status: Option<String>,
    pub session_title: Option<String>,
    pub model: Option<String>,
    pub agent_type: Option<String>,
    pub unread_sessions: Vec<UnreadSession>,
    pub worktree: Option<String>,
    #[serde(rename = "baseBranch")]
    pub base_branch: String,
    pub icon: Option<RepoIcon>,
    /// Added and removed lines; `None` without a worktree, without extras and until the first
    /// refresh has finished.
    pub change_stats: Option<ChangeStats>,
    /// The pull request's state; `None` without extras and until the first refresh has finished.
    pub pr_status: Option<PrStatus>,
    /// The pull request's number; `None` as `pr_status`.
    pub pr_number: Option<i64>,
    /// The pull request's URL; `None` as `pr_status`.
    pub pr_url: Option<String>,
    /// Whether the Run task of the worktree is alive; `false` without extras and until the first
    /// listing has finished.
    pub run_active: bool,
}

/// A repository Conductor can create workspaces in: the TypeScript `RepoRow`.
#[derive(Debug, Clone, Serialize)]
pub struct RepoRow {
    /// The TypeScript type says `string`, but the column is nullable and the value is shipped as
    /// it is.
    pub name: Option<String>,
    pub root_path: Option<String>,
    pub default_branch: Option<String>,
    pub icon: Option<RepoIcon>,
}

/// A workspace as a lookup by id names it, live or archived: the TypeScript `SearchWorkspace`.
#[derive(Debug, Clone, Serialize)]
pub struct SearchWorkspace {
    pub id: String,
    pub workspace_name: Option<String>,
    pub pr_title: Option<String>,
    pub branch: Option<String>,
    pub directory_name: Option<String>,
    pub state: Option<String>,
    pub updated_at: String,
    pub repo_name: Option<String>,
    pub icon: Option<RepoIcon>,
    pub archived: bool,
}

/// The columns of Query A, in the order the SQL selects them.
struct WorkspaceRow {
    id: String,
    directory_name: Option<String>,
    workspace_name: Option<String>,
    branch: Option<String>,
    pr_title: Option<String>,
    derived_status: Option<String>,
    manual_status: Option<String>,
    state: Option<String>,
    created_at: String,
    updated_at: String,
    pinned_at: Option<String>,
    active_session_id: Option<String>,
    intended_target_branch: Option<String>,
    repo_name: Option<String>,
    repo_root: Option<String>,
    repo_icon: Option<String>,
    remote_url: Option<String>,
    default_branch: Option<String>,
    session_status: Option<String>,
    session_title: Option<String>,
    model: Option<String>,
    agent_type: Option<String>,
}

/// The columns of Query D, in the order the SQL selects them.
struct AnyWorkspaceRow {
    id: String,
    workspace_name: Option<String>,
    pr_title: Option<String>,
    branch: Option<String>,
    directory_name: Option<String>,
    state: Option<String>,
    updated_at: String,
    repo_name: Option<String>,
    repo_icon: Option<String>,
    repo_root: Option<String>,
    remote_url: Option<String>,
}

/// The columns of Query C, in the order the SQL selects them.
struct RepoQueryRow {
    name: Option<String>,
    root_path: Option<String>,
    default_branch: Option<String>,
    icon: Option<String>,
    remote_url: Option<String>,
}

const WORKSPACES_SQL: &str = "\
SELECT w.id, w.directory_name, w.workspace_name, w.branch, w.pr_title, w.derived_status, w.manual_status,
       w.state, w.created_at, w.updated_at, w.pinned_at, w.active_session_id, w.intended_target_branch,
       r.name AS repo_name, r.root_path AS repo_root, r.icon AS repo_icon,
       r.remote_url AS remote_url, r.default_branch AS default_branch,
       s.status AS session_status, s.title AS session_title, s.model AS model,
       s.agent_type AS agent_type
FROM workspaces w
LEFT JOIN repos r ON r.id = w.repository_id
LEFT JOIN sessions s ON s.id = w.active_session_id
WHERE w.state IN ('ready', 'setting_up')
ORDER BY (w.pinned_at IS NULL), w.updated_at DESC";

const UNREAD_SQL: &str = "\
SELECT workspace_id, id, updated_at
FROM sessions
WHERE COALESCE(unread_count, 0) > 0 AND COALESCE(is_hidden, 0) = 0";

const REPOS_SQL: &str = "\
SELECT r.name, r.root_path, r.default_branch, r.icon, r.remote_url
FROM repos r
LEFT JOIN workspaces w ON w.repository_id = r.id
WHERE COALESCE(r.hidden, 0) = 0
GROUP BY r.id
ORDER BY (MAX(MAX(REPLACE(REPLACE(w.updated_at, 'T', ' '), 'Z', ''), w.created_at)) IS NULL),
         MAX(MAX(REPLACE(REPLACE(w.updated_at, 'T', ' '), 'Z', ''), w.created_at)) DESC,
         (r.display_order IS NULL), r.display_order, r.name";

const ANY_WORKSPACE_SQL: &str = "\
SELECT w.id, w.workspace_name, w.pr_title, w.branch, w.directory_name, w.state, w.updated_at,
       r.name AS repo_name, r.icon AS repo_icon, r.root_path AS repo_root, r.remote_url AS remote_url
FROM workspaces w
LEFT JOIN repos r ON r.id = w.repository_id
WHERE w.id = ?
LIMIT 1";

impl Reads {
    /// The live workspaces, in the order the list shows them.
    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, ReadError> {
        let rows = self.db().read("list workspaces", |conn| {
            let mut stmt = conn.prepare(WORKSPACES_SQL)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(WorkspaceRow {
                        id: r.get(0)?,
                        directory_name: r.get(1)?,
                        workspace_name: r.get(2)?,
                        branch: r.get(3)?,
                        pr_title: r.get(4)?,
                        derived_status: r.get(5)?,
                        manual_status: r.get(6)?,
                        state: r.get(7)?,
                        created_at: r.get(8)?,
                        updated_at: r.get(9)?,
                        pinned_at: r.get(10)?,
                        active_session_id: r.get(11)?,
                        intended_target_branch: r.get(12)?,
                        repo_name: r.get(13)?,
                        repo_root: r.get(14)?,
                        repo_icon: r.get(15)?,
                        remote_url: r.get(16)?,
                        default_branch: r.get(17)?,
                        session_status: r.get(18)?,
                        session_title: r.get(19)?,
                        model: r.get(20)?,
                        agent_type: r.get(21)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })?;

        let unread_rows = self.db().read("list unread chats", |conn| {
            let mut stmt = conn.prepare(UNREAD_SQL)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        UnreadSession {
                            id: r.get(1)?,
                            at: r.get(2)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })?;
        let mut unread: HashMap<String, Vec<UnreadSession>> = HashMap::new();
        for (workspace_id, session) in unread_rows {
            if let Some(workspace_id) = workspace_id {
                unread.entry(workspace_id).or_default().push(session);
            }
        }

        // File-system and git work happens here, with the database lock released.
        Ok(rows
            .into_iter()
            .map(|row| {
                let worktree = resolve_worktree(
                    self.workspaces_root(),
                    row.repo_name.as_deref(),
                    row.directory_name.as_deref(),
                    row.branch.as_deref(),
                    row.repo_root.as_deref(),
                )
                .map(|p| p.to_string_lossy().into_owned());
                let base_branch = [
                    row.intended_target_branch.as_deref(),
                    row.default_branch.as_deref(),
                ]
                .into_iter()
                .flatten()
                .find(|b| !b.is_empty())
                .unwrap_or("main")
                .to_owned();
                let icon = describe_repo_icon(
                    row.repo_icon.as_deref(),
                    row.repo_root.as_deref(),
                    row.remote_url.as_deref(),
                );
                let mut workspace = Workspace {
                    unread_sessions: unread.get(&row.id).cloned().unwrap_or_default(),
                    id: row.id,
                    directory_name: row.directory_name,
                    workspace_name: row.workspace_name,
                    branch: row.branch,
                    pr_title: row.pr_title,
                    derived_status: row.derived_status,
                    manual_status: row.manual_status,
                    state: row.state,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                    pinned_at: row.pinned_at,
                    active_session_id: row.active_session_id,
                    intended_target_branch: row.intended_target_branch,
                    repo_name: row.repo_name,
                    repo_root: row.repo_root,
                    repo_icon: row.repo_icon,
                    remote_url: row.remote_url,
                    default_branch: row.default_branch,
                    session_status: row.session_status,
                    session_title: row.session_title,
                    model: row.model,
                    agent_type: row.agent_type,
                    worktree,
                    base_branch,
                    icon,
                    change_stats: None,
                    pr_status: None,
                    pr_number: None,
                    pr_url: None,
                    run_active: false,
                };
                if let Some(extras) = self.extras() {
                    // Each call returns the last known value and queues a refresh: none waits.
                    if let Some(worktree) = workspace.worktree.as_deref() {
                        workspace.change_stats = extras.change_stats.get(
                            worktree,
                            &workspace.base_branch,
                            &workspace.updated_at,
                            workspace.session_status.as_deref() == Some("working"),
                        );
                    }
                    let pr = extras.pr.get(
                        workspace.repo_root.as_deref(),
                        workspace.branch.as_deref(),
                        workspace.worktree.as_deref(),
                        &workspace.base_branch,
                    );
                    workspace.pr_status = pr.status;
                    workspace.pr_number = pr.number;
                    workspace.pr_url = pr.url;
                    workspace.run_active =
                        extras.processes.run_active(workspace.worktree.as_deref());
                }
                workspace
            })
            .collect())
    }

    /// The repositories Conductor shows, the most recently used first.
    pub fn list_repos(&self) -> Result<Vec<RepoRow>, ReadError> {
        let rows = self.db().read("list repos", |conn| {
            let mut stmt = conn.prepare(REPOS_SQL)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(RepoQueryRow {
                        name: r.get(0)?,
                        root_path: r.get(1)?,
                        default_branch: r.get(2)?,
                        icon: r.get(3)?,
                        remote_url: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })?;
        Ok(rows
            .into_iter()
            .map(|row| RepoRow {
                icon: describe_repo_icon(
                    row.icon.as_deref(),
                    row.root_path.as_deref(),
                    row.remote_url.as_deref(),
                ),
                name: row.name,
                root_path: row.root_path,
                default_branch: row.default_branch,
            })
            .collect())
    }

    /// One workspace by id, live or archived.
    pub fn get_any_workspace(&self, id: &str) -> Result<Option<SearchWorkspace>, ReadError> {
        let row = self.db().read("get any workspace", |conn| {
            let mut stmt = conn.prepare(ANY_WORKSPACE_SQL)?;
            let mut rows = stmt.query_map([id], |r| {
                Ok(AnyWorkspaceRow {
                    id: r.get(0)?,
                    workspace_name: r.get(1)?,
                    pr_title: r.get(2)?,
                    branch: r.get(3)?,
                    directory_name: r.get(4)?,
                    state: r.get(5)?,
                    updated_at: r.get(6)?,
                    repo_name: r.get(7)?,
                    repo_icon: r.get(8)?,
                    repo_root: r.get(9)?,
                    remote_url: r.get(10)?,
                })
            })?;
            rows.next().transpose()
        })?;
        Ok(row.map(|row| SearchWorkspace {
            icon: describe_repo_icon(
                row.repo_icon.as_deref(),
                row.repo_root.as_deref(),
                row.remote_url.as_deref(),
            ),
            archived: row.state.as_deref() == Some("archived"),
            id: row.id,
            workspace_name: row.workspace_name,
            pr_title: row.pr_title,
            branch: row.branch,
            directory_name: row.directory_name,
            state: row.state,
            updated_at: row.updated_at,
            repo_name: row.repo_name,
        }))
    }
}

/// The directory of a workspace's worktree, or `None` when it cannot be found.
///
/// The directory `<workspaces_root>/<repo_name>/<directory_name>` is taken when it holds a `.git`
/// entry. Otherwise `git -C <repo_root> worktree list --porcelain` is searched for the first
/// block that mentions `refs/heads/<branch>` (a substring match). The listing is kept per
/// repository root for 30 seconds, a failed one as "nothing", and then looked up again.
pub fn resolve_worktree(
    workspaces_root: &Path,
    repo_name: Option<&str>,
    directory_name: Option<&str>,
    branch: Option<&str>,
    repo_root: Option<&str>,
) -> Option<PathBuf> {
    if let (Some(repo), Some(dir)) = (non_empty(repo_name), non_empty(directory_name)) {
        let guess = workspaces_root.join(repo).join(dir);
        if guess.join(".git").exists() {
            return Some(guess);
        }
    }
    let (repo_root, branch) = (non_empty(repo_root)?, non_empty(branch)?);
    let listing = worktree_listing(repo_root)?;
    let needle = format!("refs/heads/{branch}");
    listing
        .split("\n\n")
        .filter(|block| block.contains(&needle))
        .find_map(|block| {
            block
                .lines()
                .find_map(|line| line.strip_prefix("worktree ").filter(|p| !p.is_empty()))
        })
        .map(PathBuf::from)
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.is_empty())
}

type WorktreeCache = Mutex<HashMap<String, (Instant, Option<String>)>>;

/// The porcelain `git worktree list` output of a repository, from the process-wide cache.
fn worktree_listing(repo_root: &str) -> Option<String> {
    static CACHE: OnceLock<WorktreeCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    listing_at(cache, repo_root, Instant::now(), run_worktree_list)
}

/// The listing of `repo_root` as of `now`: the cached one while it is younger than
/// `LISTING_TTL`, otherwise what `run` makes, which is cached with `now` as its time.
fn listing_at(
    cache: &WorktreeCache,
    repo_root: &str,
    now: Instant,
    run: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let lock = || cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, hit)) = lock().get(repo_root) {
        if now.saturating_duration_since(*at) < LISTING_TTL {
            return hit.clone();
        }
    }
    // The lock is not held while git runs: two calls may both run it.
    let listing = run(repo_root).filter(|l| !l.is_empty());
    lock().insert(repo_root.to_owned(), (now, listing.clone()));
    listing
}

/// Runs `git -C <repo_root> worktree list --porcelain`; `None` on any failure or after 5 seconds.
fn run_worktree_list(repo_root: &str) -> Option<String> {
    let mut child = Command::new("git")
        .args(["-C", repo_root, "worktree", "list", "--porcelain"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let result = stdout.read_to_end(&mut out).map(|_| out);
        let _ = tx.send(result);
    });
    let started = Instant::now();
    let out = match rx.recv_timeout(GIT_TIMEOUT) {
        Ok(Ok(out)) => out,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < GIT_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    status
        .success()
        .then(|| String::from_utf8_lossy(&out).into_owned())
}

/// How to draw a repository's avatar, in Conductor's order: an explicit icon, a known icon file in
/// the repository root, the GitHub owner's avatar, nothing.
fn describe_repo_icon(
    icon: Option<&str>,
    repo_root: Option<&str>,
    remote_url: Option<&str>,
) -> Option<RepoIcon> {
    if let Some(explicit) = icon.map(str::trim).filter(|i| !i.is_empty()) {
        match explicit.strip_prefix("emoji:") {
            Some(rest) => {
                let value = rest.trim();
                if !value.is_empty() {
                    return Some(RepoIcon::Emoji {
                        value: value.to_owned(),
                    });
                }
                // An empty `emoji:` is meaningless: fall through to the file and GitHub steps.
            }
            None => {
                return Some(RepoIcon::Named {
                    value: explicit.to_owned(),
                });
            }
        }
    }
    if non_empty(repo_root).is_some_and(has_icon_file) {
        return Some(RepoIcon::File);
    }
    non_empty(remote_url)
        .and_then(github_owner)
        .map(|owner| RepoIcon::Github { owner })
}

type IconCache = Mutex<HashMap<String, (Instant, Option<PathBuf>)>>;

/// Whether the repository root holds one of the icon files; the answer is kept for 30 seconds.
fn has_icon_file(repo_root: &str) -> bool {
    resolve_repo_icon(repo_root).is_some()
}

/// The first icon file of `ICON_CANDIDATES` under the repository root that is a regular file (a
/// directory or a symbolic link is not an icon). The answer, a path or nothing, is kept for 30
/// seconds per root.
pub fn resolve_repo_icon(repo_root: &str) -> Option<PathBuf> {
    static CACHE: OnceLock<IconCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let lock = || cache.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let cached = lock().get(repo_root).cloned();
    if let Some((at, found)) = cached {
        if now.duration_since(at) < ICON_TTL {
            return found;
        }
    }
    let root = Path::new(repo_root);
    let found = ICON_CANDIDATES
        .iter()
        .map(|rel| root.join(rel))
        .find(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()));
    lock().insert(repo_root.to_owned(), (now, found.clone()));
    found
}

/// The GitHub owner of a remote URL, as the pattern
/// `/github\.com[:/]+([^/]+)\/[^/]+$/i` finds it, with a trailing `.git` removed from the owner.
/// `None` when the URL does not match or the owner is empty.
fn github_owner(remote_url: &str) -> Option<String> {
    const HOST: &str = "github.com";
    // ASCII lowercasing keeps every byte offset.
    let lower = remote_url.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(HOST) {
        let start = from + found;
        from = start + 1;
        let after = &remote_url[start + HOST.len()..];
        let rest = after.trim_start_matches([':', '/']);
        if rest.len() == after.len() {
            continue;
        }
        // The rest must be `owner/name`: one slash, both sides non-empty.
        let Some((owner, name)) = rest.split_once('/') else {
            continue;
        };
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            continue;
        }
        let owner = match owner.len().checked_sub(4) {
            Some(cut)
                if owner.is_char_boundary(cut) && owner[cut..].eq_ignore_ascii_case(".git") =>
            {
                &owner[..cut]
            }
            _ => owner,
        };
        return (!owner.is_empty()).then(|| owner.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_listing_is_looked_up_again_after_it_expires() {
        let cache = WorktreeCache::default();
        let start = Instant::now();
        let calls = std::cell::Cell::new(0);
        let lookup = |now: Instant, answer: Option<&'static str>| {
            listing_at(&cache, "/repo", now, |_| {
                calls.set(calls.get() + 1);
                answer.map(str::to_owned)
            })
        };

        // A failure is kept as nothing for the whole interval, then looked up again.
        assert_eq!(lookup(start, None), None);
        assert_eq!(lookup(start + LISTING_TTL / 2, Some("late")), None);
        assert_eq!(calls.get(), 1);
        let later = start + LISTING_TTL;
        assert_eq!(lookup(later, Some("first")), Some("first".to_owned()));
        assert_eq!(calls.get(), 2);

        // A success is kept too, until its own age reaches the limit.
        let almost = later + LISTING_TTL - Duration::from_millis(1);
        assert_eq!(lookup(almost, Some("second")), Some("first".to_owned()));
        assert_eq!(calls.get(), 2);
        let expired = later + LISTING_TTL;
        assert_eq!(lookup(expired, Some("second")), Some("second".to_owned()));
        assert_eq!(calls.get(), 3);

        // Another repository root has its own entry.
        let other = listing_at(&cache, "/other", start, |_| Some("other".to_owned()));
        assert_eq!(other, Some("other".to_owned()));
    }
}
