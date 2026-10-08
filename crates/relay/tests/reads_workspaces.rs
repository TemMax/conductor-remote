#[path = "support/seed_workspaces.rs"]
mod seed_workspaces;
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use conductor_remote::reads::workspaces::resolve_worktree;
use conductor_remote::reads::Reads;
use serde_json::{json, Value};
use support::TestDb;

fn setup() -> (TestDb, Reads) {
    let test = TestDb::new();
    seed_workspaces::seed(&test.conn(), test.root());
    let reads = Reads::new(test.db(), test.root());
    (test, reads)
}

fn workspaces_json(reads: &Reads) -> Vec<Value> {
    match serde_json::to_value(reads.list_workspaces().unwrap()).unwrap() {
        Value::Array(items) => items,
        other => panic!("expected an array, got {other}"),
    }
}

fn workspace_json(reads: &Reads, id: &str) -> Value {
    workspaces_json(reads)
        .into_iter()
        .find(|w| w["id"] == id)
        .unwrap_or_else(|| panic!("no live workspace {id}"))
}

fn repo_json(reads: &Reads, name: &str) -> Value {
    match serde_json::to_value(reads.list_repos().unwrap()).unwrap() {
        Value::Array(items) => items.into_iter().find(|r| r["name"] == name),
        other => panic!("expected an array, got {other}"),
    }
    .unwrap_or_else(|| panic!("no repo {name}"))
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// Replaces the temporary root in every string of `value` with `/ROOT`.
fn without_root(value: Value, root: &Path) -> Value {
    fn walk(value: Value, root: &str) -> Value {
        match value {
            Value::String(s) => Value::String(s.replace(root, "/ROOT")),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| walk(v, root)).collect()),
            Value::Object(map) => {
                Value::Object(map.into_iter().map(|(k, v)| (k, walk(v, root))).collect())
            }
            other => other,
        }
    }
    walk(value, root.to_str().expect("utf-8 root"))
}

fn golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/tests/contract/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

// ---------------------------------------------------------------- list_workspaces

