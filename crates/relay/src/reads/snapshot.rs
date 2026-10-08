//! The answers the relay keeps while Conductor's database does not change.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::extras::swr::Revision;
use crate::db::{ConductorDb, DataVersion, DbError};

/// More distinct answers than any real Conductor has workspaces: past this the cache starts over
/// rather than grow with the ids a client asks for.
const MAX_ENTRIES: usize = 256;

/// How long a `Key::State` body is reused: it holds facts from the file system (worktree
/// directories, icon files), which change without a database commit.
const STATE_MAX_AGE: Duration = Duration::from_secs(10);

/// How long either body is reused when the extras are attached: the shortest lifetime of a fact
/// they hold (the run listing and the process list are trusted for 5 seconds).
const EXTRAS_MAX_AGE: Duration = Duration::from_secs(5);

/// What a cached body answers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// `GET /api/state`.
    State,
    /// `GET /api/workspaces/:id/sessions`, by workspace id.
    Sessions(String),
}

/// Serialised response bodies, valid while `ConductorDb::data_version` and the extras revision
/// stay equal to the values they were built under. Any other value, or `clear`, drops all of
/// them. A body is also dropped once it is older than its maximum age: the state maximum age for
/// `Key::State`, the chat-list maximum age (if there is one) for `Key::Sessions`.
pub struct Snapshot {
    state_max_age: Duration,
    sessions_max_age: Option<Duration>,
    /// The extras' revision counter; `None` without extras, where the revision is a constant.
    revision: Option<Arc<Revision>>,
    inner: Mutex<Inner>,
}

/// The short-lived table lock: held to look a slot up, never while a body is built.
#[derive(Default)]
struct Inner {
    version: Option<DataVersion>,
    revision: u64,
    slots: HashMap<Key, Arc<Slot>>,
}

/// One key's body. The slot lock is held while that key's body is built, so concurrent callers of
/// the key share the build and callers of other keys are not held up.
#[derive(Default)]
struct Slot {
    entry: Mutex<Option<Entry>>,
}

struct Entry {
    version: DataVersion,
    revision: u64,
    /// When the build that made `body` started: the facts in it are at least this old.
    built_at: Instant,
    body: Vec<u8>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::with_state_max_age(STATE_MAX_AGE)
    }
}

impl Snapshot {
    /// A snapshot that reuses a `Key::State` body for at most `max_age`; `default()` uses 10
    /// seconds.
    pub fn with_state_max_age(max_age: Duration) -> Self {
        Self {
            state_max_age: max_age,
            sessions_max_age: None,
            revision: None,
            inner: Mutex::default(),
        }
    }

    /// A snapshot for a `Reads` with extras: bodies are also tied to `revision`, and both kinds
    /// are reused for at most 5 seconds.
    pub fn with_revision(revision: Arc<Revision>) -> Self {
        Self {
            state_max_age: EXTRAS_MAX_AGE,
            sessions_max_age: Some(EXTRAS_MAX_AGE),
            revision: Some(revision),
            inner: Mutex::default(),
        }
    }

    /// The same snapshot, reusing a `Key::Sessions` body for at most `max_age`.
    pub fn with_sessions_max_age(mut self, max_age: Duration) -> Self {
        self.sessions_max_age = Some(max_age);
        self
    }

    /// The cached body for `key`, or the one `build` makes, kept for the next call.
    ///
    /// A body is reused while the data version and the extras revision are equal and it is
    /// younger than the maximum age of its key. Without extras the revision is a constant and
    /// `Key::Sessions` bodies hold database facts only, so they have no age limit. The version
    /// and the revision are read before `build` runs, so a commit that lands during the build
    /// leaves a body labelled with the older version, which the next call replaces.
    /// The lock of `key`'s slot is held while building, so concurrent callers of the same key
    /// share one build while callers of other keys are not blocked; keep the call on a thread
    /// that may block. A failed build caches nothing, and a build that finishes after its
    /// version or the revision has moved is returned but not kept.
    pub fn get_or_build<E: From<DbError>>(
        &self,
        db: &ConductorDb,
        key: Key,
        build: impl FnOnce() -> Result<Vec<u8>, E>,
    ) -> Result<Vec<u8>, E> {
        let current = db.data_version()?;
        let revision = self.revision();
        let max_age = match key {
            Key::State => Some(self.state_max_age),
            Key::Sessions(_) => self.sessions_max_age,
        };
        let slot = self.slot(current, revision, key);
        let mut entry = slot.entry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = entry.as_ref() {
            let fresh = max_age.is_none_or(|max| cached.built_at.elapsed() < max);
            if cached.version == current && cached.revision == revision && fresh {
                return Ok(cached.body.clone());
            }
        }
        let built_at = Instant::now();
        let body = build()?;
        // Only a body of the version and revision still current is kept: a newer one must not be
        // overwritten by a build that started before its version was replaced, and the facts of
        // the extras may have moved while it was built.
        if self.lock().version == Some(current) && self.revision() == revision {
            *entry = Some(Entry {
                version: current,
                revision,
                built_at,
                body: body.clone(),
            });
        } else {
            *entry = None;
        }
        Ok(body)
    }

    /// Drops every body and forgets the version.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.version = None;
        inner.slots.clear();
    }

    /// The extras revision now; a constant without extras.
    fn revision(&self) -> u64 {
        self.revision.as_ref().map_or(0, |revision| revision.get())
    }

    /// The slot of `key` under `version` and `revision`; another of either first drops every slot.
    fn slot(&self, version: DataVersion, revision: u64, key: Key) -> Arc<Slot> {
        let mut inner = self.lock();
        if inner.version != Some(version) || inner.revision != revision {
            inner.slots.clear();
            inner.version = Some(version);
            inner.revision = revision;
        }
        if !inner.slots.contains_key(&key) && inner.slots.len() >= MAX_ENTRIES {
            // Start over, but keep the slots a caller is using so their builds stay shared.
            inner.slots.retain(|_, slot| Arc::strong_count(slot) > 1);
        }
        Arc::clone(inner.slots.entry(key).or_default())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while the table is held leaves it consistent: recover the guard.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}
