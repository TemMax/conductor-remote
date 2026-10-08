//! Search results: workspaces matched by name or by chat, with the evidence.

use std::collections::{HashMap, HashSet};

use rusqlite::{params_from_iter, Connection, Row};
use serde::Serialize;

use super::index::{ChunkHit, IndexError, IndexStatus, SearchIndex};
use super::query::{match_query, query_tokens};
use crate::db::DbError;
use crate::reads::workspaces::{resolve_repo_icon, RepoIcon, SearchWorkspace};
use crate::reads::{ReadError, Reads};

/// Chunks the full-text query returns at most.
const HIT_LIMIT: usize = 300;
/// Hits of a workspace that become snippets and make up its score.
const SNIPPETS_PER_RESULT: usize = 3;
/// The columns a token is looked for in, as `w.` and `r.` name them.
const NAME_FIELDS: [&str; 5] = [
    "w.workspace_name",
    "w.pr_title",
    "w.branch",
    "w.directory_name",
    "r.name",
];

/// What `GET /api/search` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchParams {
    pub q: String,
    /// Repo names to limit the search to; empty means every repo.
    pub repos: Vec<String>,
    pub archived: bool,
    pub limit: usize,
}

/// Which kind of prose matched: the web app's `SearchRole`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchRole {
    User,
    Assistant,
    Thinking,
}

/// The web app's `SearchSnippet`: one matching excerpt.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSnippet {
    pub session_id: String,
    /// Opaque source-message pointer for a bounded read around this hit.
    pub cursor: String,
    pub role: SearchRole,
    pub at: String,
    /// The excerpt, hits wrapped in the open and close markers.
    pub text: String,
}

/// The web app's `SearchResult`: a workspace a search matched, with the evidence
/// (`SearchResult<SearchWorkspace>` plus the tab title).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub workspace: SearchWorkspace,
    /// The chat holding this workspace's strongest passage.
    pub session_id: Option<String>,
    /// Number of matching messages, all of them.
    pub hits: u64,
    /// Higher is better: the summed score of the snippets below.
    pub score: f64,
    /// Most recent matching message.
    pub at: Option<String>,
    pub snippets: Vec<SearchSnippet>,
    /// True when the workspace's own name or branch matched, rather than (only) its chats.
    pub by_name: bool,
    pub session_title: Option<String>,
}

/// The web app's `SearchResponse`.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub query: String,
    /// Repo names the search was scoped to; empty means every repo.
    pub repos: Vec<String>,
    pub index: IndexStatus,
    pub results: Vec<SearchResult>,
}

impl Reads {
    /// Workspaces matching `params.q` by name or by what was said in their chats: the full-text
    /// hits folded per workspace, the name matches first, cut to `params.limit`.
    ///
    /// Without an index, or while it fails, there are no full-text hits; a failure is reported
    /// as `index.error`, never as a failed answer.
    pub fn search(
        &self,
        index: Option<&SearchIndex>,
        params: &SearchParams,
    ) -> Result<SearchResponse, ReadError> {
        let mut status = match index {
            Some(index) => index.status(),
            None => IndexStatus {
                chunks: 0,
                ready: false,
                progress: 0.0,
                error: None,
            },
        };
        let tokens = query_tokens(&params.q);
        let mut results = Vec::new();
        if !tokens.is_empty() {
            let repos = (!params.repos.is_empty()).then_some(params.repos.as_slice());
            let mut hits = Vec::new();
            if let (Some(index), Some(expr)) = (index, match_query(&params.q)) {
                let scope = if repos.is_some() || !params.archived {
                    Some(self.db().read("search scope", |conn| {
                        scope_session_ids(conn, repos, params.archived)
                    })?)
                } else {
                    None
                };
                match index.search(&expr, scope.as_deref(), HIT_LIMIT) {
                    Ok(found) => hits = found,
                    Err(error) => status.error = Some(index_error_kind(&error)),
                }
            }
            let targets = self.search_targets(&hits)?;
            let from_chats = fold_hits(&hits, |session_id| {
                let workspace = &targets.get(session_id)?.workspace;
                (params.archived || !workspace.archived).then_some(workspace)
            });
            let names = self.workspaces_by_name(&tokens, params.limit, repos, params.archived)?;
            results = merge(names, from_chats, params.limit);
            for result in &mut results {
                result.session_title = result
                    .session_id
                    .as_deref()
                    .and_then(|id| targets.get(id))
                    .and_then(|target| target.session_title.clone());
            }
        }
        Ok(SearchResponse {
            query: params.q.clone(),
            repos: params.repos.clone(),
            index: status,
            results,
        })
    }

