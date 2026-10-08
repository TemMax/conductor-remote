//! The first-prompt queue over an in-memory store and a synthetic Conductor database, with a fake
//! window (a backend that records its sends) and a fake lock probe, in paused time. The writes'
//! own pending list and dismiss run through `Writes` over the fake desktop.

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_remote::delivery::deliver::DeliveryTimings;
use conductor_remote::delivery::firstprompt::{
    first_prompt_json, inspect_target, materialize_staged, staging_root, Clock, FirstPromptBackend,
    FirstPromptQueue, FirstPromptTarget, Phase, SendOutcome, KEEP_FAILED, MAX_ATTEMPTS,
    MAX_EARLY_ATTEMPTS, NEVER_SET_UP, PRUNE_EVERY,
};
use conductor_remote::delivery::parked::{LockProbe, ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{BoxFuture, WriteService};
use conductor_remote::files::attachments::{write_attachment, ATTACHMENTS_DIR};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::{FirstPromptRow, NewFirstPrompt, ParkedStatus, Store};
use conductor_remote::testing::FakeCommands;
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::UiActor;
use conductor_remote::ui::fake::{conductor_app, FakeDesktop, WindowSpec};
use serde_json::json;
use support::TestDb;

const WS: &str = "ws-1";
const CHAT: &str = "chat-1";
const DAY_MS: i64 = 24 * 3600 * 1000;
const NO_COMPOSER: &str = "no composer yet";

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

/// Lets the pump run, then moves paused time on by `millis`.
async fn flush(millis: u64) {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(ms(millis.max(1))).await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

fn wall_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// The wall clock as of the test's start, moved on by paused time.
fn paused_clock() -> Clock {
    let base = wall_ms();
    let start = tokio::time::Instant::now();
    Arc::new(move || base + i64::try_from(start.elapsed().as_millis()).unwrap())
}

/// The fake window: the target comes from the synthetic database through `inspect_target`, read
/// inline so paused time stays deterministic; sends and copies are recorded.
struct Backend {
    reads: Reads,
    staging: PathBuf,
    inspects: AtomicUsize,
    outcome: Mutex<SendOutcome>,
    sends: Mutex<Vec<String>>,
    /// `materialize:<ids>` and `send:<text>`, in order.
    events: Mutex<Vec<String>>,
    closed: AtomicBool,
}

impl Backend {
    fn sends(&self) -> Vec<String> {
        self.sends.lock().unwrap().clone()
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn inspects(&self) -> usize {
        self.inspects.load(Ordering::SeqCst)
    }

    fn answer(&self, outcome: SendOutcome) {
        *self.outcome.lock().unwrap() = outcome;
    }
}

impl FirstPromptBackend for Backend {
    fn inspect(&self, workspace_id: &str) -> BoxFuture<Result<Option<FirstPromptTarget>, String>> {
        self.inspects.fetch_add(1, Ordering::SeqCst);
        let target = inspect_target(&self.reads, workspace_id).map_err(|error| error.to_string());
        Box::pin(async move { target })
    }

    fn materialize(
        &self,
        worktree: PathBuf,
        attachment_ids: Vec<String>,
    ) -> BoxFuture<Result<(), String>> {
        self.events
            .lock()
            .unwrap()
            .push(format!("materialize:{}", attachment_ids.join(",")));
        let copied = materialize_staged(&self.staging, &worktree, &attachment_ids);
        Box::pin(async move { copied })
    }

    fn send(&self, _workspace_id: &str, _session_id: &str, text: &str) -> BoxFuture<SendOutcome> {
        self.sends.lock().unwrap().push(text.to_owned());
        self.events.lock().unwrap().push(format!("send:{text}"));
        let outcome = self.outcome.lock().unwrap().clone();
        Box::pin(async move { outcome })
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// A synthetic database with repo "relay"; the workspace `ws-1` and its chat are added by the
/// tests that want them.
struct Rig {
    test: TestDb,
    store: Arc<Store>,
    lock: Arc<Mutex<Option<bool>>>,
    clock: Clock,
}

impl Rig {
    fn new() -> Rig {
        let test = TestDb::new();
        test.conn()
            .execute("INSERT INTO repos (id, name) VALUES ('r-1', 'relay')", [])
            .unwrap();
        Rig {
            test,
            store: Arc::new(Store::open_in_memory().expect("an in-memory store")),
            lock: Arc::new(Mutex::new(Some(false))),
            clock: paused_clock(),
        }
    }

    /// `ws-1` in `state` (directory "beta" of repo "relay") with one open chat, `chat-1`.
    fn with_workspace(state: &str) -> Rig {
        let rig = Rig::new();
        rig.add_workspace(state);
        rig.add_chat(CHAT, "2026-10-01 10:00:00", false);
        rig
    }

    fn add_workspace(&self, state: &str) {
        self.test
            .conn()
            .execute(
                "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
                 directory_name, state) VALUES (?1, ?1, 'r-1', 'user/x', 'beta', 'beta', ?2)",
                [WS, state],
            )
            .unwrap();
    }

    fn add_chat(&self, id: &str, created_at: &str, hidden: bool) {
        self.test
            .conn()
            .execute(
                "INSERT INTO sessions (id, workspace_id, created_at, is_hidden) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, WS, created_at, i64::from(hidden)],
            )
            .unwrap();
    }

    fn set_state(&self, state: &str) {
        self.test
            .conn()
            .execute(
                "UPDATE workspaces SET state = ?1 WHERE id = ?2",
                [state, WS],
            )
            .unwrap();
    }

    fn set_active(&self, chat: &str) {
        self.test
            .conn()
            .execute(
                "UPDATE workspaces SET active_session_id = ?1 WHERE id = ?2",
                [chat, WS],
            )
            .unwrap();
    }

    /// The chat got a user message: `last_user_message_at` is set.
    fn mark_sent(&self, chat: &str) {
        self.test
            .conn()
            .execute(
                "UPDATE sessions SET last_user_message_at = '2026-10-01 10:00:05' WHERE id = ?1",
                [chat],
            )
            .unwrap();
    }

    /// The worktree `<root>/relay/beta` with its `.git` entry.
    fn make_worktree(&self) -> PathBuf {
        let worktree = self.test.root().join("relay").join("beta");
        std::fs::create_dir_all(worktree.join(".git")).unwrap();
        worktree
    }

    fn state_dir(&self) -> PathBuf {
        self.test.dir().join("state")
    }

    fn staging(&self) -> PathBuf {
        staging_root(&self.state_dir())
    }

    fn lock(&self, locked: Option<bool>) {
        *self.lock.lock().unwrap() = locked;
    }

    fn backend(&self, outcome: SendOutcome) -> Arc<Backend> {
        Arc::new(Backend {
            reads: Reads::new(self.test.db(), self.test.root()),
            staging: self.staging(),
            inspects: AtomicUsize::new(0),
            outcome: Mutex::new(outcome),
            sends: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        })
    }

    fn queue(&self, backend: &Arc<Backend>) -> Arc<FirstPromptQueue> {
        let lock = Arc::clone(&self.lock);
        let probe: LockProbe = Arc::new(move || *lock.lock().unwrap());
        FirstPromptQueue::new(
            Arc::clone(&self.store),
            &self.state_dir(),
            probe,
            Arc::clone(backend) as Arc<dyn FirstPromptBackend>,
            Arc::clone(&self.clock),
        )
    }

    /// A started queue over a backend answering `outcome`.
    fn started(&self, outcome: SendOutcome) -> (Arc<FirstPromptQueue>, Arc<Backend>) {
        let backend = self.backend(outcome);
        let queue = self.queue(&backend);
        queue.start();
        (queue, backend)
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn enqueue(&self, queue: &FirstPromptQueue, text: &str, send_immediately: bool, ids: &[&str]) {
        queue
            .enqueue(
                WS,
                text,
                send_immediately,
                ids.iter().map(|id| (*id).to_owned()).collect(),
                self.now(),
            )
            .unwrap();
    }

    fn entry(&self) -> Option<FirstPromptRow> {
        self.store.first_prompt(WS).unwrap()
    }

    /// A staged file under the staging root; its six-character id.
    fn stage(&self, name: &str, bytes: &[u8]) -> String {
        write_attachment(&self.staging(), name, bytes, true)
            .unwrap()
            .id
    }
}

fn failed(error: &str) -> SendOutcome {
    SendOutcome::Failed(error.to_owned())
}

/// Sets the modification time of a staged directory `days` days back.
fn age(dir: &Path, days: u64) {
    let then = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
    std::fs::File::open(dir)
        .unwrap()
        .set_modified(then)
        .unwrap();
}

// ---- the reference's cases ----

#[tokio::test(start_paused = true)]
async fn sends_into_a_setting_up_workspace_and_settles_delivery() {
    let rig = Rig::with_workspace("setting_up");
    let (queue, backend) = rig.started(SendOutcome::Delivered);
    rig.enqueue(&queue, "hello", true, &[]);
    flush(0).await;

    assert_eq!(backend.sends(), vec!["hello"]);
    assert_eq!(rig.entry(), None);
    assert!(queue.pending_prompts().is_empty());
}

/// The reference's agent-settings cases, without agent settings (this relay has none): a prompt
/// with nothing to type settles without a send.
#[tokio::test(start_paused = true)]
async fn a_prompt_with_nothing_to_type_settles_without_a_send() {
    let rig = Rig::with_workspace("setting_up");
    let (queue, backend) = rig.started(failed(NO_COMPOSER));
    rig.enqueue(&queue, "  \n ", true, &[]);
    flush(0).await;

    assert!(backend.sends().is_empty());
    assert_eq!(rig.entry(), None);
}

/// The reference's persisted-entry case: an entry left in the store by a previous process is sent
/// by a queue that starts over it.
#[tokio::test(start_paused = true)]
async fn resumes_an_entry_a_previous_process_left_behind() {
    let rig = Rig::with_workspace("ready");
    rig.store
        .upsert_first_prompt(&NewFirstPrompt {
            workspace_id: WS.to_owned(),
            text: "left behind".to_owned(),
            send_immediately: true,
            attachment_ids: Vec::new(),
            created_at_ms: rig.now() - 60_000,
        })
        .unwrap();
    let (_queue, backend) = rig.started(SendOutcome::Delivered);
    flush(0).await;

    assert_eq!(backend.sends(), vec!["left behind"]);
    assert_eq!(rig.entry(), None);
}

#[tokio::test(start_paused = true)]
async fn uses_a_separate_budget_for_early_and_ready_failures() {
    let rig = Rig::with_workspace("setting_up");
    let (queue, backend) = rig.started(failed(NO_COMPOSER));
    rig.enqueue(&queue, "hello", true, &[]);
    flush(0).await;
    assert_eq!(backend.sends(), vec!["hello"]);
    let entry = rig.entry().unwrap();
    assert_eq!(
        (entry.attempts, entry.early_attempts, entry.status),
        (0, 1, ParkedStatus::Waiting)
    );

    flush(1_500).await;
    assert_eq!(backend.sends().len(), 1);
    rig.set_state("ready");
    // The ready spacing (5 s) runs from the early failure.
    flush(3_000).await;
    assert_eq!(backend.sends().len(), 1);
    flush(600).await;
    assert_eq!(backend.sends().len(), 2);
    let entry = rig.entry().unwrap();
    assert_eq!((entry.attempts, entry.early_attempts), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn does_not_resend_a_prompt_already_sent_from_the_mac() {
    let rig = Rig::with_workspace("setting_up");
    rig.mark_sent(CHAT);
    let (queue, backend) = rig.started(SendOutcome::Delivered);
    rig.enqueue(&queue, "hello", true, &[]);
    flush(0).await;

    assert!(backend.sends().is_empty());
    assert_eq!(rig.entry(), None);
}

#[tokio::test(start_paused = true)]
async fn hands_a_blocked_early_attempt_back() {
    let rig = Rig::with_workspace("setting_up");
    let (queue, backend) = rig.started(SendOutcome::Locked);
    rig.enqueue(&queue, "hello", true, &[]);
    flush(0).await;

    assert_eq!(backend.sends(), vec!["hello"]);
    let entry = rig.entry().unwrap();
    assert_eq!(
        (
            entry.attempts,
            entry.early_attempts,
            entry.last_attempt_at_ms
        ),
        (0, 0, None)
    );
}

#[tokio::test(start_paused = true)]
async fn honors_the_option_to_wait_until_setup_finishes() {
    let rig = Rig::with_workspace("setting_up");
    let (queue, backend) = rig.started(failed(NO_COMPOSER));
    rig.enqueue(&queue, "hello", false, &[]);
    flush(1_500).await;
    assert!(backend.sends().is_empty());
    assert_eq!(rig.entry().unwrap().early_attempts, 0);

    rig.set_state("ready");
    flush(1_000).await;
    assert_eq!(backend.sends(), vec!["hello"]);
}

#[tokio::test(start_paused = true)]
async fn materializes_staged_files_before_sending_their_token() {
    let rig = Rig::with_workspace("setting_up");
    let id = rig.stage("notes.txt", b"staged bytes");
    let (queue, backend) = rig.started(SendOutcome::Delivered);
    rig.enqueue(&queue, "hello", true, &[&id]);
    flush(100).await;
    // No worktree yet: nothing is copied and nothing is sent.
    assert!(backend.events().is_empty());

    let worktree = rig.make_worktree();
    flush(900).await;
    assert_eq!(
        backend.events(),
        vec![format!("materialize:{id}"), "send:hello".to_owned()]
    );
    let copied = worktree.join(ATTACHMENTS_DIR).join(&id).join("notes.txt");
    assert_eq!(std::fs::read(copied).unwrap(), b"staged bytes");
    assert!(!rig.staging().join(ATTACHMENTS_DIR).join(&id).exists());
    assert_eq!(rig.entry(), None);
}

#[tokio::test(start_paused = true)]
async fn charges_a_ready_failure_to_the_counted_budget() {
    let rig = Rig::with_workspace("ready");
    let (queue, backend) = rig.started(failed(NO_COMPOSER));
    rig.enqueue(&queue, "hello", true, &[]);
    flush(0).await;

    assert_eq!(backend.sends(), vec!["hello"]);
    let entry = rig.entry().unwrap();
    assert_eq!((entry.attempts, entry.early_attempts), (1, 0));
}

// ---- the rest of the step ----

#[tokio::test(start_paused = true)]
async fn fails_on_the_last_counted_attempt_with_its_error() {
    let rig = Rig::with_workspace("ready");
    let (queue, backend) = rig.started(failed(NO_COMPOSER));
    rig.enqueue(&queue, "hello", true, &[]);
    flush(0).await;
    flush(4_500).await;
    assert_eq!(backend.sends().len(), 1);
    flush(1_000).await;
    assert_eq!(backend.sends().len(), 2);
    backend.answer(failed("the composer is disabled"));
    flush(5_000).await;
    assert_eq!(backend.sends().len(), MAX_ATTEMPTS as usize);

    let entry = rig.entry().unwrap();
    assert_eq!(entry.status, ParkedStatus::Failed);
    assert_eq!(entry.attempts, MAX_ATTEMPTS);
    assert_eq!(entry.error.as_deref(), Some("the composer is disabled"));
    // Failed entries stay listed and are not tried again.
    flush(60_000).await;
    assert_eq!(backend.sends().len(), MAX_ATTEMPTS as usize);
    assert_eq!(queue.pending_prompts()[0]["status"], "failed");
}

#[tokio::test(start_paused = true)]
async fn early_sends_stop_at_two_twenty_seconds_apart_and_never_fail() {
    let rig = Rig::with_workspace("setting_up");
    let (queue, backend) = rig.started(failed(NO_COMPOSER));
    rig.enqueue(&queue, "hello", true, &[]);
    flush(0).await;
    flush(19_000).await;
    assert_eq!(backend.sends().len(), 1);
    flush(1_500).await;
    assert_eq!(backend.sends().len(), 2);
    flush(120_000).await;
    assert_eq!(backend.sends().len(), MAX_EARLY_ATTEMPTS as usize);

    let entry = rig.entry().unwrap();
    assert_eq!(
        (entry.status, entry.attempts, entry.early_attempts),
        (ParkedStatus::Waiting, 0, MAX_EARLY_ATTEMPTS)
    );
    // Ready later: the counted budget is whole.
    rig.set_state("ready");
    flush(1_000).await;
    assert_eq!(backend.sends().len(), 3);
    assert_eq!(rig.entry().unwrap().attempts, 1);
}

#[tokio::test(start_paused = true)]
async fn waits_for_the_workspace_row_and_its_chat_then_sends() {
    let rig = Rig::new();
    let (queue, backend) = rig.started(SendOutcome::Delivered);
    rig.enqueue(&queue, "hello", true, &[]);
    flush(3_000).await;
    assert!(backend.sends().is_empty());

    rig.add_workspace("setting_up");
    flush(3_000).await;
    assert!(backend.sends().is_empty());

    rig.add_chat(CHAT, "2026-10-01 10:00:00", false);
    flush(1_000).await;
    assert_eq!(backend.sends(), vec!["hello"]);
    assert_eq!(rig.entry(), None);
}

#[tokio::test(start_paused = true)]
async fn fails_a_workspace_that_never_finished_setting_up() {
    let rig = Rig::with_workspace("setting_up");
    let (queue, backend) = rig.started(SendOutcome::Delivered);
    rig.enqueue(&queue, "hello", false, &[]);
    flush(14 * 60_000).await;
    assert_eq!(rig.entry().unwrap().status, ParkedStatus::Waiting);

    flush(61_000 + 1_000).await;
    let entry = rig.entry().unwrap();
    assert_eq!(entry.status, ParkedStatus::Failed);
    assert_eq!(entry.error.as_deref(), Some(NEVER_SET_UP));
    assert!(backend.sends().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_locked_mac_freezes_every_entry_and_reads_nothing() {
    let rig = Rig::with_workspace("ready");
    rig.lock(Some(true));
    let (queue, backend) = rig.started(failed(NO_COMPOSER));
    rig.enqueue(&queue, "hello", true, &[]);
    flush(20 * 60_000).await;

    assert!(backend.sends().is_empty());
    assert_eq!(backend.inspects(), 0);
    let entry = rig.entry().unwrap();
    assert_eq!((entry.status, entry.attempts), (ParkedStatus::Waiting, 0));

    rig.lock(Some(false));
    backend.answer(SendOutcome::Delivered);
    flush(1_000).await;
    assert_eq!(backend.sends(), vec!["hello"]);
    assert_eq!(rig.entry(), None);
}

#[tokio::test(start_paused = true)]
async fn with_no_entries_the_pump_reads_nothing() {
    let rig = Rig::with_workspace("ready");
    let (queue, backend) = rig.started(SendOutcome::Delivered);
    flush(2 * 3_600_000 + 1_000).await;
    assert_eq!(backend.inspects(), 0);

    rig.enqueue(&queue, "hello", true, &[]);
    flush(0).await;
    assert!(backend.inspects() > 0);
    assert_eq!(backend.sends(), vec!["hello"]);

    // Delivered: the queue is empty again, and the reads stop.
    let after = backend.inspects();
    flush(3_600_000).await;
    assert_eq!(backend.inspects(), after);
}

#[tokio::test(start_paused = true)]
async fn a_new_queue_over_the_same_store_resumes_without_resending() {
    let rig = Rig::with_workspace("setting_up");
    let (first, before) = rig.started(failed(NO_COMPOSER));
    rig.enqueue(&first, "hello", true, &[]);
    flush(0).await;
    assert_eq!(before.sends(), vec!["hello"]);
    assert_eq!(rig.entry().unwrap().early_attempts, 1);

    // The relay stops; the send had landed after all.
    before.closed.store(true, Ordering::SeqCst);
    flush(2_000).await;
    rig.mark_sent(CHAT);
    rig.set_state("ready");

    let (_second, after) = rig.started(failed(NO_COMPOSER));
    flush(0).await;
    assert!(after.sends().is_empty());
    assert_eq!(rig.entry(), None);
    assert_eq!(before.sends().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn prunes_old_entries_and_unreferenced_staged_files_at_start_and_hourly() {
    let rig = Rig::with_workspace("ready");
    let referenced = rig.stage("entry.txt", b"a");
    let drafted = rig.stage("draft.txt", b"b");
    let orphan = rig.stage("orphan.txt", b"c");
    let fresh = rig.stage("fresh.txt", b"d");
    let of_old_entry = rig.stage("old.txt", b"e");
    let staged = |id: &str| rig.staging().join(ATTACHMENTS_DIR).join(id);
    for id in [&referenced, &drafted, &orphan, &of_old_entry] {
        age(&staged(id), 8);
    }
    rig.store
        .set_meta(
            "prefs",
            &json!({
                "readMarks": {},
                "drafts": {
                    "new-workspace": {
                        "text": "draft",
                        "attachments": [{ "stageId": drafted, "name": "draft.txt" }]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    let entry = |workspace_id: &str, ids: Vec<String>, created_at_ms: i64| NewFirstPrompt {
        workspace_id: workspace_id.to_owned(),
        text: "hello".to_owned(),
        send_immediately: false,
        attachment_ids: ids,
        created_at_ms,
    };
    rig.store
        .upsert_first_prompt(&entry(
            "ws-old",
            vec![of_old_entry.clone()],
            rig.now() - 8 * DAY_MS,
        ))
        .unwrap();
    rig.store
        .upsert_first_prompt(&entry("ws-gone", vec![referenced.clone()], rig.now()))
        .unwrap();
    rig.store.fail_first_prompt("ws-gone", "kept").unwrap();

    let (_queue, _backend) = rig.started(SendOutcome::Delivered);
    let workspaces: Vec<String> = rig
        .store
        .first_prompts()
        .unwrap()
        .into_iter()
        .map(|row| row.workspace_id)
        .collect();
    assert_eq!(workspaces, vec!["ws-gone"]);
    assert!(staged(&referenced).exists());
    assert!(staged(&drafted).exists());
    assert!(staged(&fresh).exists());
    assert!(!staged(&orphan).exists());
    assert!(!staged(&of_old_entry).exists());

    // An hour later, again.
    let late = rig.stage("late.txt", b"f");
    age(&staged(&late), 8);
    rig.store
        .upsert_first_prompt(&entry(
            "ws-older",
            Vec::new(),
            rig.now() - KEEP_FAILED.as_millis() as i64 + 30 * 60_000,
        ))
        .unwrap();
    rig.store.fail_first_prompt("ws-older", "kept").unwrap();
    flush(PRUNE_EVERY.as_millis() as u64 + 1_000).await;
    assert!(!staged(&late).exists());
    assert!(staged(&referenced).exists());
    assert!(rig.store.first_prompt("ws-older").unwrap().is_none());
    assert!(rig.store.first_prompt("ws-gone").unwrap().is_some());
}

// ---- what Conductor's database says ----

#[test]
fn the_target_is_the_live_workspace_its_active_or_first_open_chat_and_worktree() {
    let rig = Rig::new();
    let reads = Reads::new(rig.test.db(), rig.test.root());
    assert_eq!(inspect_target(&reads, WS).unwrap(), None);

    rig.add_workspace("setting_up");
    assert_eq!(
        inspect_target(&reads, WS).unwrap(),
        Some(FirstPromptTarget {
            phase: Phase::SettingUp,
            session_id: None,
            already_sent: false,
            worktree: None,
        })
    );

    rig.add_chat("hidden", "2026-10-01 09:00:00", true);
    rig.add_chat("first", "2026-10-01 10:00:00", false);
    rig.add_chat("second", "2026-10-01 11:00:00", false);
    let target = inspect_target(&reads, WS).unwrap().unwrap();
    assert_eq!(target.session_id.as_deref(), Some("first"));

    rig.set_active("second");
    rig.mark_sent("second");
    rig.set_state("ready");
    let worktree = rig.make_worktree();
    assert_eq!(
        inspect_target(&reads, WS).unwrap(),
        Some(FirstPromptTarget {
            phase: Phase::Ready,
            session_id: Some("second".to_owned()),
            already_sent: true,
            worktree: Some(worktree),
        })
    );

    // An active chat that is hidden is not the target.
    rig.set_active("hidden");
    let target = inspect_target(&reads, WS).unwrap().unwrap();
    assert_eq!(
        (target.session_id.as_deref(), target.already_sent),
        (Some("first"), false)
    );

    rig.set_state("archived");
    assert_eq!(inspect_target(&reads, WS).unwrap(), None);
}

// ---- through the writes ----

fn writes(rig: &Rig) -> Writes {
    let ui = UiActor::spawn(|| {
        let app = conductor_app(&WindowSpec {
            repo: "relay".to_owned(),
            branch: "user/x".to_owned(),
            sidebar: vec!["beta".to_owned()],
            chats: vec!["One".to_owned()],
            selected: 0,
            composer_value: None,
        });
        Box::new(Driver::new(FakeDesktop::new(app)))
    });
    let reads = Arc::new(Reads::new(rig.test.db(), rig.test.root()));
    let parked = ParkedQueue::new(
        Arc::clone(&rig.store),
        Arc::new(|| Some(false)),
        ParkedTimings::default(),
    );
    let timings = WriteTimings {
        delivery: DeliveryTimings {
            confirm_window: ms(60),
            poll: ms(10),
            min_attempt: ms(50),
            min_confirm: ms(10),
            retry_pause: ms(10),
        },
        stop_poll: ms(10),
        stop_checks: 3,
        chat_poll: ms(10),
        chat_checks: 3,
        send_budget: Some(ms(400)),
        restore_poll: ms(10),
        restore_checks: 3,
        create_poll: ms(10),
        create_checks: 3,
    };
    Writes::new(reads, ui, Arc::new(|| true), timings, parked).configure(WriteDeps {
        state_dir: rig.state_dir(),
        store: Arc::clone(&rig.store),
        commands: Arc::new(FakeCommands::new()),
        locked: Arc::new(|| Some(false)),
    })
}

#[tokio::test]
async fn the_writes_list_pending_prompts_and_dismiss_them_with_their_staged_files() {
    let rig = Rig::with_workspace("ready");
    let id = rig.stage("notes.txt", b"x");
    let writes = writes(&rig);
    assert!(writes.pending_prompts().is_empty());

    let row = rig
        .store
        .upsert_first_prompt(&NewFirstPrompt {
            workspace_id: WS.to_owned(),
            text: "hello".to_owned(),
            send_immediately: false,
            attachment_ids: vec![id.clone()],
            created_at_ms: 1_000,
        })
        .unwrap();
    let waiting = json!({
        "workspaceId": WS,
        "text": "hello",
        "status": "waiting",
        "attempts": 0,
        "earlyAttempts": 0,
        "sendImmediately": false,
        "attachmentIds": [id],
        "createdAt": 1_000,
    });
    assert_eq!(first_prompt_json(&row), waiting);
    assert_eq!(writes.pending_prompts(), vec![waiting]);

    rig.store
        .record_first_prompt_attempt(WS, false, 2_000)
        .unwrap();
    rig.store.fail_first_prompt(WS, NO_COMPOSER).unwrap();
    assert_eq!(
        writes.pending_prompts(),
        vec![json!({
            "workspaceId": WS,
            "text": "hello",
            "status": "failed",
            "attempts": 1,
            "earlyAttempts": 0,
            "sendImmediately": false,
            "attachmentIds": [id],
            "createdAt": 1_000,
            "lastAttemptAt": 2_000,
            "error": NO_COMPOSER,
        })]
    );

    let answer = writes.dismiss_first_prompt(WS.to_owned()).await;
    assert_eq!((answer.status, answer.body), (200, json!({ "ok": true })));
    assert!(writes.pending_prompts().is_empty());
    assert!(!rig.staging().join(ATTACHMENTS_DIR).join(&id).exists());

    let answer = writes.dismiss_first_prompt(WS.to_owned()).await;
    assert_eq!(
        (answer.status, answer.body),
        (404, json!({ "error": "no pending prompt" }))
    );
}
