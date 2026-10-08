//! The notifier: devices, keys, the ticker and the fan-out.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::task::JoinSet;

use super::sender::{PushRequest, PushResult, PushSender};
use super::watcher::{clip_exact, parked_message, turn_message, Due, TurnWatcher, Viewing};
use super::webpush::{encrypt, vapid_authorization, VapidKeys, MAX_PAYLOAD_BYTES};
use super::{device_id, DeviceInfo, NotifyService, PushConfig, PushMessage, Subscription};
use crate::db::DataVersion;
use crate::delivery::BoxFuture;
use crate::reads::states::{workspace_title, SessionStateRow};
use crate::reads::{ReadError, Reads};
use crate::state::store::{DeviceRow, NewDevice, ParkedRow, Store, StoreError};

/// How long a push service holds a notification for a phone that is offline: long enough to
/// survive a blip, short enough that "your agent finished" never surfaces the next morning.
pub const TTL_SECS: u32 = 3600;
/// Consecutive failures before a device is dropped. A `gone` answer drops it at once.
pub const MAX_FAILURES: u32 = 20;
/// A notification body is one glance on a lock screen.
pub const BODY_CHARS: usize = 180;
/// VAPID's `sub` claim when `PUSH_SUBJECT` names none.
pub const DEFAULT_SUBJECT: &str = "https://github.com/TemMax/conductor-remote";
/// The meta key of the VAPID private key: 64 lowercase hex characters.
const VAPID_KEY: &str = "vapid_private_key";
/// How many characters of a device label are kept.
const LABEL_CHARS: usize = 64;
const DEFAULT_LABEL: &str = "phone";
const DEFAULT_PARKED_TITLE: &str = "Conductor";
const NOT_SUBSCRIBED: &str = "this device is not subscribed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub subject: String,
    /// How often the ticker polls (2 s in production).
    pub tick: Duration,
    /// How long a viewing stamp counts (10 s in production).
    pub fresh: Duration,
}

impl NotifyConfig {
    /// `PUSH_NOTIFY` trimmed and lowercased in {off, false, 0} → disabled; `PUSH_SUBJECT` when
    /// non-empty replaces the subject.
    pub fn from_env() -> NotifyConfig {
        let notify = std::env::var("PUSH_NOTIFY").unwrap_or_default();
        let enabled = !matches!(notify.trim().to_lowercase().as_str(), "off" | "false" | "0");
        let subject = std::env::var("PUSH_SUBJECT")
            .ok()
            .map(|subject| subject.trim().to_owned())
            .filter(|subject| !subject.is_empty())
            .unwrap_or_else(|| DEFAULT_SUBJECT.to_owned());
        NotifyConfig {
            enabled,
            subject,
            tick: Duration::from_secs(2),
            fresh: Duration::from_secs(10),
        }
    }
}

/// The session states of the last read, and the `data_version` they were read at.
type CachedStates = (DataVersion, Vec<SessionStateRow>);

pub struct Notifier {
    /// Lets the trait's `&self` methods hand an owned notifier to a future.
    me: Weak<Notifier>,
    store: Arc<Store>,
    reads: Arc<Reads>,
    sender: Arc<dyn PushSender>,
    config: NotifyConfig,
    keys: VapidKeys,
    public_key: String,
    watcher: Mutex<TurnWatcher>,
    viewing: Mutex<Viewing>,
    states: Mutex<Option<CachedStates>>,
}

impl Notifier {
    /// Loads the VAPID key from the store, or makes one and stores it.
    pub fn new(
        store: Arc<Store>,
        reads: Arc<Reads>,
        sender: Arc<dyn PushSender>,
        config: NotifyConfig,
    ) -> Result<Arc<Notifier>, StoreError> {
        let keys = load_or_make_keys(&store)?;
        let public_key = keys.public_key();
        let viewing = Viewing::new(config.fresh);
        Ok(Arc::new_cyclic(|me| Notifier {
            me: me.clone(),
            store,
            reads,
            sender,
            config,
            keys,
            public_key,
            watcher: Mutex::new(TurnWatcher::new()),
            viewing: Mutex::new(viewing),
            states: Mutex::new(None),
        }))
    }

