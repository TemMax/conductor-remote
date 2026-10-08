//! The reads that attach background facts (change stats, pull requests, Run tasks, background
//! tasks), and the cached answers that follow them.

#[path = "support/seed_extras.rs"]
mod seed_extras;
mod support;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use conductor_remote::reads::extras::commands::{CommandError, Commands, Limits, Output};
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::snapshot::{Key, Snapshot};
use conductor_remote::reads::{ReadError, Reads};
use conductor_remote::testing::FakeCommands;
use serde_json::{json, Value};
use support::TestDb;

const HOME: &str = "/Users/tester";

/// Runs every command on the wrapped fake, but only once the gate is open: a test can look at the
/// answers while no background work has finished.
struct Gated {
    fake: Arc<FakeCommands>,
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gated {
    fn set_open(&self, open: bool) {
        *self.open.lock().unwrap() = open;
        self.changed.notify_all();
    }
}

impl Commands for Gated {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        let guard = self.open.lock().unwrap();
        // Bounded, so a test that fails with the gate closed does not hang its pool threads.
        let _open = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(20), |open| !*open)
            .unwrap();
        self.fake.run(program, args, cwd, limits)
    }
}

struct Setup {
    test: TestDb,
    reads: Reads,
    extras: Arc<Extras>,
    fake: Arc<FakeCommands>,
    gate: Arc<Gated>,
    worktree: String,
}

fn pull_request(branch: &str, number: i64, state: &str, draft: bool) -> Value {
    json!({
        "headRefName": branch,
        "number": number,
        "url": format!("https://example.test/pull/{number}"),
        "state": state,
        "isDraft": draft,
        "updatedAt": "2026-03-01T00:00:00Z",
        "statusCheckRollup": [],
    })
}

/// A seeded database with a worktree for `ext-workspace`, the commands scripted so that every
/// background refresh finds something, and the gate closed.
fn setup(pull_requests: &[Value]) -> Setup {
    let test = TestDb::new();
    seed_extras::seed(&test.conn());
    let worktree: PathBuf = test
        .root()
        .join(seed_extras::REPO_NAME)
        .join(seed_extras::DIRECTORY);
    std::fs::create_dir_all(worktree.join(".git")).expect("worktree directory");
    let worktree = worktree.to_string_lossy().into_owned();

    let fake = Arc::new(FakeCommands::new());
    let wt = worktree.as_str();
    fake.on("git", &["-C", wt, "rev-parse"], 0, "");
    fake.on("git", &["-C", wt, "merge-base"], 0, "abc123\n");
    fake.on(
        "git",
        &["-C", wt, "diff", "--numstat"],
        0,
        "10\t2\tsrc/a.rs\n3\t1\tb.rs\n",
    );
    fake.on("git", &["-C", wt, "ls-files"], 0, "");
    fake.on(
        "gh",
        &["pr", "list"],
        0,
        &Value::from(pull_requests).to_string(),
    );
    let run_key = worktree.replace('/', "--");
    fake.on(
        "ps",
        &["-axww"],
        0,
        &format!("/sbin/launchd\nzsh {HOME}/.conductor/projects/{run_key}/run-run:1.sh\n"),
    );
    fake.on(
        "ps",
        &["-axo"],
        0,
        &format!("100 10:00 claude --resume={}\n", seed_extras::LIVE_CHAT),
    );

    let gate = Arc::new(Gated {
        fake: fake.clone(),
        open: Mutex::new(false),
        changed: Condvar::new(),
    });
    let commands: Arc<dyn Commands> = gate.clone();
    let extras = Arc::new(Extras::new(commands, HOME));
    let reads = Reads::new(test.db(), test.root()).with_extras(extras.clone());
    Setup {
        test,
        reads,
        extras,
        fake,
        gate,
        worktree,
    }
}

impl Setup {
    /// Lets the commands run and waits until every queued refresh has finished.
    fn settle(&self) {
        self.gate.set_open(true);
        self.extras.wait_idle();
    }

    /// The state body as `GET /api/state` caches it: one build per database version and extras
    /// revision. `builds` counts the builds.
    fn state(&self, builds: &Cell<usize>) -> Vec<Value> {
        let body = self
            .reads
            .snapshot()
            .get_or_build(self.reads.db(), Key::State, || {
                builds.set(builds.get() + 1);
                let list = self.reads.list_workspaces()?;
                Ok::<_, ReadError>(serde_json::to_vec(&list).unwrap())
            })
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }
}

