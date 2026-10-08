//! The parked-prompt queue over an in-memory store, with fake deliverers and a fake lock probe, in
//! paused time.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::delivery::parked::{
    cursor_of, parked_json, Deliverer, LockProbe, Notice, ParkedOutcome, ParkedQueue,
    ParkedTimings, KEEP_FAILED, MAX_ATTEMPTS, PARKED_REASON,
};
use conductor_remote::delivery::BoxFuture;
use conductor_remote::reads::receipts::DeliveryCursor;
use conductor_remote::state::store::{ParkedRow, ParkedStatus, Store};
use serde_json::json;
use tokio::time::sleep;

const NOW: i64 = 1_700_000_000_000;
const DAY_MS: i64 = 24 * 3600 * 1000;

fn store() -> Arc<Store> {
    Arc::new(Store::open_in_memory().expect("in-memory store"))
}

fn cursor(rowid: i64, outbox: &[&str]) -> DeliveryCursor {
    DeliveryCursor {
        rowid,
        outbox_ids: outbox.iter().map(|id| (*id).to_owned()).collect(),
    }
}

/// A lock probe the test can flip.
struct Lock(Mutex<Option<bool>>);

impl Lock {
    fn new(state: Option<bool>) -> Arc<Lock> {
        Arc::new(Lock(Mutex::new(state)))
    }

    fn set(&self, state: Option<bool>) {
        *self.0.lock().unwrap() = state;
    }

    fn probe(self: &Arc<Self>) -> LockProbe {
        let lock = Arc::clone(self);
        Arc::new(move || *lock.0.lock().unwrap())
    }
}

/// A deliverer that records each row it was handed and answers the current outcome.
struct Fake {
    answer: Mutex<ParkedOutcome>,
    calls: Mutex<Vec<ParkedRow>>,
}

