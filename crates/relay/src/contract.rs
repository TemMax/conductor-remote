//! Types and interfaces shared by every unit of the relay.

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use subtle::ConstantTimeEq;
use tokio::sync::watch;

/// Loopback port the relay listens on unless `RELAY_PORT` overrides it.
pub const DEFAULT_PORT: u16 = 8790;
/// Bundle identifier of the Conductor desktop app.
pub const CONDUCTOR_BUNDLE_ID: &str = "com.conductor.app";
/// This app's bundle identifier; also the LaunchAgent label and the state directory name.
pub const APP_BUNDLE_ID: &str = "com.temmax.conductor-remote";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    pub state_dir: PathBuf,
}

/// The shared secret every `/api/*` request carries.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison, so response timing does not reveal the token.
    pub fn matches(&self, candidate: &str) -> bool {
        self.0.as_bytes().ct_eq(candidate.as_bytes()).into()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConductorStatus {
    Running,
    NotRunning,
}

impl ConductorStatus {
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Who is waiting on a UI command. The phone goes first; background work (agents, queues) waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Interactive,
    Background,
}

#[derive(Debug, thiserror::Error)]
#[error("could not launch Conductor: {0}")]
pub struct LaunchError(pub String);

/// Whether Conductor is running, and the one thing the relay may do about it.
pub trait ConductorControl: Send + Sync + 'static {
    fn status(&self) -> ConductorStatus;
    fn subscribe(&self) -> watch::Receiver<ConductorStatus>;
    fn launch(&self) -> Result<(), LaunchError>;
}

pub struct Asset {
    pub bytes: Cow<'static, [u8]>,
    pub content_type: String,
}

/// The built web app. `path` is relative, with no leading slash: `index.html`, `assets/app.js`.
pub trait Assets: Send + Sync + 'static {
    fn get(&self, path: &str) -> Option<Asset>;
}

#[derive(Clone)]
pub struct AppState {
    pub token: Arc<Token>,
    pub conductor: Arc<dyn ConductorControl>,
    pub assets: Arc<dyn Assets>,
    /// The reads over Conductor's database; `None` serves the skeleton `/api/state` and answers
    /// every other read route with 503.
    pub reads: Option<Arc<crate::reads::Reads>>,
    /// The writes; `None` answers every write route with 503.
    pub writes: Option<Arc<dyn crate::delivery::WriteService>>,
    /// Push notifications; `None` answers the push routes with 503.
    pub notify: Option<Arc<dyn crate::notify::NotifyService>>,
    pub services: Services,
}

/// Synced phone preferences: `GET /api/prefs` and `PATCH /api/prefs`. The service works on the
/// inner document; the route wraps it as `{"prefs": …}`.
pub trait PrefsService: Send + Sync + 'static {
    /// The whole preferences document (`readMarks`, `drafts`).
    fn get(&self) -> serde_json::Value;
    /// Sanitises and merges a partial document and returns the result; `Err` is the 400 text
    /// ("preferences must be an object" or "nothing to sync").
    fn patch(&self, patch: serde_json::Value) -> Result<serde_json::Value, String>;
}

/// Services added after the first milestones; every field defaults to `None`.
#[derive(Clone, Default)]
pub struct Services {
    pub prefs: Option<Arc<dyn PrefsService>>,
    pub host: Option<Arc<dyn crate::host::HostService>>,
    pub dev: Option<Arc<dyn crate::dev::DevServerService>>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ActuatorInfo {
    pub name: &'static str,
    pub caveat: &'static str,
    pub precise: bool,
    pub available: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ConductorInfo {
    pub running: bool,
}

/// `GET /api/state`.
#[derive(Debug, Serialize, PartialEq)]
pub struct StateResponse {
    pub workspaces: Vec<serde_json::Value>,
    pub actuator: ActuatorInfo,
    pub version: &'static str,
    pub conductor: ConductorInfo,
}

impl StateResponse {
    /// What the relay can say before it reads Conductor's database: no workspaces, and whether
    /// Conductor is running.
    pub fn skeleton(status: ConductorStatus) -> Self {
        Self {
            workspaces: Vec::new(),
            actuator: ActuatorInfo {
                name: "accessibility",
                caveat: "",
                precise: true,
                available: false,
            },
            version: VERSION,
            conductor: ConductorInfo {
                running: status.is_running(),
            },
        }
    }
}

/// `POST /api/conductor/launch`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LaunchResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Body of every failed `/api/*` request.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub error: String,
}
