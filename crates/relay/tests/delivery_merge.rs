//! Merging a workspace's pull request: every answer, the two `gh` argument lists and what reaches
//! the phone when `gh` fails. `gh` is a `FakeCommands`, the database a synthetic one; nothing here
//! runs a real program or reaches the Mac.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{WriteAnswer, WriteService};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::testing::{CommandCall, FakeCommands};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::UiActor;
use conductor_remote::ui::fake::{conductor_app, FakeDesktop, WindowSpec};
use serde_json::{json, Value};
use support::TestDb;

const WORKSPACE: &str = "ws-1";
const BRANCH: &str = "user/feature-x";
const VIEW_FIELDS: &str = "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed";

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

fn timings() -> WriteTimings {
    WriteTimings {
        delivery: DeliveryTimings {
            confirm_window: ms(60),
            poll: ms(10),
            min_attempt: ms(50),
            min_confirm: ms(10),
            retry_pause: ms(10),
        },
        stop_poll: ms(10),
        stop_checks: 5,
        chat_poll: ms(10),
        chat_checks: 4,
        send_budget: Some(ms(400)),
        restore_poll: ms(10),
        restore_checks: 3,
        create_poll: ms(10),
        create_checks: 3,
    }
}

/// The workspace row of a test: its branch and its repository's `root_path`.
struct Seed {
    branch: Option<&'static str>,
    root: Option<PathBuf>,
}

struct Rig {
    // Kept alive: the database files live in it.
    _test: TestDb,
    writes: Writes,
    commands: Arc<FakeCommands>,
    root: PathBuf,
}

impl Rig {
    /// One live workspace `ws-1` of repo "relay" on `BRANCH`, whose checkout is `root`.
    fn new() -> Rig {
        Rig::seeded(|root| Seed {
            branch: Some(BRANCH),
            root: Some(root),
        })
    }

    fn seeded(seed: impl FnOnce(PathBuf) -> Seed) -> Rig {
        let test = TestDb::new();
        let root = test.dir().join("checkout");
        let seed = seed(root.clone());
        let conn = test.conn();
        conn.execute(
            "INSERT INTO repos (id, name, root_path) VALUES ('r-1', 'relay', ?1)",
            [seed
                .root
                .as_ref()
                .map(|root| root.to_string_lossy().into_owned())],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, state) \
             VALUES (?1, ?1, 'r-1', ?2, 'beta', 'ready')",
            rusqlite::params![WORKSPACE, seed.branch],
        )
        .unwrap();

        let ui = UiActor::spawn(|| {
            let app = conductor_app(&WindowSpec {
                repo: "relay".to_owned(),
                branch: BRANCH.to_owned(),
                sidebar: vec!["beta".to_owned()],
                chats: vec!["One".to_owned()],
                selected: 0,
                composer_value: None,
            });
            Box::new(Driver::new(FakeDesktop::new(app)))
        });
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let store = Arc::new(Store::open_in_memory().expect("an in-memory store"));
        let parked = ParkedQueue::new(
            Arc::clone(&store),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let commands = Arc::new(FakeCommands::new());
        let writes =
            Writes::new(reads, ui, Arc::new(|| true), timings(), parked).configure(WriteDeps {
                state_dir: test.dir().join("state"),
                store,
                commands: Arc::clone(&commands)
                    as Arc<dyn conductor_remote::reads::extras::commands::Commands>,
                locked: Arc::new(|| Some(false)),
            });
        Rig {
            _test: test,
            writes,
            commands,
            root,
        }
    }

    async fn merge(&self) -> WriteAnswer {
        self.writes.merge(WORKSPACE.to_owned()).await
    }

    /// The arguments of every `gh` call, in order.
    fn gh_args(&self) -> Vec<Vec<String>> {
        self.commands
            .calls()
            .into_iter()
            .map(|call| {
                assert_eq!(call.program, "gh");
                call.args
            })
            .collect()
    }