fn workspace<'a>(list: &'a [Value], id: &str) -> &'a Value {
    list.iter()
        .find(|w| w["id"] == id)
        .unwrap_or_else(|| panic!("no workspace {id}"))
}

fn draft_and_merged() -> Vec<Value> {
    vec![
        pull_request(seed_extras::BRANCH_DRAFT, 11, "OPEN", true),
        pull_request(seed_extras::BRANCH_MERGED, 12, "MERGED", false),
    ]
}

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

fn sessions(reads: &Reads) -> Vec<Value> {
    match serde_json::to_value(reads.list_sessions(seed_extras::WORKSPACE).unwrap()).unwrap() {
        Value::Array(items) => items,
        other => panic!("expected an array, got {other}"),
    }
}

fn tasks_of<'a>(list: &'a [Value], id: &str) -> &'a [Value] {
    list.iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no chat {id}"))["background_tasks"]
        .as_array()
        .expect("an array")
}

// ---------------------------------------------------------------- workspaces

#[test]
fn the_first_state_has_no_stats_no_pull_request_and_no_run() {
    let setup = setup(&draft_and_merged());
    let builds = Cell::new(0);
    let list = setup.state(&builds);
    assert_eq!(list.len(), 2);
    for ws in &list {
        assert_eq!(ws["change_stats"], Value::Null);
        assert_eq!(ws["pr_status"], Value::Null);
        assert_eq!(ws["pr_number"], Value::Null);
        assert_eq!(ws["pr_url"], Value::Null);
        assert_eq!(ws["run_active"], json!(false));
    }
    // The worktree is known at once: it comes from the file system, not from a background job.
    assert_eq!(
        workspace(&list, seed_extras::WORKSPACE)["worktree"],
        json!(setup.worktree)
    );
    setup.settle();
}

#[test]
fn a_finished_refresh_shows_in_the_next_state() {
    let setup = setup(&draft_and_merged());
    let builds = Cell::new(0);
    let first = setup.state(&builds);
    assert_eq!(
        workspace(&first, seed_extras::WORKSPACE)["change_stats"],
        Value::Null
    );
    assert_eq!(builds.get(), 1);

    let version = setup.reads.db().data_version().unwrap();
    setup.settle();
    assert!(
        setup.reads.db().data_version().unwrap() == version,
        "only the extras changed, not the database"
    );

    let next = setup.state(&builds);
    assert_eq!(builds.get(), 2, "the moved revision rebuilt the body");

    let with_worktree = workspace(&next, seed_extras::WORKSPACE);
    assert_eq!(
        with_worktree["change_stats"],
        json!({"added": 13, "removed": 3})
    );
    assert_eq!(with_worktree["pr_status"], json!("draft"));
    assert_eq!(with_worktree["pr_number"], json!(11));
    assert_eq!(
        with_worktree["pr_url"],
        json!("https://example.test/pull/11")
    );
    assert_eq!(with_worktree["run_active"], json!(true));

    // Without a worktree there are no stats and no Run task, but the pull request is known.
    let bare = workspace(&next, seed_extras::WORKSPACE_BARE);
    assert_eq!(bare["worktree"], Value::Null);
    assert_eq!(bare["change_stats"], Value::Null);
    assert_eq!(bare["pr_status"], json!("merged"));
    assert_eq!(bare["pr_number"], json!(12));
    assert_eq!(bare["pr_url"], json!("https://example.test/pull/12"));
    assert_eq!(bare["run_active"], json!(false));
}

#[test]
fn an_open_pull_request_shows_its_conflict_one_refresh_later() {
    let setup = setup(&[pull_request(seed_extras::BRANCH_DRAFT, 7, "OPEN", false)]);
    setup
        .fake
        .on("git", &["-C", &setup.worktree, "merge-tree"], 1, "");
    let builds = Cell::new(0);
    setup.state(&builds);
    setup.settle();

    // The pull request is known; whether it conflicts is only being asked.
    let second = setup.state(&builds);
    assert_eq!(
        workspace(&second, seed_extras::WORKSPACE)["pr_status"],
        json!("mergeable")
    );
    setup.extras.wait_idle();

    let third = setup.state(&builds);
    let ws = workspace(&third, seed_extras::WORKSPACE);
    assert_eq!(ws["pr_status"], json!("conflicts"));
    assert_eq!(ws["pr_number"], json!(7));
    assert_eq!(builds.get(), 3, "each finished refresh rebuilt the body");
}

