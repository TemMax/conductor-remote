//! The duplicate memo: one outcome per client id, in paused time.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conductor_remote::delivery::sendonce::{SendOnce, SENDONCE_TTL};
use tokio::sync::oneshot;

fn keep_all(_: &String) -> bool {
    true
}

/// A work that counts its runs and answers `outcome`.
fn counted(runs: &Arc<AtomicUsize>, outcome: &str) -> impl std::future::Future<Output = String> {
    let runs = Arc::clone(runs);
    let outcome = outcome.to_owned();
    async move {
        runs.fetch_add(1, Ordering::SeqCst);
        outcome
    }
}

async fn broken() -> String {
    panic!("the work broke on purpose");
}

#[tokio::test(start_paused = true)]
async fn repeated_keys_run_once() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));

    let first = memo.run(Some("a"), keep_all, counted(&runs, "sent")).await;
    let second = memo.run(Some("a"), keep_all, counted(&runs, "again")).await;
    let third = memo.run(Some("a"), keep_all, counted(&runs, "again")).await;

    assert_eq!(first.as_deref(), Some("sent"));
    assert_eq!(second.as_deref(), Some("sent"));
    assert_eq!(third.as_deref(), Some("sent"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn different_keys_run_separately() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));

    let a = memo.run(Some("a"), keep_all, counted(&runs, "for a")).await;
    let b = memo.run(Some("b"), keep_all, counted(&runs, "for b")).await;

    assert_eq!(a.as_deref(), Some("for a"));
    assert_eq!(b.as_deref(), Some("for b"));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failed_outcome_is_not_kept() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));
    let keep_sent = |outcome: &String| outcome == "sent";

    let first = memo
        .run(Some("a"), keep_sent, counted(&runs, "failed"))
        .await;
    let second = memo.run(Some("a"), keep_sent, counted(&runs, "sent")).await;

    assert_eq!(first.as_deref(), Some("failed"));
    assert_eq!(second.as_deref(), Some("sent"));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_kept_outcome_answers_repeats_within_the_ttl() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));
    let keep_sent = |outcome: &String| outcome == "sent";

    let first = memo.run(Some("a"), keep_sent, counted(&runs, "sent")).await;
    tokio::time::advance(SENDONCE_TTL - Duration::from_secs(1)).await;
    let second = memo
        .run(Some("a"), keep_sent, counted(&runs, "failed"))
        .await;

    assert_eq!(first.as_deref(), Some("sent"));
    assert_eq!(second.as_deref(), Some("sent"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_caller_joins_the_run_in_progress() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = oneshot::channel::<()>();

    let first = {
        let memo = memo.clone();
        let runs = Arc::clone(&runs);
        tokio::spawn(async move {
            memo.run(Some("a"), keep_all, async move {
                runs.fetch_add(1, Ordering::SeqCst);
                started_tx.send(()).unwrap();
                release_rx.await.unwrap();
                "sent".to_owned()
            })
            .await
        })
    };
    started_rx.await.unwrap();

    let second = {
        let memo = memo.clone();
        let work = counted(&runs, "typed twice");
        tokio::spawn(async move { memo.run(Some("a"), keep_all, work).await })
    };
    tokio::task::yield_now().await;
    assert!(!second.is_finished(), "the second caller waits for the run");
    release_tx.send(()).unwrap();

    assert_eq!(first.await.unwrap().as_deref(), Some("sent"));
    assert_eq!(second.await.unwrap().as_deref(), Some("sent"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_work_answers_none_and_frees_the_key() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));

    let panicked = memo.run(Some("a"), keep_all, broken()).await;
    let next = memo.run(Some("a"), keep_all, counted(&runs, "sent")).await;

    assert_eq!(panicked, None);
    assert_eq!(next.as_deref(), Some("sent"));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn an_expired_outcome_runs_again() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));

    let first = memo.run(Some("a"), keep_all, counted(&runs, "first")).await;
    tokio::time::advance(SENDONCE_TTL + Duration::from_secs(1)).await;
    let second = memo
        .run(Some("a"), keep_all, counted(&runs, "second"))
        .await;

    assert_eq!(first.as_deref(), Some("first"));
    assert_eq!(second.as_deref(), Some("second"));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn no_key_never_deduplicates() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));

    let first = memo.run(None, keep_all, counted(&runs, "first")).await;
    let second = memo.run(None, keep_all, counted(&runs, "second")).await;

    assert_eq!(first.as_deref(), Some("first"));
    assert_eq!(second.as_deref(), Some("second"));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_caller_does_not_cancel_the_work() {
    let memo = SendOnce::<String>::new(SENDONCE_TTL);
    let runs = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = oneshot::channel::<()>();

    let caller = {
        let memo = memo.clone();
        let runs = Arc::clone(&runs);
        let finished = Arc::clone(&finished);
        tokio::spawn(async move {
            memo.run(Some("a"), keep_all, async move {
                runs.fetch_add(1, Ordering::SeqCst);
                started_tx.send(()).unwrap();
                release_rx.await.unwrap();
                finished.fetch_add(1, Ordering::SeqCst);
                "sent".to_owned()
            })
            .await
        })
    };
    started_rx.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());

    release_tx.send(()).unwrap();
    let repeat = memo
        .run(Some("a"), keep_all, counted(&runs, "typed twice"))
        .await;

    assert_eq!(repeat.as_deref(), Some("sent"));
    assert_eq!(finished.load(Ordering::SeqCst), 1);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}
