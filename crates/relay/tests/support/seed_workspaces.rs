//! A small invented data set for the workspace reads. Every id starts with `ws-`.
//!
//! Live workspaces, in the order the list shows them (pinned first, then raw `updated_at`
//! descending): `ws-pinned`, `ws-iso`, `ws-space`, `ws-unread`, `ws-setting-up`.
//! Not live: `ws-archived` (state `archived`) and `ws-active` (state `active`).
//!
//! Worktree directories exist under `root` for `ws-pinned` (`.git` directory) and `ws-iso`
//! (`.git` file); `ws-space` has a directory without a `.git` entry.

use std::path::Path;

use rusqlite::{params, Connection};

struct Repo<'a> {
    id: &'a str,
    name: &'a str,
    root_path: Option<String>,
    default_branch: Option<&'a str>,
    icon: Option<&'a str>,
    remote_url: Option<&'a str>,
    display_order: Option<i64>,
    hidden: i64,
}

struct Workspace<'a> {
    id: &'a str,
    repo: &'a str,
    directory_name: &'a str,
    branch: Option<&'a str>,
    state: Option<&'a str>,
    pinned_at: Option<&'a str>,
    created_at: &'a str,
    updated_at: &'a str,
    workspace_name: Option<&'a str>,
    pr_title: Option<&'a str>,
    derived_status: Option<&'a str>,
    manual_status: Option<&'a str>,
    active_session_id: Option<&'a str>,
    intended_target_branch: Option<&'a str>,
}

struct Session<'a> {
    id: &'a str,
    workspace_id: Option<&'a str>,
    status: &'a str,
    title: &'a str,
    model: Option<&'a str>,
    agent_type: Option<&'a str>,
    unread_count: i64,
    is_hidden: i64,
    updated_at: &'a str,
}