    /// When enabled, spawns the ticker: every `tick`, `tick_once` unless `running()` is false.
    pub fn start(self: &Arc<Self>, running: Arc<dyn Fn() -> bool + Send + Sync>) {
        if !self.config.enabled {
            tracing::info!("push notifications are off (PUSH_NOTIFY)");
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(me.config.tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if running() {
                    me.tick_once().await;
                }
            }
        });
    }

    /// One tick (public for tests). Reads nothing when no device is subscribed; reuses the last
    /// states while `data_version` is unchanged. Never waits for a push.
    pub async fn tick_once(self: &Arc<Self>) {
        let devices = match self.store.devices() {
            Ok(devices) => devices,
            Err(error) => {
                tracing::warn!("push tick skipped: {error}");
                return;
            }
        };
        if devices.is_empty() {
            lock(&self.watcher).reset();
            return;
        }
        let me = Arc::clone(self);
        let states = match tokio::task::spawn_blocking(move || me.read_states()).await {
            Ok(Ok(states)) => states,
            Ok(Err(error)) => {
                tracing::warn!("push tick skipped: {error}");
                return;
            }
            Err(error) => {
                tracing::warn!("push tick skipped: the read failed: {error}");
                return;
            }
        };
        let due = lock(&self.watcher).step(&states);
        for due in due {
            let me = Arc::clone(self);
            tokio::spawn(me.fire(due));
        }
    }

    /// Blocking: `data_version`, then the session states unless the cached ones are current.
    fn read_states(&self) -> Result<Vec<SessionStateRow>, ReadError> {
        let version = self.reads.db().data_version()?;
        if let Some((cached, states)) = lock(&self.states).as_ref() {
            if *cached == version {
                return Ok(states.clone());
            }
        }
        let states = self.reads.session_states()?;
        *lock(&self.states) = Some((version, states.clone()));
        Ok(states)
    }

    /// The notification of one confirmed turn, to every device not reading that chat.
    async fn fire(self: Arc<Self>, due: Due) {
        let reads = Arc::clone(&self.reads);
        let session_id = due.state.session_id.clone();
        let read = tokio::task::spawn_blocking(move || reads.last_assistant_text(&session_id));
        let said = match read.await {
            Ok(Ok(said)) => said,
            Ok(Err(error)) => {
                tracing::warn!("push: could not read the last answer: {error}");
                None
            }
            Err(error) => {
                tracing::warn!("push: could not read the last answer: {error}");
                None
            }
        };
        let message = turn_message(&due, said.as_deref(), now_ms());
        let kind = message.kind.clone();
        let sent = self
            .notify_all(message, Some(due.state.session_id.clone()))
            .await;
        if sent > 0 {
            tracing::info!("push: {kind} → {sent} device(s)");
        }
    }

    /// Sends to every device not reading `unless_reading`; returns how many succeeded.
    pub async fn notify_all(
        self: &Arc<Self>,
        message: PushMessage,
        unless_reading: Option<String>,
    ) -> usize {
        let devices = match self.store.devices() {
            Ok(devices) => devices,
            Err(error) => {
                tracing::warn!("push: could not list the devices: {error}");
                return 0;
            }
        };
        let total = devices.len();
        let targets: Vec<DeviceRow> = match unless_reading.as_deref() {
            Some(session_id) => {
                let now = Instant::now();
                let viewing = lock(&self.viewing);
                devices
                    .into_iter()
                    .filter(|device| !viewing.is_reading(&device.id, session_id, now))
                    .collect()
            }
            None => devices,
        };
        let held = total - targets.len();
        if held > 0 {
            tracing::info!("push: held back from {held} device(s) reading that chat");
        }
        if targets.is_empty() {
            return 0;
        }

        let message = Arc::new(message);
        let mut pushes = JoinSet::new();
        for device in targets {
            let me = Arc::clone(self);
            let message = Arc::clone(&message);
            pushes.spawn_blocking(move || {
                let result = me.push(&device, &message);
                (device, result)
            });
        }
        let mut results = Vec::new();
        while let Some(joined) = pushes.join_next().await {
            match joined {
                Ok(pair) => results.push(pair),
                Err(error) => tracing::warn!("push: a delivery task failed: {error}"),
            }
        }
        results
            .iter()
            .filter(|(device, result)| self.settle(device, result).is_ok())
            .count()
    }

    /// The parked-prompt notice: title from `write_workspace` → `workspace_title`, else
    /// "Conductor"; spawns `notify_all(parked_message(..), None)`.
    pub fn notify_parked(self: &Arc<Self>, row: &ParkedRow, error: Option<&str>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("push: no runtime for the parked-prompt notice");
            return;
        };
        let me = Arc::clone(self);
        let row = row.clone();
        let error = error.map(str::to_owned);
        runtime.spawn(async move {
            let title = me.parked_title(&row).await;
            let message = parked_message(
                &title,
                &row.workspace_id,
                &row.session_id,
                &row.text,
                error.as_deref(),
                now_ms(),
            );
            me.notify_all(message, None).await;
        });
    }

    /// The workspace's title, read on a blocking thread; "Conductor" when it cannot be read.
    async fn parked_title(&self, row: &ParkedRow) -> String {
        let reads = Arc::clone(&self.reads);
        let (workspace_id, session_id) = (row.workspace_id.clone(), row.session_id.clone());
        let lookup = tokio::task::spawn_blocking(move || {
            reads.write_workspace(Some(&workspace_id), Some(&session_id))
        });
        match lookup.await {
            Ok(Ok(Some(workspace))) => workspace_title(
                workspace.workspace_name.as_deref(),
                None,
                workspace.branch.as_deref(),
                workspace.directory_name.as_deref(),
                &workspace.id,
            ),
            Ok(Ok(None)) => DEFAULT_PARKED_TITLE.to_owned(),
            Ok(Err(error)) => {
                tracing::warn!("push: could not read the parked prompt's workspace: {error}");
                DEFAULT_PARKED_TITLE.to_owned()
            }
            Err(error) => {
                tracing::warn!("push: could not read the parked prompt's workspace: {error}");
                DEFAULT_PARKED_TITLE.to_owned()
            }
        }
    }

    /// Blocking: encrypts the message for one device and sends it.
    fn push(&self, device: &DeviceRow, message: &PushMessage) -> PushResult {
        let body = match encrypt(&device.p256dh, &device.auth, &payload(message)) {
            Ok(body) => body,
            Err(error) => return failure(error.to_string()),
        };
        let authorization = match vapid_authorization(
            &device.endpoint,
            &self.keys,
            &self.config.subject,
            now_secs(),
        ) {
            Ok(authorization) => authorization,
            Err(error) => return failure(error.to_string()),
        };
        self.sender.send(&PushRequest {
            endpoint: device.endpoint.clone(),
            authorization,
            body,
            ttl_secs: TTL_SECS,
        })
    }

    /// Folds one push's outcome into the device's row; `Err` carries the error text.
    fn settle(&self, device: &DeviceRow, result: &PushResult) -> Result<(), String> {
        if result.ok {
            if let Err(error) = self.store.record_device_ok(&device.id, now_ms()) {
                tracing::warn!("push: could not record a delivery: {error}");
            }
            return Ok(());
        }
        let error = result
            .error
            .clone()
            .unwrap_or_else(|| format!("HTTP {}", result.status));
        // Never the endpoint in a log: it is a capability URL.
        if result.gone {
            self.drop_device(&device.id);
            tracing::info!("push: dropped a subscription the push service no longer has ({error})");
            return Err(error);
        }
        match self.store.record_device_failure(&device.id, &error) {
            Ok(failures) => {
                tracing::warn!("push: delivery failed ({error}), {failures} in a row");
                if failures >= MAX_FAILURES {
                    self.drop_device(&device.id);
                    tracing::info!("push: dropped a device after {failures} failures in a row");
                }
            }
            Err(store_error) => {
                tracing::warn!("push: delivery failed ({error}); not recorded: {store_error}");
            }
        }
        Err(error)
    }

    fn drop_device(&self, id: &str) {
        if let Err(error) = self.store.remove_device(id) {
            tracing::warn!("push: could not remove a device: {error}");
        }
        lock(&self.viewing).forget(id);
    }

    fn device_infos(&self) -> Result<Vec<DeviceInfo>, String> {
        let devices = self.store.devices().map_err(|error| error.to_string())?;
        Ok(devices.iter().map(device_info).collect())
    }
}