impl Fake {
    fn new(answer: ParkedOutcome) -> Arc<Fake> {
        Arc::new(Fake {
            answer: Mutex::new(answer),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn answer(&self, outcome: ParkedOutcome) {
        *self.answer.lock().unwrap() = outcome;
    }

    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn texts(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|row| row.text.clone())
            .collect()
    }

    fn deliverer(self: &Arc<Self>) -> Deliverer {
        let fake = Arc::clone(self);
        Arc::new(move |row: ParkedRow| -> BoxFuture<ParkedOutcome> {
            let fake = Arc::clone(&fake);
            Box::pin(async move {
                fake.calls.lock().unwrap().push(row);
                fake.answer.lock().unwrap().clone()
            })
        })
    }
}

type Notices = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A notice that records the text and the error of each call.
fn recorder() -> (Notice, Notices) {
    let seen: Notices = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let notice: Notice = Arc::new(move |row: &ParkedRow, error: Option<&str>| {
        sink.lock()
            .unwrap()
            .push((row.text.clone(), error.map(str::to_owned)));
    });
    (notice, seen)
}

fn notices(seen: &Notices) -> Vec<(String, Option<String>)> {
    seen.lock().unwrap().clone()
}

fn texts(queue: &ParkedQueue) -> Vec<String> {
    queue.list().into_iter().map(|row| row.text).collect()
}

/// Lets the pump run what is due now.
async fn settle() {
    sleep(Duration::from_millis(1)).await;
}

#[tokio::test(start_paused = true)]
async fn park_then_list() {
    let queue = ParkedQueue::new(store(), Lock::new(None).probe(), ParkedTimings::default());

    let row = queue
        .park("w1", "s1", "  hello  ", true, &cursor(7, &["o1"]), NOW)
        .unwrap();

    assert_eq!(row.text, "hello");
    assert_eq!(row.workspace_id, "w1");
    assert_eq!(row.session_id, "s1");
    assert!(row.queue);
    assert_eq!(row.status, ParkedStatus::Waiting);
    assert_eq!(row.attempts, 0);
    assert_eq!(row.created_at_ms, NOW);
    assert_eq!(row.reason, PARKED_REASON);
    assert_eq!(row.error, None);
    assert_eq!(queue.list(), vec![row]);
}

#[tokio::test(start_paused = true)]
async fn re_park_resets_the_row() {
    let store = store();
    let queue = ParkedQueue::new(
        Arc::clone(&store),
        Lock::new(None).probe(),
        ParkedTimings::default(),
    );
    let first = queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    for _ in 0..MAX_ATTEMPTS {
        store
            .record_parked_failure(first.id, "boom", MAX_ATTEMPTS)
            .unwrap();
    }
    assert_eq!(queue.list()[0].status, ParkedStatus::Failed);

    let again = queue
        .park("w2", "s1", " hello ", true, &cursor(9, &["o9"]), NOW + 1000)
        .unwrap();

    assert_eq!(again.id, first.id);
    assert_eq!(again.workspace_id, "w2");
    assert!(again.queue);
    assert_eq!(again.status, ParkedStatus::Waiting);
    assert_eq!(again.attempts, 0);
    assert_eq!(again.error, None);
    assert_eq!(again.created_at_ms, NOW);
    assert_eq!(cursor_of(&again), Some(cursor(9, &["o9"])));
    assert_eq!(queue.list(), vec![again]);
}

#[tokio::test(start_paused = true)]
async fn parked_json_has_the_phone_shape() {
    let store = store();
    let queue = ParkedQueue::new(
        Arc::clone(&store),
        Lock::new(None).probe(),
        ParkedTimings::default(),
    );
    let plain = queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    let queued = queue
        .park("w1", "s2", "later", true, &cursor(1, &[]), NOW)
        .unwrap();

    assert_eq!(
        parked_json(&plain),
        json!({
            "workspaceId": "w1",
            "sessionId": "s1",
            "text": "hello",
            "status": "waiting",
            "attempts": 0,
            "createdAt": NOW,
            "reason": PARKED_REASON,
        })
    );
    assert_eq!(
        parked_json(&queued),
        json!({
            "workspaceId": "w1",
            "sessionId": "s2",
            "text": "later",
            "queue": true,
            "status": "waiting",
            "attempts": 0,
            "createdAt": NOW,
            "reason": PARKED_REASON,
        })
    );

    let mut failed = None;
    for _ in 0..MAX_ATTEMPTS {
        failed = store
            .record_parked_failure(plain.id, "boom", MAX_ATTEMPTS)
            .unwrap();
    }
    assert_eq!(
        parked_json(&failed.unwrap()),
        json!({
            "workspaceId": "w1",
            "sessionId": "s1",
            "text": "hello",
            "status": "failed",
            "attempts": 3,
            "createdAt": NOW,
            "reason": PARKED_REASON,
            "error": "boom",
        })
    );
}

#[tokio::test(start_paused = true)]
async fn locked_holds_delivery_until_unlocked() {
    let lock = Lock::new(Some(true));
    let timings = ParkedTimings::default();
    let queue = ParkedQueue::new(store(), lock.probe(), timings);
    let fake = Fake::new(ParkedOutcome::Delivered);
    let (notice, seen) = recorder();
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    queue.start(fake.deliverer(), notice, NOW);

    sleep(Duration::from_secs(600)).await;
    assert_eq!(fake.calls(), 0);
    assert_eq!(texts(&queue), vec!["hello"]);
    assert!(notices(&seen).is_empty());

    lock.set(Some(false));
    sleep(timings.poll).await;
    settle().await;

    assert_eq!(fake.texts(), vec!["hello"]);
    assert!(queue.list().is_empty());
    assert_eq!(notices(&seen), vec![("hello".to_owned(), None)]);
}

#[tokio::test(start_paused = true)]
async fn delivered_is_removed_and_noticed_once() {
    let queue = ParkedQueue::new(
        store(),
        Lock::new(Some(false)).probe(),
        ParkedTimings::default(),
    );
    let fake = Fake::new(ParkedOutcome::Delivered);
    let (notice, seen) = recorder();
    queue.start(fake.deliverer(), notice, NOW);
    settle().await;
    assert_eq!(fake.calls(), 0);

    queue
        .park("w1", "s1", "hello", true, &cursor(1, &[]), NOW)
        .unwrap();
    settle().await;

    assert_eq!(fake.texts(), vec!["hello"]);
    assert!(fake.calls.lock().unwrap()[0].queue);
    assert!(queue.list().is_empty());
    assert_eq!(notices(&seen), vec![("hello".to_owned(), None)]);

    sleep(Duration::from_secs(600)).await;
    assert_eq!(fake.calls(), 1);
    assert_eq!(notices(&seen).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn locked_answer_is_not_counted() {
    let timings = ParkedTimings::default();
    let queue = ParkedQueue::new(store(), Lock::new(None).probe(), timings);
    let fake = Fake::new(ParkedOutcome::Locked);
    let (notice, seen) = recorder();
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    queue.start(fake.deliverer(), notice, NOW);

    sleep(timings.poll * 6).await;
    assert!(fake.calls() >= 2);
    let rows = queue.list();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, ParkedStatus::Waiting);
    assert_eq!(rows[0].attempts, 0);
    assert_eq!(rows[0].error, None);
    assert!(notices(&seen).is_empty());

    fake.answer(ParkedOutcome::Delivered);
    sleep(timings.poll).await;
    settle().await;
    assert!(queue.list().is_empty());
    assert_eq!(notices(&seen), vec![("hello".to_owned(), None)]);
}

#[tokio::test(start_paused = true)]
async fn unknown_lock_and_locked_answers_run_once_per_poll() {
    let timings = ParkedTimings::default();
    let queue = ParkedQueue::new(store(), Lock::new(None).probe(), timings);
    let fake = Fake::new(ParkedOutcome::Locked);
    let (notice, _seen) = recorder();
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    queue.start(fake.deliverer(), notice, NOW);
    settle().await;
    let before = fake.calls();
    assert!(before >= 1);

    sleep(timings.poll * 10).await;
    let during = fake.calls() - before;

    assert!(
        (9..=11).contains(&during),
        "{during} deliveries in ten polls"
    );
}

#[tokio::test(start_paused = true)]
async fn three_failures_mark_the_row_failed_and_notice_once() {
    let queue = ParkedQueue::new(
        store(),
        Lock::new(Some(false)).probe(),
        ParkedTimings::default(),
    );
    let fake = Fake::new(ParkedOutcome::Failed("boom".to_owned()));
    let (notice, seen) = recorder();
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    queue.start(fake.deliverer(), notice, NOW);

    sleep(Duration::from_secs(600)).await;

    assert_eq!(fake.calls(), MAX_ATTEMPTS as usize);
    assert_eq!(
        notices(&seen),
        vec![("hello".to_owned(), Some("boom".to_owned()))]
    );
    let rows = queue.list();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, ParkedStatus::Failed);
    assert_eq!(rows[0].attempts, MAX_ATTEMPTS);
    assert_eq!(rows[0].error.as_deref(), Some("boom"));
}

#[tokio::test(start_paused = true)]
async fn a_failure_waits_retry_before_the_next_try() {
    let timings = ParkedTimings {
        poll: Duration::from_secs(1),
        retry: Duration::from_secs(10),
    };
    let queue = ParkedQueue::new(store(), Lock::new(Some(false)).probe(), timings);
    let fake = Fake::new(ParkedOutcome::Failed("boom".to_owned()));
    let (notice, _seen) = recorder();
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    queue.start(fake.deliverer(), notice, NOW);
    settle().await;
    assert_eq!(fake.calls(), 1);
    let row = &queue.list()[0];
    assert_eq!(row.attempts, 1);
    assert_eq!(row.status, ParkedStatus::Waiting);
    assert_eq!(row.error, None);

    // A park in the meantime does not cut the retry wait short.
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    sleep(timings.retry - Duration::from_millis(10)).await;
    assert_eq!(fake.calls(), 1);

    sleep(timings.poll + Duration::from_millis(20)).await;
    assert_eq!(fake.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn delivery_is_fifo_across_chats() {
    let lock = Lock::new(Some(true));
    let timings = ParkedTimings::default();
    let queue = ParkedQueue::new(store(), lock.probe(), timings);
    let fake = Fake::new(ParkedOutcome::Delivered);
    let (notice, seen) = recorder();
    queue.start(fake.deliverer(), notice, NOW);
    for (session, text) in [
        ("s1", "one"),
        ("s2", "two"),
        ("s1", "three"),
        ("s3", "four"),
    ] {
        queue
            .park("w1", session, text, false, &cursor(1, &[]), NOW)
            .unwrap();
        settle().await;
    }
    assert_eq!(fake.calls(), 0);

    lock.set(Some(false));
    sleep(timings.poll).await;
    settle().await;

    assert_eq!(fake.texts(), vec!["one", "two", "three", "four"]);
    let noticed: Vec<String> = notices(&seen).into_iter().map(|(text, _)| text).collect();
    assert_eq!(noticed, vec!["one", "two", "three", "four"]);
    assert!(queue.list().is_empty());
}

#[tokio::test(start_paused = true)]
async fn forget_session_and_forget_delivered() {
    let queue = ParkedQueue::new(store(), Lock::new(None).probe(), ParkedTimings::default());
    for (session, text) in [
        ("s1", "one"),
        ("s1", "two"),
        ("s2", "three"),
        ("s2", "four"),
    ] {
        queue
            .park("w1", session, text, false, &cursor(1, &[]), NOW)
            .unwrap();
    }

    assert_eq!(queue.forget_session("s1"), 2);
    assert_eq!(texts(&queue), vec!["three", "four"]);
    assert_eq!(queue.forget_session("s1"), 0);

    assert_eq!(queue.forget_delivered("s2", "  three "), 1);
    assert_eq!(texts(&queue), vec!["four"]);
    assert_eq!(queue.forget_delivered("s2", "three"), 0);
    assert_eq!(queue.forget_delivered("s1", "four"), 0);
    assert_eq!(texts(&queue), vec!["four"]);
}

#[tokio::test(start_paused = true)]
async fn a_row_dismissed_during_delivery_stays_gone() {
    let queue = ParkedQueue::new(
        store(),
        Lock::new(Some(false)).probe(),
        ParkedTimings::default(),
    );
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&runs);
    let slow_failure: Deliverer = Arc::new(move |_row: ParkedRow| -> BoxFuture<ParkedOutcome> {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            sleep(Duration::from_secs(1)).await;
            ParkedOutcome::Failed("boom".to_owned())
        })
    });
    let (notice, seen) = recorder();
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    queue.start(slow_failure, notice, NOW);

    sleep(Duration::from_millis(500)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(queue.forget_session("s1"), 1);

    sleep(Duration::from_secs(600)).await;
    assert!(queue.list().is_empty());
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert!(notices(&seen).is_empty());
}

#[tokio::test(start_paused = true)]
async fn start_twice_runs_one_pump() {
    let store = store();
    let queue = ParkedQueue::new(
        Arc::clone(&store),
        Lock::new(Some(false)).probe(),
        ParkedTimings::default(),
    );
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&runs);
    let slow: Deliverer = Arc::new(move |_row: ParkedRow| -> BoxFuture<ParkedOutcome> {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            sleep(Duration::from_secs(1)).await;
            ParkedOutcome::Delivered
        })
    });
    let second = Fake::new(ParkedOutcome::Delivered);
    let (notice, seen) = recorder();
    let (second_notice, second_seen) = recorder();
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();

    queue.start(slow, notice, NOW);
    // A second start neither spawns a pump nor prunes, even with a clock far ahead.
    queue.start(second.deliverer(), second_notice, NOW + 30 * DAY_MS);
    assert_eq!(texts(&queue), vec!["hello"]);

    sleep(Duration::from_secs(600)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls(), 0);
    assert_eq!(notices(&seen), vec![("hello".to_owned(), None)]);
    assert!(notices(&second_seen).is_empty());
}