    /// `gh repo view` allows exactly these methods.
    fn allows(&self, squash: bool, merge: bool, rebase: bool) {
        let body = json!({
            "squashMergeAllowed": squash,
            "mergeCommitAllowed": merge,
            "rebaseMergeAllowed": rebase,
        })
        .to_string();
        self.commands.on("gh", &["repo", "view"], 0, &body);
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn view_args() -> Vec<String> {
    strings(&["repo", "view", "--json", VIEW_FIELDS])
}

fn merge_args(method: &str) -> Vec<String> {
    strings(&["pr", "merge", BRANCH, &format!("--{method}")])
}

fn assert_answer(answer: &WriteAnswer, status: u16, body: Value) {
    assert_eq!(answer.status, status, "body: {}", answer.body);
    assert_eq!(answer.body, body);
}

// ---- the answers ----

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_workspace_is_a_404() {
    let rig = Rig::new();
    let answer = rig.writes.merge("ws-nope".to_owned()).await;
    assert_answer(&answer, 404, json!({ "error": "workspace not found" }));
    assert!(rig.commands.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_without_a_branch_is_a_409_and_runs_nothing() {
    let rig = Rig::seeded(|root| Seed {
        branch: None,
        root: Some(root),
    });
    let answer = rig.merge().await;
    assert_answer(
        &answer,
        409,
        json!({ "ok": false, "branch": "", "error": "workspace has no branch" }),
    );
    assert!(rig.commands.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_branch_starting_with_a_dash_is_refused_and_runs_nothing() {
    let rig = Rig::seeded(|root| Seed {
        branch: Some("--delete-branch"),
        root: Some(root),
    });
    rig.allows(true, true, true);
    rig.commands.on("gh", &["pr", "merge"], 0, "");
    let answer = rig.merge().await;
    assert_answer(
        &answer,
        409,
        json!({
            "ok": false,
            "branch": "--delete-branch",
            "error": "workspace has no branch",
        }),
    );
    assert!(rig.commands.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_without_a_root_is_a_409_and_runs_nothing() {
    let rig = Rig::seeded(|_| Seed {
        branch: Some(BRANCH),
        root: None,
    });
    let answer = rig.merge().await;
    assert_answer(
        &answer,
        409,
        json!({ "ok": false, "branch": BRANCH, "error": "repo root unresolved" }),
    );
    assert!(rig.commands.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merged_pull_request_is_a_200_with_the_method() {
    let rig = Rig::new();
    rig.allows(true, true, true);
    rig.commands.on("gh", &["pr", "merge"], 0, "");
    let answer = rig.merge().await;
    assert_answer(
        &answer,
        200,
        json!({ "ok": true, "branch": BRANCH, "method": "squash" }),
    );
}

// ---- the two argument lists ----

#[tokio::test(flavor = "multi_thread")]
async fn gh_gets_exactly_the_two_argument_lists_in_the_repo_root() {
    let rig = Rig::new();
    rig.allows(true, false, false);
    rig.commands.on("gh", &["pr", "merge"], 0, "");
    rig.merge().await;

    assert_eq!(rig.gh_args(), vec![view_args(), merge_args("squash")]);
    let calls: Vec<CommandCall> = rig.commands.calls();
    for call in &calls {
        assert_eq!(call.cwd.as_deref(), Some(rig.root.as_path()));
        assert_eq!(call.limits.timeout, Duration::from_secs(30));
    }
}

// ---- the method ----

#[tokio::test(flavor = "multi_thread")]
async fn the_method_is_squash_then_merge_then_rebase_by_what_is_allowed() {
    for (allowed, method) in [
        ((true, true, true), "squash"),
        ((false, true, true), "merge"),
        ((false, false, true), "rebase"),
        ((false, false, false), "squash"),
    ] {
        let rig = Rig::new();
        rig.allows(allowed.0, allowed.1, allowed.2);
        rig.commands.on("gh", &["pr", "merge"], 0, "");
        let answer = rig.merge().await;
        assert_answer(
            &answer,
            200,
            json!({ "ok": true, "branch": BRANCH, "method": method }),
        );
        assert_eq!(rig.gh_args(), vec![view_args(), merge_args(method)]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_repo_view_falls_back_to_squash() {
    // Exits non-zero, cannot start, times out, prints something else: each is squash.
    let setups: [fn(&FakeCommands); 4] = [
        |commands| commands.on_with_stderr("gh", &["repo", "view"], 1, "", "not a repository"),
        |_| {},
        |commands| commands.fail("gh", &["repo", "view"]),
        |commands| commands.on("gh", &["repo", "view"], 0, "not json"),
    ];
    for setup in setups {
        let rig = Rig::new();
        setup(&rig.commands);
        rig.commands.on("gh", &["pr", "merge"], 0, "");
        let answer = rig.merge().await;
        assert_answer(
            &answer,
            200,
            json!({ "ok": true, "branch": BRANCH, "method": "squash" }),
        );
        assert_eq!(rig.gh_args(), vec![view_args(), merge_args("squash")]);
    }
}

// ---- when gh fails ----

#[tokio::test(flavor = "multi_thread")]
async fn gh_stderr_reaches_the_body_trimmed() {
    let rig = Rig::new();
    rig.allows(false, true, false);
    rig.commands.on_with_stderr(
        "gh",
        &["pr", "merge"],
        1,
        "ignored",
        "\n X Pull request is not mergeable: the base branch policy prohibits the merge\n",
    );
    let answer = rig.merge().await;
    assert_answer(
        &answer,
        409,
        json!({
            "ok": false,
            "branch": BRANCH,
            "method": "merge",
            "error": "X Pull request is not mergeable: the base branch policy prohibits the merge",
        }),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_failure_names_the_exit_code() {
    let rig = Rig::new();
    rig.allows(true, false, false);
    rig.commands
        .on_with_stderr("gh", &["pr", "merge"], 4, "", "  \n");
    let answer = rig.merge().await;
    assert_answer(
        &answer,
        409,
        json!({
            "ok": false,
            "branch": BRANCH,
            "method": "squash",
            "error": "gh exited with 4",
        }),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_error_reaches_the_body_as_its_text() {
    // A time-out of the merge itself.
    let rig = Rig::new();
    rig.allows(true, false, false);
    rig.commands.fail("gh", &["pr", "merge"]);
    let answer = rig.merge().await;
    assert_answer(
        &answer,
        409,
        json!({
            "ok": false,
            "branch": BRANCH,
            "method": "squash",
            "error": "gh timed out",
        }),
    );

    // A `gh` that cannot start: no rule answers, so the fake reports a spawn failure.
    let rig = Rig::new();
    let answer = rig.merge().await;
    assert_answer(
        &answer,
        409,
        json!({
            "ok": false,
            "branch": BRANCH,
            "method": "squash",
            "error": "could not start gh",
        }),
    );
    assert_eq!(rig.gh_args(), vec![view_args(), merge_args("squash")]);
}
