//! The read side: queries over Conductor's database.

pub mod background;
pub mod context;
pub mod extras;
pub mod images;
pub mod messages;
pub mod models;
pub mod receipts;
pub mod review;
pub mod sessions;
pub mod snapshot;
pub mod states;
pub mod workspaces;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use crate::contract::ConductorStatus;
use crate::db::ConductorDb;
use crate::files::{ExposeMode, PreviewRoots};
use crate::search::index::SearchIndex;
use crate::usage::plan::PlanUsageService;
use crate::usage::tools::ToolUsageService;
use snapshot::Snapshot;

/// The shared handle every read goes through.
pub struct Reads {
    db: Arc<ConductorDb>,
    workspaces_root: PathBuf,
    snapshot: Snapshot,
    extras: Option<Arc<extras::Extras>>,
    host_paths: Option<HostPaths>,
    preview: Option<(PreviewRoots, ExposeMode)>,
    search: Option<Arc<SearchIndex>>,
    usage: Option<Usage>,
}

/// The two services behind the usage sheet.
struct Usage {
    plan: Arc<PlanUsageService>,
    tools: Arc<ToolUsageService>,
}

/// Directories of this Mac the reads need besides Conductor's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPaths {
    /// The user's home directory.
    pub home: PathBuf,
    /// The relay's own state directory.
    pub state_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
}

impl Reads {
    pub fn new(
        db: std::sync::Arc<crate::db::ConductorDb>,
        workspaces_root: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            db,
            workspaces_root: workspaces_root.into(),
            snapshot: Snapshot::default(),
            extras: None,
            host_paths: None,
            preview: None,
            search: None,
            usage: None,
        }
    }

    /// Attaches the facts that do not live in the database. The cached answers then also follow
    /// the extras' revision and live at most 5 seconds.
    pub fn with_extras(mut self, extras: Arc<extras::Extras>) -> Self {
        self.snapshot = Snapshot::with_revision(extras.shared().revision.clone());
        self.extras = Some(extras);
        self
    }

    pub fn extras(&self) -> Option<&Arc<extras::Extras>> {
        self.extras.as_ref()
    }

    pub fn with_host_paths(mut self, paths: HostPaths) -> Self {
        self.host_paths = Some(paths);
        self
    }

    pub fn host_paths(&self) -> Option<&HostPaths> {
        self.host_paths.as_ref()
    }

    /// Sets where file previews and local images may be read from, and how far the relay is exposed.
    pub fn with_preview(mut self, roots: PreviewRoots, mode: ExposeMode) -> Self {
        self.preview = Some((roots, mode));
        self
    }

    pub fn preview(&self) -> Option<(&PreviewRoots, ExposeMode)> {
        self.preview.as_ref().map(|(roots, mode)| (roots, *mode))
    }

    /// Attaches the full-text index the search route reads. Without one, search finds workspaces
    /// by name only.
    pub fn with_search(mut self, index: Arc<SearchIndex>) -> Self {
        self.search = Some(index);
        self
    }

    pub fn search_index(&self) -> Option<&Arc<SearchIndex>> {
        self.search.as_ref()
    }

    /// Attaches the services of the usage routes: the plan allowances and the tool traffic.
    pub fn with_usage(mut self, plan: Arc<PlanUsageService>, tools: Arc<ToolUsageService>) -> Self {
        self.usage = Some(Usage { plan, tools });
        self
    }

    pub fn plan_usage(&self) -> Option<&Arc<PlanUsageService>> {
        self.usage.as_ref().map(|usage| &usage.plan)
    }

    pub fn tool_usage(&self) -> Option<&Arc<ToolUsageService>> {
        self.usage.as_ref().map(|usage| &usage.tools)
    }

    pub fn db(&self) -> &crate::db::ConductorDb {
        &self.db
    }

    pub fn workspaces_root(&self) -> &std::path::Path {
        &self.workspaces_root
    }

    /// The answers cached while the database is unchanged.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Drops the cached answers and the connection; the next read opens a new one. Takes the
    /// locks of both, so call it from a thread that may block.
    pub fn close(&self) {
        self.snapshot.clear();
        self.db.close();
    }
}

/// Closes `reads` every time the Conductor status is "not running", until the sender is gone.
/// Run it on the server's runtime; the closing itself runs on the blocking pool.
pub async fn close_when_not_running(
    reads: Arc<Reads>,
    mut status: watch::Receiver<ConductorStatus>,
) {
    loop {
        let running = status.borrow_and_update().is_running();
        if !running {
            let reads = reads.clone();
            // A panic in `close` ends nothing but this attempt: the next change tries again.
            let _ = tokio::task::spawn_blocking(move || reads.close()).await;
        }
        if status.changed().await.is_err() {
            return;
        }
    }
}