impl NotifyService for Notifier {
    fn config(&self) -> Result<PushConfig, String> {
        Ok(PushConfig {
            enabled: self.config.enabled,
            public_key: self.public_key.clone(),
            devices: self.device_infos()?,
        })
    }

    fn subscribe(
        &self,
        subscription: Subscription,
        label: Option<String>,
    ) -> Result<(String, Vec<DeviceInfo>), String> {
        let id = device_id(&subscription.endpoint);
        let label: String = label
            .as_deref()
            .unwrap_or("")
            .trim()
            .chars()
            .take(LABEL_CHARS)
            .collect();
        // An empty label keeps the label a known device has; a new device is a "phone".
        let known = self
            .store
            .device(&id)
            .map_err(|error| error.to_string())?
            .is_some();
        let label = if label.is_empty() && !known {
            DEFAULT_LABEL.to_owned()
        } else {
            label
        };
        self.store
            .upsert_device(&NewDevice {
                id: id.clone(),
                endpoint: subscription.endpoint,
                p256dh: subscription.p256dh,
                auth: subscription.auth,
                label,
                created_at_ms: now_ms(),
            })
            .map_err(|error| error.to_string())?;
        if !known {
            tracing::info!("push: a device subscribed");
        }
        Ok((id, self.device_infos()?))
    }

