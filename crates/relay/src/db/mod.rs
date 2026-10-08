//! One read-only handle to Conductor's SQLite file.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const SLOW_READ: Duration = Duration::from_millis(100);
const SLOW_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Read-only access to Conductor's database. The connection is opened on first use and
/// reopened when the file at the path is replaced.
pub struct ConductorDb {
    path: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    open: Option<Open>,
    generation: u64,
    slow: HashMap<&'static str, SlowLog>,
}

struct Open {
    conn: Connection,
    file: FileId,
}

/// Device and inode of the database file.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

#[derive(Default)]
struct SlowLog {
    last_logged: Option<Instant>,
    suppressed: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("cannot open the Conductor database at {path}: {source}")]
    Open {
        path: std::path::PathBuf,
        source: rusqlite::Error,
    },
    #[error(transparent)]
    Query(#[from] rusqlite::Error),
}

/// `generation` counts the connections this handle has opened; `version` is SQLite's
/// `PRAGMA data_version` on the current one, which changes when another connection commits.
/// The pragma restarts with every new connection, so two values are comparable only when
/// their generations are equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataVersion {
    pub generation: u64,
    pub version: i64,
}

impl ConductorDb {
    /// Opens nothing: the connection is made on first use.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: path.into(),
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Runs `f` on the connection. `label` names the query in the slow-query log.
    ///
    /// `f` runs while the handle's lock is held: keep file-system and subprocess work out of it.
    /// An error from `f` is returned as it is; there is no retry.
    pub fn read<T>(
        &self,
        label: &'static str,
        f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Result<T, DbError> {
        let started = Instant::now();
        let mut inner = self.lock();
        let result = inner
            .connection(&self.path)
            .and_then(|conn| f(conn).map_err(DbError::Query));
        inner.note_duration(label, started.elapsed());
        result
    }

    /// Identifies the state of the data as this handle sees it; see `DataVersion`.
    pub fn data_version(&self) -> Result<DataVersion, DbError> {
        let mut inner = self.lock();
        let version =
            inner
                .connection(&self.path)?
                .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?;
        Ok(DataVersion {
            generation: inner.generation,
            version,
        })
    }

    /// Drops the connection; the next `read` opens a new one.
    pub fn close(&self) {
        self.lock().open = None;
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic inside a caller's closure leaves the state consistent: recover the guard.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Inner {
    /// The connection to the file now at `path`, opening a new one when there is none or the
    /// file was replaced since it was opened.
    fn connection(&mut self, path: &Path) -> Result<&Connection, DbError> {
        // Taken before opening: a replacement racing the open is seen on the next call.
        let current = std::fs::metadata(path).ok().map(|m| FileId {
            dev: m.dev(),
            ino: m.ino(),
        });
        let reusable = match (&self.open, current) {
            (Some(open), Some(current)) => open.file == current,
            _ => false,
        };
        if !reusable {
            // Close the old connection first so the new one does not share its WAL index.
            self.open = None;
            let conn = open_connection(path).map_err(|source| DbError::Open {
                path: path.to_path_buf(),
                source,
            })?;
            // The file vanished between the two calls: the next call sees it and retries.
            let file = current.unwrap_or(FileId { dev: 0, ino: 0 });
            self.generation += 1;
            self.open = Some(Open { conn, file });
        }
        match &self.open {
            Some(open) => Ok(&open.conn),
            None => unreachable!("a connection was just opened"),
        }
    }

    fn note_duration(&mut self, label: &'static str, took: Duration) {
        if took <= SLOW_READ {
            return;
        }
        let entry = self.slow.entry(label).or_default();
        let now = Instant::now();
        let due = entry
            .last_logged
            .is_none_or(|at| now.duration_since(at) >= SLOW_LOG_INTERVAL);
        if due {
            tracing::warn!(
                label,
                duration_ms = took.as_millis() as u64,
                suppressed = entry.suppressed,
                "slow Conductor DB read"
            );
            entry.last_logged = Some(now);
            entry.suppressed = 0;
        } else {
            entry.suppressed += 1;
        }
    }
}

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}
