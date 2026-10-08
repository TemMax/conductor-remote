//! Facts that do not live in Conductor's database: change stats, pull requests, processes.

pub mod change_stats;
pub mod commands;
pub mod pr;
pub mod processes;
pub mod swr;

use std::path::PathBuf;
use std::sync::Arc;

/// What every source of extras shares.
pub struct Shared {
    pub commands: Arc<dyn commands::Commands>,
    pub pool: Arc<swr::Pool>,
    pub revision: Arc<swr::Revision>,
    /// The user's home directory.
    pub home: PathBuf,
}

/// The facts `/api/state` and the chat list attach that are not in Conductor's database.
pub struct Extras {
    shared: Arc<Shared>,
    pub change_stats: change_stats::ChangeStatsSource,
    pub pr: pr::PrSource,
    pub processes: processes::ProcessSource,
}

impl Extras {
    /// A pool of 4 threads and a fresh revision.
    pub fn new(commands: Arc<dyn commands::Commands>, home: impl Into<PathBuf>) -> Self {
        let shared = Arc::new(Shared {
            commands,
            pool: swr::Pool::new(4),
            revision: Arc::new(swr::Revision::new()),
            home: home.into(),
        });
        Self {
            change_stats: change_stats::ChangeStatsSource::new(shared.clone()),
            pr: pr::PrSource::new(shared.clone()),
            processes: processes::ProcessSource::new(shared.clone()),
            shared,
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn revision(&self) -> u64 {
        self.shared.revision.get()
    }

    /// Blocks until no background refresh is queued or running. For tests.
    pub fn wait_idle(&self) {
        self.shared.pool.wait_idle();
    }
}