#[tokio::test(start_paused = true)]
async fn start_prunes_rows_older_than_seven_days() {
    let keep_ms = KEEP_FAILED.as_millis() as i64;
    assert_eq!(keep_ms, 7 * DAY_MS);
    let queue = ParkedQueue::new(
        store(),
        Lock::new(Some(true)).probe(),
        ParkedTimings::default(),
    );
    let c = cursor(1, &[]);
    queue
        .park("w1", "s1", "ancient", false, &c, NOW - 8 * DAY_MS)
        .unwrap();
    queue
        .park("w1", "s1", "just over", false, &c, NOW - keep_ms - 1)
        .unwrap();
    queue
        .park("w1", "s2", "exactly", false, &c, NOW - keep_ms)
        .unwrap();
    queue
        .park("w1", "s2", "fresh", false, &c, NOW - DAY_MS)
        .unwrap();
    let fake = Fake::new(ParkedOutcome::Delivered);
    let (notice, _seen) = recorder();

    queue.start(fake.deliverer(), notice, NOW);

    assert_eq!(texts(&queue), vec!["exactly", "fresh"]);
}

/// The wall clock, which paused time does not move.
fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test(start_paused = true)]
async fn the_pump_prunes_old_failed_rows_while_it_runs() {
    let queue = ParkedQueue::new(
        store(),
        Lock::new(Some(false)).probe(),
        ParkedTimings::default(),
    );
    let fake = Fake::new(ParkedOutcome::Failed("boom".to_owned()));
    let (notice, _seen) = recorder();
    let c = cursor(1, &[]);
    queue.start(fake.deliverer(), notice, wall_ms());

    // Inserted after start: only the running pump can drop the old one.
    queue
        .park("w1", "s1", "old", false, &c, wall_ms() - 8 * DAY_MS)
        .unwrap();
    queue
        .park("w1", "s2", "fresh", false, &c, wall_ms() - DAY_MS)
        .unwrap();
    sleep(Duration::from_secs(60)).await;
    let rows = queue.list();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.status == ParkedStatus::Failed));

    sleep(Duration::from_secs(3600)).await;

    assert_eq!(texts(&queue), vec!["fresh"]);
    assert_eq!(queue.list()[0].status, ParkedStatus::Failed);
}

