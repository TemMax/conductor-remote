use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use conductor_remote::host::parent::parent_gone;
use tokio::sync::oneshot::error::TryRecvError;

const EVERY: Duration = Duration::from_millis(10);
/// How long a test waits for the watching thread before it fails.
const PATIENCE: Duration = Duration::from_secs(5);

/// A parent id a test can change, and how often it was asked for.
struct Source {
    id: Arc<AtomicU32>,
    calls: Arc<AtomicUsize>,
}

impl Source {
    fn new(id: u32) -> Self {
        Self {
            id: Arc::new(AtomicU32::new(id)),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn reader(&self) -> impl Fn() -> u32 + Send + 'static {
        let id = Arc::clone(&self.id);
        let calls = Arc::clone(&self.calls);
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            id.load(Ordering::SeqCst)
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Whether the watching thread still holds the reader: it drops it when it ends.
    fn held(&self) -> bool {
        Arc::strong_count(&self.calls) > 1
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn pending_while_the_parent_is_the_same_and_resolves_once_it_changes() {
    let source = Source::new(4242);
    let mut gone = parent_gone(source.reader(), EVERY, None);

    // One read at the call, then several looks.
    wait_until("the parent was looked at several times", || {
        source.calls() >= 5
    });
    assert_eq!(gone.try_recv(), Err(TryRecvError::Empty));

    source.id.store(1, Ordering::SeqCst);
    let changed = Instant::now();
    wait_until("the receiver resolved", || gone.try_recv() == Ok(()));
    assert!(
        changed.elapsed() < Duration::from_secs(2),
        "took {:?} to notice",
        changed.elapsed()
    );

    // The thread ends after sending.
    wait_until("the thread ended", || !source.held());
}

#[test]
fn the_id_is_read_at_the_call() {
    let source = Source::new(7);
    let mut gone = parent_gone(source.reader(), EVERY, None);
    // Changed before the first look: only an id read at the call can tell.
    source.id.store(8, Ordering::SeqCst);

    wait_until("the receiver resolved", || gone.try_recv() == Ok(()));
}

#[test]
fn dropping_the_receiver_ends_the_thread() {
    let source = Source::new(4242);
    let gone = parent_gone(source.reader(), EVERY, None);
    wait_until("the parent was looked at", || source.calls() >= 3);
    assert!(source.held());

    drop(gone);
    wait_until("the thread ended", || !source.held());

    let calls = source.calls();
    std::thread::sleep(EVERY * 10);
    assert_eq!(source.calls(), calls, "the source was called after the end");
}

#[test]
fn a_parent_that_is_already_gone_resolves_at_once() {
    // The source never returns 41: the parent was gone before the first look.
    let source = Source::new(42);
    let mut gone = parent_gone(source.reader(), Duration::from_secs(10), Some(41));

    wait_until("the receiver resolved", || gone.try_recv() == Ok(()));
}

#[test]
fn an_expected_parent_that_is_still_there_keeps_waiting() {
    let source = Source::new(42);
    let mut gone = parent_gone(source.reader(), EVERY, Some(42));

    wait_until("the parent was looked at several times", || {
        source.calls() >= 5
    });
    assert_eq!(gone.try_recv(), Err(TryRecvError::Empty));

    source.id.store(1, Ordering::SeqCst);
    wait_until("the receiver resolved", || gone.try_recv() == Ok(()));
}
