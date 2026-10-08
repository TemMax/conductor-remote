//! The duplicate memo: one outcome per client id.
//!
//! The phone repeats a send under the same client id when an answer is slow or lost; the memo
//! answers every repeat with the first send's outcome instead of typing the prompt again.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// How long a kept outcome answers repeats.
pub const SENDONCE_TTL: Duration = Duration::from_secs(600);

/// What a running work publishes: `None` while it runs, then `Some(outcome)`, where a panicked
/// work's outcome is `None`.
type Outcome<T> = Option<Option<T>>;

enum Slot<T> {
    /// A run in progress; callers of the same key wait on it.
    Running(watch::Receiver<Outcome<T>>),
    /// A kept outcome and when it was kept.
    Done { value: T, at: Instant },
}

struct Memo<T> {
    ttl: Duration,
    slots: Mutex<HashMap<String, Slot<T>>>,
}

impl<T> Memo<T> {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Slot<T>>> {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Inserts `slot` under `key`, dropping every expired outcome first.
    fn insert(&self, slots: &mut HashMap<String, Slot<T>>, key: String, slot: Slot<T>) {
        let now = Instant::now();
        slots.retain(|_, kept| match kept {
            Slot::Done { at, .. } => now.duration_since(*at) < self.ttl,
            Slot::Running(_) => true,
        });
        slots.insert(key, slot);
    }
}

/// One outcome per client id. Cheap to clone; clones share the memo.
#[derive(Clone)]
pub struct SendOnce<T> {
    memo: Arc<Memo<T>>,
}

impl<T: Clone + Send + Sync + 'static> SendOnce<T> {
    pub fn new(ttl: Duration) -> SendOnce<T> {
        SendOnce {
            memo: Arc::new(Memo {
                ttl,
                slots: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Without a key, runs `work` and returns its outcome. With a key: a kept outcome younger
    /// than the TTL is returned without running anything; a run of the same key in progress is
    /// joined; otherwise `work` runs in its own task (`tokio::spawn`, so a dropped caller does not
    /// cancel it) and its outcome is kept when `keep(&outcome)`. A run whose work panicked keeps
    /// nothing, answers its callers with `None`, and frees the key.
    ///
    /// The keyless run is spawned as well, so it too survives a dropped caller and answers `None`
    /// when its work panics.
    pub async fn run<W, K>(&self, key: Option<&str>, keep: K, work: W) -> Option<T>
    where
        W: Future<Output = T> + Send + 'static,
        K: Fn(&T) -> bool + Send + 'static,
    {
        let Some(key) = key else {
            return tokio::spawn(work).await.ok();
        };

        let mut receiver = {
            let mut slots = self.memo.lock();
            match slots.get(key) {
                Some(Slot::Done { value, at })
                    if Instant::now().duration_since(*at) < self.memo.ttl =>
                {
                    return Some(value.clone());
                }
                Some(Slot::Running(receiver)) => receiver.clone(),
                _ => {
                    let (sender, receiver) = watch::channel(None);
                    self.memo
                        .insert(&mut slots, key.to_owned(), Slot::Running(receiver.clone()));
                    self.start(key.to_owned(), keep, work, sender);
                    receiver
                }
            }
        };

        let answer = match receiver.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().flatten(),
            // The run's task went away without an answer (the runtime is shutting down).
            Err(_) => None,
        };
        answer
    }

    /// Spawns the run of `key`: the work in a task of its own, so a panic surfaces as a join
    /// error, and a task that records its outcome and answers the callers.
    fn start<W, K>(&self, key: String, keep: K, work: W, sender: watch::Sender<Outcome<T>>)
    where
        W: Future<Output = T> + Send + 'static,
        K: Fn(&T) -> bool + Send + 'static,
    {
        let memo = Arc::clone(&self.memo);
        tokio::spawn(async move {
            let outcome = tokio::spawn(work).await.ok();
            {
                let mut slots = memo.lock();
                match &outcome {
                    Some(value) if keep(value) => {
                        let done = Slot::Done {
                            value: value.clone(),
                            at: Instant::now(),
                        };
                        memo.insert(&mut slots, key, done);
                    }
                    _ => {
                        slots.remove(&key);
                    }
                }
            }
            sender.send_replace(Some(outcome));
        });
    }
}
