//! Web Push: tells the phone when a turn ends or errors, and when a parked prompt was sent.

pub mod sender;
pub mod service;
pub mod watcher;
pub mod webpush;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::delivery::BoxFuture;

/// The safe half of a device: no endpoint, no keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub id: String,
    pub label: String,
    pub created_at: i64,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    pub failures: u32,
}

/// `GET /api/push`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushConfig {
    pub enabled: bool,
    pub public_key: String,
    pub devices: Vec<DeviceInfo>,
}

/// A browser's push subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// What the service worker shows. `kind` is "done", "error" or "test"; `ts` in milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PushMessage {
    pub title: String,
    pub body: String,
    pub tag: String,
    pub url: String,
    pub kind: String,
    pub ts: i64,
}

/// The push routes and the viewing heartbeat call this. `service::Notifier` is the real one.
pub trait NotifyService: Send + Sync + 'static {
    fn config(&self) -> Result<PushConfig, String>;
    /// Returns the device's id and every device.
    fn subscribe(
        &self,
        subscription: Subscription,
        label: Option<String>,
    ) -> Result<(String, Vec<DeviceInfo>), String>;
    /// Returns whether a device was removed, and every device left.
    fn unsubscribe(&self, endpoint: &str) -> Result<(bool, Vec<DeviceInfo>), String>;
    /// Sends the test notification to one device.
    fn test(&self, device_id: String) -> BoxFuture<Result<(), String>>;
    /// The device is showing this chat right now.
    fn note_viewing(&self, device_id: &str, session_id: &str);
}

/// `/w/<workspaceId>?session=<sessionId>`, the session id encoded like JavaScript's
/// `encodeURIComponent` (kept: `A-Z a-z 0-9 - _ . ! ~ * ' ( )`).
pub fn chat_route(workspace_id: &str, session_id: &str) -> String {
    let mut route = format!("/w/{workspace_id}?session=");
    for byte in session_id.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            route.push(char::from(byte));
        } else {
            route.push_str(&format!("%{byte:02X}"));
        }
    }
    route
}

/// The first 16 hex characters of SHA-256 of the endpoint.
pub fn device_id(endpoint: &str) -> String {
    let digest = Sha256::digest(endpoint.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