#[test]
fn an_unchanged_revision_reuses_the_body() {
    let setup = setup(&draft_and_merged());
    let builds = Cell::new(0);
    setup.state(&builds);
    setup.settle();
    let settled = setup.state(&builds);
    setup.extras.wait_idle();
    let built = builds.get();

    for _ in 0..3 {
        assert_eq!(setup.state(&builds), settled);
    }
    assert_eq!(builds.get(), built);
}

#[test]
fn a_build_that_finishes_after_the_revision_moved_is_not_kept() {
    let setup = setup(&draft_and_merged());
    let builds = Cell::new(0);
    let snapshot = setup.reads.snapshot();
    let build = || {
        builds.set(builds.get() + 1);
        setup.extras.shared().revision.bump();
        Ok::<_, ReadError>(b"body".to_vec())
    };
    snapshot
        .get_or_build(setup.reads.db(), Key::State, build)
        .unwrap();
    snapshot
        .get_or_build(setup.reads.db(), Key::State, build)
        .unwrap();
    assert_eq!(builds.get(), 2);
    setup.settle();
}

// ---------------------------------------------------------------- chats

#[test]
fn a_chat_with_a_live_agent_lists_its_open_tasks_and_a_chat_without_one_lists_none() {
    let setup = setup(&[]);
    let first = sessions(&setup.reads);
    assert_eq!(first.len(), 2);
    assert!(tasks_of(&first, seed_extras::LIVE_CHAT).is_empty());
    assert!(tasks_of(&first, seed_extras::IDLE_CHAT).is_empty());

    setup.settle();
    let next = sessions(&setup.reads);
    let open = tasks_of(&next, seed_extras::LIVE_CHAT);
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["taskId"], json!("task-open"));
    assert_eq!(open[0]["description"], json!("Watch the build"));
    assert_eq!(open[0]["since"], json!("2099-01-01 00:00:01"));
    // A frame is in the database for this chat too, but no process owns it.
    assert!(tasks_of(&next, seed_extras::IDLE_CHAT).is_empty());
}

#[test]
fn a_chat_list_body_older_than_its_maximum_age_is_rebuilt() {
    let setup = setup(&[]);
    let revision = setup.extras.shared().revision.clone();
    let key = Key::Sessions(seed_extras::WORKSPACE.to_owned());
    let builds = Cell::new(0);
    let build = || {
        builds.set(builds.get() + 1);
        Ok::<_, ReadError>(format!("build {}", builds.get()).into_bytes())
    };
    let db = setup.reads.db();

    // A maximum age of zero: every call is older than it, and nothing sleeps.
    let impatient = Snapshot::with_revision(revision.clone()).with_sessions_max_age(Duration::ZERO);
    assert_eq!(
        impatient.get_or_build(db, key.clone(), build).unwrap(),
        b"build 1"
    );
    assert_eq!(
        impatient.get_or_build(db, key.clone(), build).unwrap(),
        b"build 2"
    );

    // With extras the default is 5 seconds: the second call reuses the body.
    let patient = Snapshot::with_revision(revision);
    assert_eq!(
        patient.get_or_build(db, key.clone(), build).unwrap(),
        b"build 3"
    );
    assert_eq!(
        patient.get_or_build(db, key.clone(), build).unwrap(),
        b"build 3"
    );
    assert_eq!(builds.get(), 3);
    setup.settle();
}

// ---------------------------------------------------------------- golden files

#[test]
fn golden_workspaces_with_extras() {
    let setup = setup(&draft_and_merged());
    let builds = Cell::new(0);
    setup.state(&builds);
    setup.settle();
    let list = setup.state(&builds);
    assert_eq!(
        without_root(Value::from(list), setup.test.root()),
        golden("workspaces-extras.json")
    );
}

#[test]
fn golden_sessions_with_background_tasks() {
    let setup = setup(&[]);
    sessions(&setup.reads);
    setup.settle();
    let list = sessions(&setup.reads);
    assert_eq!(
        without_root(Value::from(list), setup.test.root()),
        golden("sessions-background.json")
    );
}
