//! Quit and relaunch Conductor when the phone asks.

use std::sync::Arc;
use std::time::Duration;

use objc2_app_kit::NSRunningApplication;
use objc2_foundation::NSString;
use serde_json::json;
use tokio::time::Instant;

use crate::contract::{ConductorControl, CONDUCTOR_BUNDLE_ID};
use crate::delivery::WriteAnswer;
use crate::ui::ax::{is_trusted, Element};
use crate::ui::snapshot::SnapshotSource;

/// What the restart does to the app. Every call blocks, so [`restart`] runs each on
/// `spawn_blocking`.
pub trait AppControl: Send + Sync + 'static {
    /// Whether Conductor is running.
    fn running(&self) -> bool;
    /// Asks Conductor to quit; whether the request was made.
    fn terminate(&self) -> bool;
    /// Starts Conductor.
    fn launch(&self) -> Result<(), String>;
    /// Whether Conductor shows a window.
    fn has_window(&self) -> bool;
}

/// How long each wait lasts and how often it looks.
#[derive(Clone, Copy, Debug)]
pub struct RestartTimings {
    /// How long Conductor gets to quit.
    pub quit_wait: Duration,
    /// How long the relaunched Conductor gets to show a window.
    pub window_wait: Duration,
    /// The pause between two looks.
    pub poll: Duration,
}

impl Default for RestartTimings {
    fn default() -> Self {
        RestartTimings {
            quit_wait: Duration::from_secs(15),
            window_wait: Duration::from_secs(30),
            poll: Duration::from_millis(250),
        }
    }
}

/// When the window wait asks Conductor for a window again, counted from the first launch: the
/// launch is `open -b`, which on a running app is the reopen request that makes it draw one.
const REOPEN_AFTER: [Duration; 2] = [Duration::from_secs(3), Duration::from_secs(10)];

const LOCKED: &str = "The Mac is locked - unlock it before restarting Conductor.";
const DID_NOT_QUIT: &str = "Conductor did not quit — quit it on your Mac and try again.";
const NO_WINDOW: &str = "Conductor started but showed no window in time.";

fn failure(status: u16, message: &str) -> WriteAnswer {
    WriteAnswer::json(status, json!({ "ok": false, "error": message }))
}

/// Runs one blocking call of the app off the async threads.
async fn blocking<T: Send + 'static>(
    app: &Arc<dyn AppControl>,
    call: impl FnOnce(&dyn AppControl) -> T + Send + 'static,
) -> T {
    let app = Arc::clone(app);
    tokio::task::spawn_blocking(move || call(app.as_ref()))
        .await
        .expect("the app control call panicked")
}

/// Looks first, then sleeps `poll`, until `done` holds (true) or `limit` has passed (false). The
/// last look is at `limit` itself.
async fn wait_until(
    app: &Arc<dyn AppControl>,
    limit: Duration,
    poll: Duration,
    done: fn(&dyn AppControl) -> bool,
) -> bool {
    let started = Instant::now();
    loop {
        if blocking(app, done).await {
            return true;
        }
        if started.elapsed() >= limit {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Looks for the window like [`wait_until`], and asks for it again with `launch` at the first look
/// at or after each of [`REOPEN_AFTER`] while none has shown. A failed ask is logged and waited
/// out.
async fn wait_for_window(app: &Arc<dyn AppControl>, limit: Duration, poll: Duration) -> bool {
    let started = Instant::now();
    let mut reopened = 0;
    loop {
        if blocking(app, |app| app.has_window()).await {
            return true;
        }
        let elapsed = started.elapsed();
        if elapsed >= limit {
            return false;
        }
        if reopened < REOPEN_AFTER.len() && elapsed >= REOPEN_AFTER[reopened] {
            reopened += 1;
            if let Err(message) = blocking(app, |app| app.launch()).await {
                tracing::warn!("asking Conductor for its window again failed: {message}");
            }
        }
        tokio::time::sleep(poll).await;
    }
}

/// `POST /api/conductor/restart`. `working` is the number of chats mid-turn, `locked` whether the
/// Mac's screen is locked.
pub async fn restart(
    app: Arc<dyn AppControl>,
    working: usize,
    locked: bool,
    stop_agents: bool,
    timings: RestartTimings,
) -> WriteAnswer {
    if locked {
        // A relaunch behind the lock screen comes up without a window.
        return failure(409, LOCKED);
    }
    if working > 0 && !stop_agents {
        let message = if working == 1 {
            "1 chat is mid-turn. Restarting Conductor ends it.".to_string()
        } else {
            format!("{working} chats are mid-turn. Restarting Conductor ends them.")
        };
        return WriteAnswer::json(
            409,
            json!({
                "ok": false,
                "agentsRunning": true,
                "working": working,
                "error": message,
            }),
        );
    }

    let started = Instant::now();

    if blocking(&app, |app| app.running()).await {
        blocking(&app, |app| app.terminate()).await;
        if !wait_until(&app, timings.quit_wait, timings.poll, |app| !app.running()).await {
            return failure(502, DID_NOT_QUIT);
        }
    }

    if let Err(message) = blocking(&app, |app| app.launch()).await {
        return failure(502, &message);
    }
    if !wait_for_window(&app, timings.window_wait, timings.poll).await {
        return failure(502, NO_WINDOW);
    }

    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    WriteAnswer::json(200, json!({ "ok": true, "ms": ms }))
}

/// The real Conductor: AppKit to quit it, the Accessibility API to see its window.
pub struct SystemAppControl {
    conductor: Arc<dyn ConductorControl>,
}

impl SystemAppControl {
    pub fn new(conductor: Arc<dyn ConductorControl>) -> SystemAppControl {
        SystemAppControl { conductor }
    }

    fn conductor_pid() -> Option<i32> {
        NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(
            CONDUCTOR_BUNDLE_ID,
        ))
        .iter()
        .filter(|app| !app.isTerminated())
        .map(|app| app.processIdentifier())
        .find(|pid| *pid != -1)
    }
}

impl AppControl for SystemAppControl {
    fn running(&self) -> bool {
        self.conductor.status().is_running()
    }

    fn terminate(&self) -> bool {
        let mut asked = false;
        for app in NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(CONDUCTOR_BUNDLE_ID),
        )
        .iter()
        {
            asked |= app.terminate();
        }
        asked
    }

    fn launch(&self) -> Result<(), String> {
        self.conductor.launch().map_err(|error| error.to_string())
    }

    fn has_window(&self) -> bool {
        if !is_trusted(false) {
            // Without Accessibility there is no looking; a running Conductor has to do.
            return self.running();
        }
        let Some(pid) = Self::conductor_pid() else {
            return false;
        };
        Element::application(pid)
            .children()
            .iter()
            .any(|child| child.read().role.as_deref() == Some("AXWindow"))
    }
}
