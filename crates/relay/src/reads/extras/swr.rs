//! A cache whose reads never wait: a stale value is returned at once while one refresh runs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts changes of cached values; cached response bodies are valid for one revision.
#[derive(Debug)]
pub struct Revision(AtomicU64);

impl Revision {
    pub fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Default for Revision {
    fn default() -> Self {
        Self::new()
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct PoolState {
    queue: VecDeque<Job>,
    /// Worker threads started so far.
    workers: usize,
    /// Workers blocked on the queue.
    idle: usize,
    /// Jobs taken from the queue and not yet finished.
    running: usize,
    /// Set when the pool is dropped: workers leave once the queue is empty.
    closed: bool,
}

/// What the workers share; they hold it, not the `Pool`, so dropping the `Pool` ends them.
struct PoolInner {
    max_parallel: usize,
    state: Mutex<PoolState>,
    /// Signalled when a job is queued or the pool closes.
    work: Condvar,
    /// Signalled when the queue is empty and no job is running.
    idle: Condvar,
}

/// Runs queued jobs on at most `max_parallel` threads.
pub struct Pool {
    inner: Arc<PoolInner>,
}

impl Pool {
    pub fn new(max_parallel: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(PoolInner {
                max_parallel: max_parallel.max(1),
                state: Mutex::new(PoolState {
                    queue: VecDeque::new(),
                    workers: 0,
                    idle: 0,
                    running: 0,
                    closed: false,
                }),
                work: Condvar::new(),
                idle: Condvar::new(),
            }),
        })
    }

    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        let inner = &self.inner;
        let mut state = lock(&inner.state);
        state.queue.push_back(Box::new(job));
        // A worker is started only when no idle one can take the job.
        if state.idle < state.queue.len() && state.workers < inner.max_parallel {
            let worker = inner.clone();
            let spawned = std::thread::Builder::new()
                .name("extras-pool".to_owned())
                .spawn(move || worker.work_loop());
            if spawned.is_ok() {
                state.workers += 1;
            }
        }
        drop(state);
        inner.work.notify_one();
    }

    /// Blocks until the queue is empty and no job is running. For tests.
    pub fn wait_idle(&self) {
        let mut state = lock(&self.inner.state);
        while !state.queue.is_empty() || state.running > 0 {
            state = self
                .inner
                .idle
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        lock(&self.inner.state).closed = true;
        self.inner.work.notify_all();
    }
}

impl PoolInner {
    fn work_loop(&self) {
        loop {
            let job = {
                let mut state = lock(&self.state);
                loop {
                    if let Some(job) = state.queue.pop_front() {
                        state.running += 1;
                        break job;
                    }
                    if state.closed {
                        return;
                    }
                    state.idle += 1;
                    state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
                    state.idle -= 1;
                }
            };
            // A panicking job must not take the thread down.
            let _ = catch_unwind(AssertUnwindSafe(job));
            let mut state = lock(&self.state);
            state.running -= 1;
            if state.queue.is_empty() && state.running == 0 {
                self.idle.notify_all();
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry<V> {
    pub value: V,
    /// When the refresh that produced the value finished.
    pub at: Instant,
}

struct SwrState<K, V> {
    entries: HashMap<K, Entry<V>>,
    in_flight: HashSet<K>,
}

pub struct Swr<K, V> {
    pool: Arc<Pool>,
    revision: Arc<Revision>,
    state: Arc<Mutex<SwrState<K, V>>>,
}

impl<K: Clone + Eq + Hash + Send + 'static, V: Clone + PartialEq + Send + 'static> Swr<K, V> {
    pub fn new(pool: Arc<Pool>, revision: Arc<Revision>) -> Self {
        Self {
            pool,
            revision,
            state: Arc::new(Mutex::new(SwrState {
                entries: HashMap::new(),
                in_flight: HashSet::new(),
            })),
        }
    }

    /// The stored value, or `None` before the first refresh finished. Never blocks on `refresh`.
    pub fn get(
        &self,
        key: &K,
        is_stale: impl FnOnce(&Entry<V>) -> bool,
        refresh: impl FnOnce() -> V + Send + 'static,
    ) -> Option<V> {
        let entry = lock(&self.state).entries.get(key).cloned();
        let wanted = entry.as_ref().is_none_or(is_stale);
        if wanted && lock(&self.state).in_flight.insert(key.clone()) {
            let state = self.state.clone();
            let revision = self.revision.clone();
            let key = key.clone();
            self.pool.submit(move || {
                // Clears the in-flight mark however the refresh ends.
                struct Clear<K: Eq + Hash, V> {
                    state: Arc<Mutex<SwrState<K, V>>>,
                    key: K,
                }
                impl<K: Eq + Hash, V> Drop for Clear<K, V> {
                    fn drop(&mut self) {
                        lock(&self.state).in_flight.remove(&self.key);
                    }
                }
                let _clear = Clear {
                    state: state.clone(),
                    key: key.clone(),
                };
                let value = refresh();
                let mut guard = lock(&state);
                let changed = guard.entries.get(&key).is_none_or(|old| old.value != value);
                guard.entries.insert(
                    key,
                    Entry {
                        value,
                        at: Instant::now(),
                    },
                );
                if changed {
                    revision.bump();
                }
            });
        }
        entry.map(|e| e.value)
    }

    /// The stored value without scheduling anything.
    pub fn peek(&self, key: &K) -> Option<V> {
        lock(&self.state).entries.get(key).map(|e| e.value.clone())
    }
}