    /// The chat and workspace of every session the hits came from.
    fn search_targets(&self, hits: &[ChunkHit]) -> Result<HashMap<String, Target>, ReadError> {
        let mut seen = HashSet::new();
        let ids: Vec<&str> = hits
            .iter()
            .map(|hit| hit.session_id.as_str())
            .filter(|id| seen.insert(*id))
            .collect();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "SELECT s.id AS session_id, s.title AS session_title, {WORKSPACE_COLUMNS}
             FROM sessions s
             JOIN workspaces w ON w.id = s.workspace_id
             LEFT JOIN repos r ON r.id = w.repository_id
             WHERE s.id IN ({})",
            placeholders(ids.len())
        );
        let rows = self.db().read("search targets", |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(ids.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get(1)?,
                    workspace_row(row, 2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<(String, Option<String>, WorkspaceRow)>>>()
        })?;
        Ok(rows
            .into_iter()
            .map(|(session_id, session_title, row)| {
                (
                    session_id,
                    Target {
                        session_title,
                        workspace: row.into_workspace(),
                    },
                )
            })
            .collect())
    }

    /// The workspaces whose own identity matches every token, archived last.
    fn workspaces_by_name(
        &self,
        tokens: &[String],
        limit: usize,
        repos: Option<&[String]>,
        include_archived: bool,
    ) -> Result<Vec<SearchWorkspace>, ReadError> {
        // AND across tokens, OR across fields.
        let clause = format!(
            "({})",
            NAME_FIELDS
                .iter()
                .map(|field| format!("{field} LIKE ? ESCAPE '\\'"))
                .collect::<Vec<_>>()
                .join(" OR ")
        );
        let mut conditions = vec![clause; tokens.len()];
        let mut args: Vec<rusqlite::types::Value> = tokens
            .iter()
            .flat_map(|token| {
                let pattern = format!("%{}%", escape_like(token));
                NAME_FIELDS.map(|_| rusqlite::types::Value::Text(pattern.clone()))
            })
            .collect();
        if let Some(repos) = repos {
            conditions.push(format!("r.name IN ({})", placeholders(repos.len())));
            args.extend(repos.iter().cloned().map(rusqlite::types::Value::Text));
        }
        if !include_archived {
            conditions.push("w.state IS NOT 'archived'".to_owned());
        }
        args.push(rusqlite::types::Value::Integer(
            i64::try_from(limit).unwrap_or(i64::MAX),
        ));
        let sql = format!(
            "SELECT {WORKSPACE_COLUMNS}
             FROM workspaces w
             LEFT JOIN repos r ON r.id = w.repository_id
             WHERE {}
             ORDER BY (w.state = 'archived'), w.updated_at DESC
             LIMIT ?",
            conditions.join(" AND ")
        );
        let rows = self.db().read("search names", |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(args), |row| workspace_row(row, 0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(WorkspaceRow::into_workspace).collect())
    }
}

/// The workspace columns both queries select, from `id` to `remote_url`.
const WORKSPACE_COLUMNS: &str = "\
w.id, w.workspace_name, w.pr_title, w.branch, w.directory_name, w.state, w.updated_at,
       r.name AS repo_name, r.icon AS repo_icon, r.root_path AS repo_root, r.remote_url AS remote_url";

/// A session's workspace and the session's own title.
struct Target {
    session_title: Option<String>,
    workspace: SearchWorkspace,
}

/// The columns of `WORKSPACE_COLUMNS`; the icon is resolved after the read.
struct WorkspaceRow {
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

fn workspace_row(row: &Row<'_>, at: usize) -> rusqlite::Result<WorkspaceRow> {
    Ok(WorkspaceRow {
        id: row.get(at)?,
        workspace_name: row.get(at + 1)?,
        pr_title: row.get(at + 2)?,
        branch: row.get(at + 3)?,
        directory_name: row.get(at + 4)?,
        state: row.get(at + 5)?,
        updated_at: row.get(at + 6)?,
        repo_name: row.get(at + 7)?,
        repo_icon: row.get(at + 8)?,
        repo_root: row.get(at + 9)?,
        remote_url: row.get(at + 10)?,
    })
}

impl WorkspaceRow {
    fn into_workspace(self) -> SearchWorkspace {
        SearchWorkspace {
            icon: describe_repo_icon(
                self.repo_icon.as_deref(),
                self.repo_root.as_deref(),
                self.remote_url.as_deref(),
            ),
            archived: self.state.as_deref() == Some("archived"),
            id: self.id,
            workspace_name: self.workspace_name,
            pr_title: self.pr_title,
            branch: self.branch,
            directory_name: self.directory_name,
            state: self.state,
            updated_at: self.updated_at,
            repo_name: self.repo_name,
        }
    }
}

/// Every chat in the scope: the repos by name, and without archived workspaces when they are
/// excluded.
fn scope_session_ids(
    conn: &Connection,
    repos: Option<&[String]>,
    include_archived: bool,
) -> rusqlite::Result<Vec<String>> {
    let mut conditions = Vec::new();
    let mut args = Vec::new();
    if let Some(repos) = repos {
        conditions.push(format!("r.name IN ({})", placeholders(repos.len())));
        args.extend(repos);
    }
    if !include_archived {
        conditions.push("w.state IS NOT 'archived'".to_owned());
    }
    let filter = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT s.id FROM sessions s
         JOIN workspaces w ON w.id = s.workspace_id
         LEFT JOIN repos r ON r.id = w.repository_id{filter}"
    ))?;
    let ids = stmt.query_map(params_from_iter(args), |row| row.get(0))?;
    ids.collect()
}

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(",")
}