#[test]
fn only_ready_and_setting_up_workspaces_are_live() {
    let (_test, reads) = setup();
    let ids: Vec<String> = workspaces_json(&reads)
        .iter()
        .map(|w| w["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(!ids.contains(&"ws-archived".to_owned()));
    assert!(!ids.contains(&"ws-active".to_owned()));
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        [
            "ws-iso",
            "ws-pinned",
            "ws-setting-up",
            "ws-space",
            "ws-unread"
        ]
    );
}

#[test]
fn pinned_first_then_raw_updated_at_descending() {
    let (_test, reads) = setup();
    let ids: Vec<String> = workspaces_json(&reads)
        .iter()
        .map(|w| w["id"].as_str().unwrap().to_owned())
        .collect();
    // ws-pinned is the oldest but pinned. ws-iso (ISO, 01:00) precedes ws-space (23:00 the same
    // day) because the text compare puts 'T' above ' '.
    assert_eq!(
        ids,
        [
            "ws-pinned",
            "ws-iso",
            "ws-space",
            "ws-unread",
            "ws-setting-up"
        ]
    );
}

#[test]
fn row_columns_and_the_active_session_come_through_raw() {
    let (_test, reads) = setup();
    let w = workspace_json(&reads, "ws-pinned");
    assert_eq!(w["directory_name"], "attic");
    assert_eq!(w["workspace_name"], "Lamp oil");
    assert_eq!(w["branch"], "feat/lamp-oil");
    assert_eq!(w["pr_title"], "Add lamp oil");
    assert_eq!(w["derived_status"], "in-progress");
    assert_eq!(w["manual_status"], Value::Null);
    assert_eq!(w["state"], "ready");
    assert_eq!(w["created_at"], "2026-01-10 12:00:00");
    assert_eq!(w["updated_at"], "2026-01-11T12:00:00.000Z");
    assert_eq!(w["pinned_at"], "2026-02-01T09:00:00.000Z");
    assert_eq!(w["active_session_id"], "ws-sess-working");
    assert_eq!(w["intended_target_branch"], "release/2");
    assert_eq!(w["repo_name"], "lantern");
    assert_eq!(w["default_branch"], "main");
    assert_eq!(w["repo_icon"], "emoji:\u{1F3EE}");
    assert_eq!(w["session_status"], "working");
    assert_eq!(w["session_title"], "Refill the lamps");
    assert_eq!(w["model"], "sonnet");
    assert_eq!(w["agent_type"], "claude");

    // A workspace without an active session has nulls from the join.
    let w = workspace_json(&reads, "ws-unread");
    assert_eq!(w["session_status"], Value::Null);
    assert_eq!(w["model"], Value::Null);
}

#[test]
fn unread_chats_are_grouped_per_workspace_with_raw_timestamps() {
    let (_test, reads) = setup();
    let w = workspace_json(&reads, "ws-unread");
    // The read, hidden and workspace-less chats are not there; both timestamp formats are kept.
    assert_eq!(
        w["unread_sessions"],
        json!([
            {"id": "ws-sess-unread-a", "at": "2026-03-02 09:00:00"},
            {"id": "ws-sess-unread-b", "at": "2026-03-02T09:30:00.000Z"},
        ])
    );
    for id in ["ws-pinned", "ws-iso", "ws-space", "ws-setting-up"] {
        assert_eq!(
            workspace_json(&reads, id)["unread_sessions"],
            json!([]),
            "{id}"
        );
    }
}

#[test]
fn worktree_is_the_directory_under_the_root_when_it_holds_a_git_entry() {
    let (test, reads) = setup();
    let root = test.root().to_str().unwrap();
    // A `.git` directory and a `.git` file both count.
    assert_eq!(
        workspace_json(&reads, "ws-pinned")["worktree"],
        format!("{root}/lantern/attic")
    );
    assert_eq!(
        workspace_json(&reads, "ws-iso")["worktree"],
        format!("{root}/anvil/forge")
    );
    // A directory without `.git`, a missing directory and a missing branch give null.
    for id in ["ws-space", "ws-unread", "ws-setting-up"] {
        assert_eq!(workspace_json(&reads, id)["worktree"], Value::Null, "{id}");
    }
}

#[test]
fn base_branch_is_target_then_repo_default_then_main_skipping_empty_strings() {
    let (_test, reads) = setup();
    // intended_target_branch wins.
    assert_eq!(
        workspace_json(&reads, "ws-pinned")["baseBranch"],
        "release/2"
    );
    // No target: the repository's default branch.
    assert_eq!(workspace_json(&reads, "ws-iso")["baseBranch"], "trunk");
    // An empty target falls through to the default.
    assert_eq!(workspace_json(&reads, "ws-space")["baseBranch"], "develop");
    // Neither: "main".
    assert_eq!(
        workspace_json(&reads, "ws-setting-up")["baseBranch"],
        "main"
    );
}

#[test]
fn workspace_icon_follows_its_repository() {
    let (_test, reads) = setup();
    assert_eq!(
        workspace_json(&reads, "ws-pinned")["icon"],
        json!({"kind": "emoji", "value": "\u{1F3EE}"})
    );
    assert_eq!(
        workspace_json(&reads, "ws-iso")["icon"],
        json!({"kind": "file"})
    );
    assert_eq!(
        workspace_json(&reads, "ws-space")["icon"],
        json!({"kind": "named", "value": "book"})
    );
    assert_eq!(
        workspace_json(&reads, "ws-setting-up")["icon"],
        json!({"kind": "github", "owner": "quartz-labs"})
    );
}

#[test]
fn fields_of_later_milestones_have_their_first_poll_values_and_nothing_else_is_emitted() {
    let (_test, reads) = setup();
    for w in workspaces_json(&reads) {
        assert_eq!(w["change_stats"], Value::Null);
        assert_eq!(w["pr_status"], Value::Null);
        assert_eq!(w["pr_number"], Value::Null);
        assert_eq!(w["pr_url"], Value::Null);
        assert_eq!(w["run_active"], false);
        assert_eq!(
            keys(&w),
            [
                "id",
                "directory_name",
                "workspace_name",
                "branch",
                "pr_title",
                "derived_status",
                "manual_status",
                "state",
                "created_at",
                "updated_at",
                "pinned_at",
                "active_session_id",
                "intended_target_branch",
                "repo_name",
                "repo_root",
                "repo_icon",
                "remote_url",
                "default_branch",
                "session_status",
                "session_title",
                "model",
                "agent_type",
                "unread_sessions",
                "worktree",
                "baseBranch",
                "icon",
                "change_stats",
                "pr_status",
                "pr_number",
                "pr_url",
                "run_active",
            ],
            "no pending_prompt, parked_prompts or removed-feature keys"
        );
    }
}

// ---------------------------------------------------------------- list_repos

#[test]
fn repos_are_ordered_by_latest_activity_then_display_order_then_name() {
    let (_test, reads) = setup();
    let names: Vec<String> = reads
        .list_repos()
        .unwrap()
        .into_iter()
        .map(|r| r.name.unwrap())
        .collect();
    // relic: archived workspace, newest of all. compass (03-05 23:00 as a space timestamp) before
    // anvil (03-05 01:00 as ISO) once both are normalised. Repos with no workspace come last, by
    // display_order (a missing one after a present one). The hidden repo is not listed.
    assert_eq!(
        names,
        ["relic", "compass", "anvil", "lantern", "quartz", "hollow", "plain"]
    );
}

#[test]
fn repo_rows_carry_only_name_root_default_branch_and_icon() {
    let (test, reads) = setup();
    let repo = repo_json(&reads, "lantern");
    assert_eq!(keys(&repo), ["name", "root_path", "default_branch", "icon"]);
    assert_eq!(
        repo["root_path"],
        format!("{}/_repos/lantern", test.root().to_str().unwrap())
    );
    assert_eq!(repo["default_branch"], "main");
    assert_eq!(repo_json(&reads, "quartz")["default_branch"], Value::Null);
    assert_eq!(repo_json(&reads, "compass")["root_path"], Value::Null);
}

#[test]
fn repo_icons_cover_emoji_named_file_github_and_none() {
    let (_test, reads) = setup();
    let icon = |name: &str| repo_json(&reads, name)["icon"].clone();
    assert_eq!(
        icon("lantern"),
        json!({"kind": "emoji", "value": "\u{1F3EE}"})
    );
    assert_eq!(icon("compass"), json!({"kind": "named", "value": "book"}));
    // An icon file in the checkout wins over the GitHub remote.
    assert_eq!(icon("anvil"), json!({"kind": "file"}));
    // The ssh remote form, with `.git` removed.
    assert_eq!(
        icon("quartz"),
        json!({"kind": "github", "owner": "quartz-labs"})
    );
    // An empty `emoji:` falls through to the GitHub remote.
    assert_eq!(
        icon("hollow"),
        json!({"kind": "github", "owner": "hollow-org"})
    );
    // A remote that is not GitHub, and no remote at all.
    assert_eq!(icon("relic"), Value::Null);
    assert_eq!(icon("plain"), Value::Null);
}

// ---------------------------------------------------------------- get_any_workspace

#[test]
fn any_workspace_returns_archived_and_live_ones_and_nothing_for_an_unknown_id() {
    let (_test, reads) = setup();
    let archived = serde_json::to_value(reads.get_any_workspace("ws-archived").unwrap()).unwrap();
    assert_eq!(
        archived,
        json!({
            "id": "ws-archived",
            "workspace_name": "Vault",
            "pr_title": "Old work",
            "branch": "old-work",
            "directory_name": "vault",
            "state": "archived",
            "updated_at": "2026-03-09T00:00:00.000Z",
            "repo_name": "relic",
            "icon": null,
            "archived": true,
        })
    );
    assert_eq!(keys(&archived)[0], "id");

    let live = serde_json::to_value(reads.get_any_workspace("ws-iso").unwrap()).unwrap();
    assert_eq!(live["archived"], false);
    assert_eq!(live["state"], "ready");
    assert_eq!(live["icon"], json!({"kind": "file"}));

    assert!(reads.get_any_workspace("ws-nope").unwrap().is_none());
    // The `active` state is neither live nor archived, but it is found.
    let active = serde_json::to_value(reads.get_any_workspace("ws-active").unwrap()).unwrap();
    assert_eq!(active["archived"], false);
}

#[test]
fn any_workspace_has_exactly_the_search_workspace_keys() {
    let (_test, reads) = setup();
    let value = serde_json::to_value(reads.get_any_workspace("ws-pinned").unwrap()).unwrap();
    assert_eq!(
        keys(&value),
        [
            "id",
            "workspace_name",
            "pr_title",
            "branch",
            "directory_name",
            "state",
            "updated_at",
            "repo_name",
            "icon",
            "archived",
        ]
    );
}

// ---------------------------------------------------------------- goldens

#[test]
fn golden_state() {
    let (test, reads) = setup();
    let actual = without_root(
        serde_json::to_value(reads.list_workspaces().unwrap()).unwrap(),
        test.root(),
    );
    assert_eq!(actual, golden("workspaces-state.json"));
}

#[test]
fn golden_repos() {
    let (test, reads) = setup();
    let actual = without_root(
        serde_json::to_value(reads.list_repos().unwrap()).unwrap(),
        test.root(),
    );
    assert_eq!(actual, golden("workspaces-repos.json"));
}

#[test]
fn golden_any() {
    let (test, reads) = setup();
    let actual = without_root(
        serde_json::to_value(reads.get_any_workspace("ws-pinned").unwrap()).unwrap(),
        test.root(),
    );
    assert_eq!(actual, golden("workspaces-any.json"));
}

// ---------------------------------------------------------------- resolve_worktree

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

/// A repository on `main` with one empty commit, in a canonical temporary path.
fn init_repo(parent: &Path, name: &str) -> PathBuf {
    let repo = parent.join(name);
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    repo
}

fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

fn resolve(root: &Path, branch: &str, repo_root: &Path) -> Option<PathBuf> {
    resolve_worktree(root, None, None, Some(branch), repo_root.to_str())
}

#[test]
fn the_directory_under_the_root_wins_without_asking_git() {
    let (_dir, tmp) = canonical_tempdir();
    std::fs::create_dir_all(tmp.join("repo/ws/.git")).unwrap();
    // The repo root does not exist: git would fail, and is not consulted.
    let found = resolve_worktree(
        &tmp,
        Some("repo"),
        Some("ws"),
        Some("b"),
        Some("/nonexistent/repo-root"),
    );
    assert_eq!(found, Some(tmp.join("repo/ws")));
}

#[test]
fn a_directory_without_git_or_unset_names_do_not_resolve() {
    let (_dir, tmp) = canonical_tempdir();
    std::fs::create_dir_all(tmp.join("repo/ws")).unwrap();
    assert_eq!(
        resolve_worktree(&tmp, Some("repo"), Some("ws"), None, None),
        None
    );
    // Empty strings count as unset.
    std::fs::create_dir_all(tmp.join("repo/ws/.git")).unwrap();
    assert_eq!(
        resolve_worktree(&tmp, Some(""), Some("ws"), Some(""), Some("")),
        None
    );
    assert_eq!(resolve_worktree(&tmp, None, None, None, None), None);
    // A repo root without a branch, and a branch without a repo root.
    assert_eq!(resolve_worktree(&tmp, None, None, None, tmp.to_str()), None);
    assert_eq!(resolve_worktree(&tmp, None, None, Some("main"), None), None);
}

#[test]
fn git_worktree_list_is_searched_by_branch() {
    let (_dir, tmp) = canonical_tempdir();
    let root = tmp.join("workspaces");
    std::fs::create_dir(&root).unwrap();
    let repo = init_repo(&tmp, "repo");
    let wt = tmp.join("elsewhere");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat/x",
            wt.to_str().unwrap(),
        ],
    );

    assert_eq!(resolve(&root, "feat/x", &repo), Some(wt));
    // The main checkout is a worktree too.
    assert_eq!(resolve(&root, "main", &repo), Some(repo.clone()));
    assert_eq!(resolve(&root, "no-such-branch", &repo), None);
}

