//! Follows Conductor: whether it is running, and launching it.

mod tracker;
mod workspace;

use std::process::Command;
use std::sync::Arc;

use tokio::sync::watch;

use crate::contract::{ConductorControl, ConductorStatus, LaunchError};

pub use tracker::{still_running, StatusTracker};

/// The system tool that asks Launch Services to open an app.
const OPEN: &str = "/usr/bin/open";

/// Start watching the app with this bundle identifier. Call on the main thread.
pub fn start(bundle_id: &str) -> Arc<dyn ConductorControl> {
    // The snapshot and the notifications are both driven by the main run loop, which is not
    // running yet, so nothing can slip in between reading the one and observing the other.
    let tracker = Arc::new(StatusTracker::new(
        bundle_id,
        workspace::current_status(bundle_id),
    ));
    workspace::observe(Arc::clone(&tracker));
    Arc::new(Conductor { tracker })
}

/// Run the main thread's run loop, which delivers the system notifications. Never returns.
pub fn run_main_loop() -> ! {
    workspace::run_main_loop()
}

struct Conductor {
    tracker: Arc<StatusTracker>,
}

impl ConductorControl for Conductor {
    fn status(&self) -> ConductorStatus {
        self.tracker.status()
    }

    fn subscribe(&self) -> watch::Receiver<ConductorStatus> {
        self.tracker.subscribe()
    }

    /// Ask the system to open the app. Returns once the request is accepted, not once the app is
    /// up: the launch notification moves the status.
    fn launch(&self) -> Result<(), LaunchError> {
        let output = Command::new(OPEN)
            .arg("-b")
            .arg(self.tracker.bundle_id())
            .output()
            .map_err(|error| LaunchError(format!("{OPEN}: {error}")))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        Err(LaunchError(if stderr.is_empty() {
            format!("{OPEN} failed: {}", output.status)
        } else {
            stderr.to_owned()
        }))
    }
}