#[tokio::test(start_paused = true)]
async fn cursor_of_returns_the_parked_cursor() {
    let queue = ParkedQueue::new(store(), Lock::new(None).probe(), ParkedTimings::default());
    let parked = cursor(42, &["o2", "o1"]);

    let row = queue
        .park("w1", "s1", "hello", false, &parked, NOW)
        .unwrap();

    assert_eq!(cursor_of(&row), Some(parked.clone()));
    assert_eq!(cursor_of(&queue.list()[0]), Some(parked));
    let without = ParkedRow {
        cursor_rowid: None,
        cursor_outbox: Vec::new(),
        ..row
    };
    assert_eq!(cursor_of(&without), None);
}

#[tokio::test(start_paused = true)]
async fn a_restart_delivers_with_the_persisted_cursor() {
    let store = store();
    let parked = cursor(42, &["o1", "o2"]);
    {
        let before = ParkedQueue::new(
            Arc::clone(&store),
            Lock::new(Some(true)).probe(),
            ParkedTimings::default(),
        );
        before
            .park("w1", "s1", "hello", true, &parked, NOW)
            .unwrap();
    }

    let after = ParkedQueue::new(
        Arc::clone(&store),
        Lock::new(Some(false)).probe(),
        ParkedTimings::default(),
    );
    let fake = Fake::new(ParkedOutcome::Delivered);
    let (notice, seen) = recorder();
    after.start(fake.deliverer(), notice, NOW + 1000);
    settle().await;

    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].text, "hello");
    assert!(calls[0].queue);
    assert_eq!(cursor_of(&calls[0]), Some(parked));
    assert_eq!(
        cursor_of(&calls[0]).unwrap().outbox_ids,
        BTreeSet::from(["o1".to_owned(), "o2".to_owned()])
    );
    assert!(after.list().is_empty());
    assert_eq!(notices(&seen), vec![("hello".to_owned(), None)]);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_deliverer_counts_one_failure_and_the_pump_goes_on() {
    let timings = ParkedTimings::default();
    let queue = ParkedQueue::new(store(), Lock::new(Some(false)).probe(), timings);
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&runs);
    let flaky: Deliverer = Arc::new(move |_row: ParkedRow| -> BoxFuture<ParkedOutcome> {
        let run = counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if run == 0 {
                panic!("the deliverer broke on purpose");
            }
            ParkedOutcome::Delivered
        })
    });
    let (notice, seen) = recorder();
    queue
        .park("w1", "s1", "hello", false, &cursor(1, &[]), NOW)
        .unwrap();
    queue.start(flaky, notice, NOW);
    settle().await;

    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let row = &queue.list()[0];
    assert_eq!(row.attempts, 1);
    assert_eq!(row.status, ParkedStatus::Waiting);
    assert!(notices(&seen).is_empty());

    sleep(timings.retry + timings.poll).await;
    settle().await;

    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert!(queue.list().is_empty());
    assert_eq!(notices(&seen), vec![("hello".to_owned(), None)]);
}