/// `text` with `\`, `%` and `_` escaped for `LIKE … ESCAPE '\'`.
fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// What the index calls the error: the words of `IndexError`'s own kind, which carry no path and
/// no row content.
fn index_error_kind(error: &IndexError) -> String {
    fn sqlite_kind(prefix: &str, error: &rusqlite::Error) -> String {
        match error.sqlite_error_code() {
            Some(code) => format!("{prefix}: {code:?}"),
            None => prefix.to_owned(),
        }
    }
    match error {
        IndexError::Sqlite(e) => sqlite_kind("sqlite", e),
        IndexError::Source(DbError::Open { .. }) => "source: open".to_owned(),
        IndexError::Source(DbError::Query(e)) => sqlite_kind("source", e),
    }
}

fn parse_role(role: &str) -> Option<SearchRole> {
    match role {
        "user" => Some(SearchRole::User),
        "assistant" => Some(SearchRole::Assistant),
        "thinking" => Some(SearchRole::Thinking),
        _ => None,
    }
}

/// `n` in base 36, as JavaScript's `toString(36)` writes it.
fn base36(n: i64) -> String {
    let mut rest = n.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        digits.push(char::from_digit((rest % 36) as u32, 36).expect("a digit below 36"));
        rest /= 36;
        if rest == 0 {
            break;
        }
    }
    if n < 0 {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

/// A workspace's hits in the order they arrive (best first), before the fold is done.
struct Folding {
    result: SearchResult,
    /// The best score of each of the workspace's sessions, in the order they first matched.
    best_by_session: Vec<(String, f64)>,
}

/// Folds chunk hits, best first, into one result per workspace.
///
/// `resolve` names the workspace of a session, or `None` to drop the session's hits. `hits` of a
/// workspace counts every one of its hits; the first three become its snippets and are the only
/// ones summed into `score`; `at` is the greatest time; `session_id` is the session with the
/// best single hit (the first of equals). Results are sorted by score, best first, keeping the
/// order the workspaces first appeared in between equal scores. A hit with a role the index
/// never writes is dropped.
pub fn fold_hits<'a>(
    hits: &[ChunkHit],
    resolve: impl Fn(&str) -> Option<&'a SearchWorkspace>,
) -> Vec<SearchResult> {
    let mut order: Vec<Folding> = Vec::new();
    let mut slot: HashMap<&str, usize> = HashMap::new();
    for hit in hits {
        let Some(role) = parse_role(&hit.role) else {
            continue;
        };
        let Some(workspace) = resolve(&hit.session_id) else {
            continue;
        };
        let at = *slot.entry(workspace.id.as_str()).or_insert_with(|| {
            order.push(Folding {
                result: SearchResult {
                    workspace: workspace.clone(),
                    session_id: None,
                    hits: 0,
                    score: 0.0,
                    at: None,
                    snippets: Vec::new(),
                    by_name: false,
                    session_title: None,
                },
                best_by_session: Vec::new(),
            });
            order.len() - 1
        });
        let entry = &mut order[at];
        entry.result.hits += 1;
        if let Some(time) = &hit.at {
            if entry.result.at.as_ref().is_none_or(|latest| time > latest) {
                entry.result.at = Some(time.clone());
            }
        }
        match entry
            .best_by_session
            .iter_mut()
            .find(|(id, _)| *id == hit.session_id)
        {
            Some((_, best)) => *best = best.max(hit.score),
            None => entry
                .best_by_session
                .push((hit.session_id.clone(), hit.score.max(0.0))),
        }
        if entry.result.snippets.len() < SNIPPETS_PER_RESULT {
            entry.result.score += hit.score;
            entry.result.snippets.push(SearchSnippet {
                session_id: hit.session_id.clone(),
                cursor: format!("m{}", base36(hit.src_rowid)),
                role,
                at: hit.at.clone().unwrap_or_default(),
                text: hit.snippet.clone(),
            });
        }
    }
    let mut results: Vec<SearchResult> = order
        .into_iter()
        .map(|folding| {
            let mut result = folding.result;
            let mut best = f64::NEG_INFINITY;
            for (id, score) in folding.best_by_session {
                if score <= best {
                    continue;
                }
                best = score;
                result.session_id = Some(id);
            }
            result
        })
        .collect();
    results.sort_by(|a, b| b.score.total_cmp(&a.score));
    results
}