#[test]
fn the_branch_match_is_a_substring_of_the_ref() {
    let (_dir, tmp) = canonical_tempdir();
    let repo = init_repo(&tmp, "repo");
    let wt = tmp.join("longer");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "foobar",
            wt.to_str().unwrap(),
        ],
    );
    // `refs/heads/foo` is inside `refs/heads/foobar`.
    assert_eq!(resolve(&tmp, "foo", &repo), Some(wt));
}

#[test]
fn the_listing_is_cached_per_repository_root_until_it_expires() {
    let (_dir, tmp) = canonical_tempdir();
    let repo = init_repo(&tmp, "repo");
    assert_eq!(resolve(&tmp, "late", &repo), None);

    let wt = tmp.join("late-wt");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "late", wt.to_str().unwrap()],
    );
    // The first listing, without the new worktree, is still the one used.
    assert_eq!(resolve(&tmp, "late", &repo), None);
}

#[test]
fn a_failed_listing_is_cached_as_nothing() {
    let (_dir, tmp) = canonical_tempdir();
    let not_yet = tmp.join("later");
    std::fs::create_dir(&not_yet).unwrap();
    // Not a repository: git fails.
    assert_eq!(resolve(&tmp, "main", &not_yet), None);
    git(&not_yet, &["init", "-q", "-b", "main"]);
    git(&not_yet, &["commit", "-q", "--allow-empty", "-m", "init"]);
    assert_eq!(resolve(&tmp, "main", &not_yet), None);
}
