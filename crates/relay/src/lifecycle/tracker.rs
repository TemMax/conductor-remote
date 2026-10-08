//! The watcher's state, free of any system binding.

use tokio::sync::watch;

use crate::contract::ConductorStatus;

/// Whether one app is running, as told by launch and terminate events for every app on the system.
pub struct StatusTracker {
    bundle_id: String,
    sender: watch::Sender<ConductorStatus>,
}

impl StatusTracker {
    pub fn new(bundle_id: impl Into<String>, initial: ConductorStatus) -> Self {
        Self {
            bundle_id: bundle_id.into(),
            sender: watch::Sender::new(initial),
        }
    }

    /// The bundle identifier of the watched app.
    pub fn bundle_id(&self) -> &str {
        &self.bundle_id
    }

    pub fn status(&self) -> ConductorStatus {
        *self.sender.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<ConductorStatus> {
        self.sender.subscribe()
    }

    /// An app launched. `bundle_id` is `None` for an app that has no bundle identifier.
    pub fn on_launched(&self, bundle_id: Option<&str>) {
        self.record(bundle_id, ConductorStatus::Running);
    }

    /// An app terminated. `bundle_id` is `None` for an app that has no bundle identifier.
    pub fn on_terminated(&self, bundle_id: Option<&str>) {
        self.record(bundle_id, ConductorStatus::NotRunning);
    }

    fn record(&self, bundle_id: Option<&str>, status: ConductorStatus) {
        if bundle_id != Some(self.bundle_id.as_str()) {
            return;
        }
        // Subscribers are woken only when the status really changes.
        self.sender.send_if_modified(|current| {
            let changed = *current != status;
            *current = status;
            changed
        });
    }
}

/// Whether an instance of the watched app is still running after one of them terminated.
/// `instances` are the running instances as (process id, already terminated).
pub fn still_running(instances: &[(i32, bool)], terminated_pid: Option<i32>) -> bool {
    instances
        .iter()
        .any(|&(pid, terminated)| !terminated && Some(pid) != terminated_pid)
}