/// Name matches first, in their order, each with its chat evidence if it has any; then the
/// results that only chats found; at most `limit`.
fn merge(
    names: Vec<SearchWorkspace>,
    from_chats: Vec<SearchResult>,
    limit: usize,
) -> Vec<SearchResult> {
    let mut remaining: Vec<Option<SearchResult>> = from_chats.into_iter().map(Some).collect();
    let mut merged = Vec::new();
    for workspace in names {
        let evidence = remaining
            .iter_mut()
            .find(|slot| {
                slot.as_ref()
                    .is_some_and(|r| r.workspace.id == workspace.id)
            })
            .and_then(Option::take);
        merged.push(match evidence {
            Some(evidence) => SearchResult {
                by_name: true,
                ..evidence
            },
            None => SearchResult {
                workspace,
                session_id: None,
                hits: 0,
                score: 0.0,
                at: None,
                snippets: Vec::new(),
                by_name: true,
                session_title: None,
            },
        });
    }
    merged.extend(remaining.into_iter().flatten());
    merged.truncate(limit);
    merged
}

/// How to draw a repository's avatar, in Conductor's order: an explicit icon, a known icon file in
/// the repository root, the GitHub owner's avatar, nothing.
fn describe_repo_icon(
    icon: Option<&str>,
    repo_root: Option<&str>,
    remote_url: Option<&str>,
) -> Option<RepoIcon> {
    fn non_empty(s: Option<&str>) -> Option<&str> {
        s.filter(|s| !s.is_empty())
    }
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
    if non_empty(repo_root).is_some_and(|root| resolve_repo_icon(root).is_some()) {
        return Some(RepoIcon::File);
    }
    non_empty(remote_url)
        .and_then(github_owner)
        .map(|owner| RepoIcon::Github { owner })
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