pub fn seed(conn: &Connection, root: &Path) {
    let repo_dir = |name: &str| {
        root.join("_repos")
            .join(name)
            .to_string_lossy()
            .into_owned()
    };

    // The anvil checkout holds an icon file; the quartz checkout does not exist.
    let anvil = root.join("_repos").join("anvil");
    std::fs::create_dir_all(anvil.join("public")).expect("anvil icon directory");
    std::fs::write(anvil.join("public/favicon.svg"), "<svg/>").expect("anvil icon file");

    // Worktree directories: one with a `.git` directory, one with a `.git` file, one with none.
    std::fs::create_dir_all(root.join("lantern/attic/.git")).expect("attic worktree");
    std::fs::create_dir_all(root.join("anvil/forge")).expect("forge worktree");
    std::fs::write(root.join("anvil/forge/.git"), "gitdir: /nowhere\n").expect("forge .git");
    std::fs::create_dir_all(root.join("compass/needle")).expect("needle directory");

    let repos = [
        Repo {
            id: "ws-repo-lantern",
            name: "lantern",
            root_path: Some(repo_dir("lantern")),
            default_branch: Some("main"),
            icon: Some("emoji:\u{1F3EE}"),
            remote_url: Some("https://github.com/lantern-works/lantern.git"),
            display_order: Some(3),
            hidden: 0,
        },
        Repo {
            id: "ws-repo-compass",
            name: "compass",
            root_path: None,
            default_branch: Some("develop"),
            icon: Some("book"),
            remote_url: None,
            display_order: Some(2),
            hidden: 0,
        },
        Repo {
            id: "ws-repo-anvil",
            name: "anvil",
            root_path: Some(anvil.to_string_lossy().into_owned()),
            default_branch: Some("trunk"),
            icon: None,
            remote_url: Some("https://github.com/anvil-co/anvil"),
            display_order: Some(1),
            hidden: 0,
        },
        Repo {
            id: "ws-repo-quartz",
            name: "quartz",
            root_path: Some(repo_dir("quartz")),
            default_branch: None,
            icon: None,
            remote_url: Some("git@github.com:quartz-labs/quartz.git"),
            display_order: Some(4),
            hidden: 0,
        },
        Repo {
            id: "ws-repo-relic",
            name: "relic",
            root_path: Some(repo_dir("relic")),
            default_branch: Some("main"),
            icon: None,
            remote_url: Some("https://example.org/relic.git"),
            display_order: Some(5),
            hidden: 0,
        },
        // No workspace at all: these sort last, by display order and then without one.
        Repo {
            id: "ws-repo-hollow",
            name: "hollow",
            root_path: Some(repo_dir("hollow")),
            default_branch: Some("main"),
            icon: Some("emoji:  "),
            remote_url: Some("https://github.com/hollow-org/hollow.git"),
            display_order: Some(1),
            hidden: 0,
        },
        Repo {
            id: "ws-repo-plain",
            name: "plain",
            root_path: None,
            default_branch: Some("main"),
            icon: None,
            remote_url: None,
            display_order: None,
            hidden: 0,
        },
        Repo {
            id: "ws-repo-hidden",
            name: "hidden",
            root_path: None,
            default_branch: Some("main"),
            icon: None,
            remote_url: None,
            display_order: Some(0),
            hidden: 1,
        },
    ];
    for r in &repos {
        conn.execute(
            "INSERT INTO repos (id, name, root_path, default_branch, icon, remote_url, display_order, hidden)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                r.id,
                r.name,
                r.root_path,
                r.default_branch,
                r.icon,
                r.remote_url,
                r.display_order,
                r.hidden
            ],
        )
        .expect("insert repo");
    }

    let workspaces = [
        // Pinned, so first although the oldest.
        Workspace {
            id: "ws-pinned",
            repo: "ws-repo-lantern",
            directory_name: "attic",
            branch: Some("feat/lamp-oil"),
            state: Some("ready"),
            pinned_at: Some("2026-02-01T09:00:00.000Z"),
            created_at: "2026-01-10 12:00:00",
            updated_at: "2026-01-11T12:00:00.000Z",
            workspace_name: Some("Lamp oil"),
            pr_title: Some("Add lamp oil"),
            derived_status: Some("in-progress"),
            manual_status: None,
            active_session_id: Some("ws-sess-working"),
            intended_target_branch: Some("release/2"),
        },
        // ISO text, 01:00: sorts above "2026-03-05 23:00:00" in a raw compare ('T' > ' ').
        Workspace {
            id: "ws-iso",
            repo: "ws-repo-anvil",
            directory_name: "forge",
            branch: Some("fix/hammer"),
            state: Some("ready"),
            pinned_at: None,
            created_at: "2026-03-01 00:00:00",
            updated_at: "2026-03-05T01:00:00.000Z",
            workspace_name: None,
            pr_title: Some("Fix the hammer"),
            derived_status: Some("in-progress"),
            manual_status: Some("review"),
            active_session_id: Some("ws-sess-codex"),
            intended_target_branch: None,
        },
        // An empty target branch falls through to the repository's default branch.
        Workspace {
            id: "ws-space",
            repo: "ws-repo-compass",
            directory_name: "needle",
            branch: Some("needle-work"),
            state: Some("ready"),
            pinned_at: None,
            created_at: "2026-03-02 00:00:00",
            updated_at: "2026-03-05 23:00:00",
            workspace_name: Some("Needle"),
            pr_title: None,
            derived_status: Some("in-progress"),
            manual_status: None,
            active_session_id: None,
            intended_target_branch: Some(""),
        },
        // Unread chats: see the sessions below.
        Workspace {
            id: "ws-unread",
            repo: "ws-repo-lantern",
            directory_name: "cellar",
            branch: Some("cellar-fix"),
            state: Some("ready"),
            pinned_at: None,
            created_at: "2026-02-25 00:00:00",
            updated_at: "2026-03-01 00:00:00",
            workspace_name: Some("Cellar"),
            pr_title: None,
            derived_status: None,
            manual_status: None,
            active_session_id: None,
            intended_target_branch: None,
        },
        // Neither a target branch nor a default branch: falls back to "main".
        Workspace {
            id: "ws-setting-up",
            repo: "ws-repo-quartz",
            directory_name: "geode",
            branch: None,
            state: Some("setting_up"),
            pinned_at: None,
            created_at: "2026-02-20 09:00:00",
            updated_at: "2026-02-20 10:00:00",
            workspace_name: None,
            pr_title: None,
            derived_status: Some("in-progress"),
            manual_status: None,
            active_session_id: None,
            intended_target_branch: None,
        },
        // Archived: no live workspace, but it still counts for its repository and can be looked up.
        Workspace {
            id: "ws-archived",
            repo: "ws-repo-relic",
            directory_name: "vault",
            branch: Some("old-work"),
            state: Some("archived"),
            pinned_at: None,
            created_at: "2026-01-02 00:00:00",
            updated_at: "2026-03-09T00:00:00.000Z",
            workspace_name: Some("Vault"),
            pr_title: Some("Old work"),
            derived_status: Some("done"),
            manual_status: None,
            active_session_id: None,
            intended_target_branch: None,
        },
        // Conductor's column default is 'active': neither live nor archived.
        Workspace {
            id: "ws-active",
            repo: "ws-repo-lantern",
            directory_name: "loft",
            branch: Some("loft"),
            state: Some("active"),
            pinned_at: None,
            created_at: "2025-12-01 00:00:00",
            updated_at: "2025-12-01 00:00:00",
            workspace_name: None,
            pr_title: None,
            derived_status: None,
            manual_status: None,
            active_session_id: None,
            intended_target_branch: None,
        },
    ];
    for w in &workspaces {
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, directory_name, branch, state, pinned_at,
                                     created_at, updated_at, workspace_name, pr_title, derived_status,
                                     manual_status, active_session_id, intended_target_branch)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                w.id,
                w.repo,
                w.directory_name,
                w.branch,
                w.state,
                w.pinned_at,
                w.created_at,
                w.updated_at,
                w.workspace_name,
                w.pr_title,
                w.derived_status,
                w.manual_status,
                w.active_session_id,
                w.intended_target_branch
            ],
        )
        .expect("insert workspace");
    }

    let sessions = [
        // The active chats of `ws-pinned` and `ws-iso`.
        Session {
            id: "ws-sess-working",
            workspace_id: Some("ws-pinned"),
            status: "working",
            title: "Refill the lamps",
            model: Some("sonnet"),
            agent_type: Some("claude"),
            unread_count: 0,
            is_hidden: 0,
            updated_at: "2026-01-11 12:00:00",
        },
        Session {
            id: "ws-sess-codex",
            workspace_id: Some("ws-iso"),
            status: "idle",
            title: "Hammer repair",
            model: Some("gpt-5"),
            agent_type: Some("codex"),
            unread_count: 0,
            is_hidden: 0,
            updated_at: "2026-03-05 01:00:00",
        },
        // Unread chats of `ws-unread`, one per timestamp format; `at` is shipped as stored.
        Session {
            id: "ws-sess-unread-a",
            workspace_id: Some("ws-unread"),
            status: "idle",
            title: "Cellar damp",
            model: Some("sonnet"),
            agent_type: Some("claude"),
            unread_count: 1,
            is_hidden: 0,
            updated_at: "2026-03-02 09:00:00",
        },
        Session {
            id: "ws-sess-unread-b",
            workspace_id: Some("ws-unread"),
            status: "idle",
            title: "Cellar shelves",
            model: Some("sonnet"),
            agent_type: Some("claude"),
            unread_count: 1,
            is_hidden: 0,
            updated_at: "2026-03-02T09:30:00.000Z",
        },
        // Not counted: read, hidden, and without a workspace.
        Session {
            id: "ws-sess-read",
            workspace_id: Some("ws-unread"),
            status: "idle",
            title: "Cellar done",
            model: None,
            agent_type: None,
            unread_count: 0,
            is_hidden: 0,
            updated_at: "2026-03-02 10:00:00",
        },
        Session {
            id: "ws-sess-hidden",
            workspace_id: Some("ws-unread"),
            status: "idle",
            title: "Cellar closed",
            model: None,
            agent_type: None,
            unread_count: 1,
            is_hidden: 1,
            updated_at: "2026-03-02 11:00:00",
        },
        Session {
            id: "ws-sess-orphan",
            workspace_id: None,
            status: "idle",
            title: "Nowhere",
            model: None,
            agent_type: None,
            unread_count: 1,
            is_hidden: 0,
            updated_at: "2026-03-02 12:00:00",
        },
    ];
    for s in &sessions {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, status, title, model, agent_type, unread_count,
                                   is_hidden, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                s.id,
                s.workspace_id,
                s.status,
                s.title,
                s.model,
                s.agent_type,
                s.unread_count,
                s.is_hidden,
                s.updated_at
            ],
        )
        .expect("insert session");
    }
}