    fn unsubscribe(&self, endpoint: &str) -> Result<(bool, Vec<DeviceInfo>), String> {
        let removed = self
            .store
            .remove_device_by_endpoint(endpoint)
            .map_err(|error| error.to_string())?;
        if removed {
            lock(&self.viewing).forget(&device_id(endpoint));
            tracing::info!("push: a device unsubscribed");
        }
        Ok((removed, self.device_infos()?))
    }

    fn test(&self, device_id: String) -> BoxFuture<Result<(), String>> {
        let me = self.me.upgrade();
        Box::pin(async move {
            let Some(me) = me else {
                return Err("the notifier has stopped".to_owned());
            };
            let device = match me.store.device(&device_id) {
                Ok(Some(device)) => device,
                Ok(None) => return Err(NOT_SUBSCRIBED.to_owned()),
                Err(error) => return Err(error.to_string()),
            };
            let message = PushMessage {
                title: "Conductor Remote".to_owned(),
                body: "Notifications are working. You’ll get one when an agent finishes."
                    .to_owned(),
                tag: "test".to_owned(),
                url: "/".to_owned(),
                kind: "test".to_owned(),
                ts: now_ms(),
            };
            let pusher = Arc::clone(&me);
            let pushed = tokio::task::spawn_blocking(move || {
                let result = pusher.push(&device, &message);
                (device, result)
            })
            .await;
            match pushed {
                Ok((device, result)) => me.settle(&device, &result),
                Err(error) => Err(format!("the push failed: {error}")),
            }
        })
    }

    fn note_viewing(&self, device_id: &str, session_id: &str) {
        lock(&self.viewing).note(device_id, session_id, Instant::now());
    }
}

/// The JSON of the message; when it is over `MAX_PAYLOAD_BYTES`, the body is shortened with
/// `clip_exact` until it fits, then (the title is user text and may be long) the title the same
/// way, keeping at least one character. The tag and url stay intact.
fn payload(message: &PushMessage) -> Vec<u8> {
    let mut json = to_json(message);
    let mut clipped = message.clone();
    let mut room = message.body.chars().count();
    while json.len() > MAX_PAYLOAD_BYTES && room > 0 {
        let over = json.len() - MAX_PAYLOAD_BYTES;
        room = room.saturating_sub(over);
        clipped.body = clip_exact(&message.body, room);
        json = to_json(&clipped);
    }
    let mut room = message.title.chars().count();
    while json.len() > MAX_PAYLOAD_BYTES && room > 1 {
        let over = json.len() - MAX_PAYLOAD_BYTES;
        room = room.saturating_sub(over).max(1);
        clipped.title = clip_exact(&message.title, room);
        json = to_json(&clipped);
    }
    json
}

fn to_json(message: &PushMessage) -> Vec<u8> {
    // Strings and an integer: serializing cannot fail.
    serde_json::to_vec(message).unwrap_or_default()
}

fn failure(error: String) -> PushResult {
    PushResult {
        ok: false,
        status: 0,
        error: Some(error),
        gone: false,
    }
}

fn device_info(device: &DeviceRow) -> DeviceInfo {
    DeviceInfo {
        id: device.id.clone(),
        label: device.label.clone(),
        created_at: device.created_at_ms,
        last_ok_at: device.last_ok_at_ms,
        last_error: device.last_error.clone(),
        failures: device.failures,
    }
}

fn load_or_make_keys(store: &Store) -> Result<VapidKeys, StoreError> {
    if let Some(stored) = store.meta(VAPID_KEY)? {
        match hex_decode(&stored).and_then(|bytes| VapidKeys::from_bytes(&bytes).ok()) {
            Some(keys) => return Ok(keys),
            // A new key invalidates every subscription, but an unreadable one serves none.
            None => tracing::warn!("push: the stored VAPID key is unreadable; making a new one"),
        }
    }
    let keys = VapidKeys::generate();
    store.set_meta(VAPID_KEY, &hex_encode(&keys.to_bytes()))?;
    Ok(keys)
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// 32 bytes from exactly 64 hex characters, else `None`.
fn hex_decode(text: &str) -> Option<Vec<u8>> {
    fn nibble(digit: u8) -> Option<u8> {
        match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            b'A'..=b'F' => Some(digit - b'A' + 10),
            _ => None,
        }
    }
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    digits
        .chunks_exact(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // No guarded state is left half-written by a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
