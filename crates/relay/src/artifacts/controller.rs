//! Memory-only sessions and shared authorization leases over durable, identity-bound origins.
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::Uri;
use base64::prelude::*;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::files::{prepare_document, ArtifactFilePolicy, PreparedArtifact, DOCUMENT_MAX_BYTES};
use super::store::{ArtifactStore, OriginBinding};
use super::*;

pub(crate) const IDLE: Duration = Duration::from_secs(30 * 60);
pub(crate) const GRANT_LIFE: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct ArtifactPreviews {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub config: ArtifactPreviewConfig,
    pub policy: Arc<ArtifactFilePolicy>,
    pub store: ArtifactStore,
    pub state: Mutex<State>,
}

#[derive(Default)]
pub(crate) struct State {
    ready: bool,
    pub sessions: HashMap<String, Session>,
    pub leases: HashMap<String, Lease>,
    cleanup: Vec<ArtifactExposureReceipt>,
}

pub(crate) struct Session {
    pub binding_id: String,
    pub parent: String,
    pub grant: Option<String>,
    pub created: Duration,
    pub touched: Duration,
    pub authorized: bool,
    pub failures: VecDeque<serde_json::Value>,
}

pub(crate) struct Lease {
    pub binding: OriginBinding,
    pub document: Arc<PreparedArtifact>,
    pub cookie: String,
    pub receipt: Option<ArtifactExposureReceipt>,
    task: JoinHandle<()>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) fn unavailable() -> ArtifactError {
    ArtifactError::Unavailable("artifact preview is unavailable".into())
}

pub(crate) fn denied() -> ArtifactError {
    ArtifactError::Forbidden("artifact authorization is missing or expired".into())
}

pub(crate) fn exact_origin(value: &str) -> Result<Uri, ArtifactError> {
    let uri: Uri = value
        .parse()
        .map_err(|_| ArtifactError::Invalid("invalid parent origin".into()))?;
    let scheme = uri.scheme_str().unwrap_or_default();
    let authority = uri
        .authority()
        .ok_or_else(|| ArtifactError::Invalid("invalid parent origin".into()))?;
    if !matches!(scheme, "http" | "https")
        || authority.as_str().contains('@')
        || uri.host().is_none_or(str::is_empty)
        || authority.port_u16() == Some(0)
        || authority.port_u16().is_some_and(|port| {
            (scheme == "http" && port == 80)
                || (scheme == "https" && port == 443)
                || authority.as_str() != format!("{}:{port}", uri.host().unwrap_or_default())
        })
        || format!("{scheme}://{authority}") != value
        || uri
            .host()
            .is_some_and(|host| host != host.to_ascii_lowercase())
        || (scheme == "http" && !matches!(uri.host(), Some("localhost" | "127.0.0.1")))
    {
        return Err(ArtifactError::Invalid("invalid parent origin".into()));
    }
    Ok(uri)
}

impl ArtifactPreviews {
    pub fn new(config: ArtifactPreviewConfig) -> Result<Self, ArtifactError> {
        if config.allowed_parent_origins.is_empty() {
            return Err(ArtifactError::Invalid(
                "preview parent origins are required".into(),
            ));
        }
        for origin in &config.allowed_parent_origins {
            exact_origin(origin)?;
        }
        let policy = Arc::new(ArtifactFilePolicy::new(
            &config.source_roots,
            &config.broad_roots,
        )?);
        let store = ArtifactStore::open(&config.state_dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                policy,
                store,
                state: Mutex::new(State::default()),
            }),
        })
    }

    pub async fn reconcile_startup(&self) -> Result<(), ArtifactError> {
        let mut state = self.inner.state.lock().await;
        if state.ready {
            return Ok(());
        }
        if let Some(exposure) = &self.inner.config.exposure {
            exposure.reconcile_startup().await?;
        } else {
            let store = self.inner.store.clone();
            let snapshot = blocking(move || store.snapshot()).await?;
            if !snapshot.receipts.is_empty() {
                return Err(ArtifactError::Unavailable(
                    "artifact exposure cleanup requires an adapter".into(),
                ));
            }
        }
        let store = self.inner.store.clone();
        blocking(move || {
            let mut guard = store.lock()?;
            for binding in guard.snapshot()?.bindings {
                guard.tombstone_binding(&binding.binding_id)?;
            }
            Ok(())
        })
        .await?;
        state.ready = true;
        Ok(())
    }

    pub async fn sweep(&self) -> Result<(), ArtifactError> {
        let mut state = self.inner.state.lock().await;
        self.inner.prune(&mut state).await
    }

    pub async fn close_all(&self) -> Result<(), ArtifactError> {
        let mut state = self.inner.state.lock().await;
        state.sessions.clear();
        self.inner.prune(&mut state).await
    }
}

impl ArtifactPreviewService for ArtifactPreviews {
    fn create(
        &self,
        request: CreateArtifactPreviewRequest,
    ) -> crate::delivery::BoxFuture<Result<ArtifactPreviewSession, ArtifactError>> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.create(request).await })
    }
    fn source(
        &self,
        id: String,
    ) -> crate::delivery::BoxFuture<Result<ArtifactPreviewSource, ArtifactError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let document = {
                let mut state = inner.state.lock().await;
                let _ = inner.prune(&mut state).await;
                let session = state
                    .sessions
                    .get_mut(&id)
                    .ok_or_else(|| ArtifactError::Missing("artifact session is missing".into()))?;
                session.touched = inner.config.clock.monotonic();
                let binding_id = session.binding_id.clone();
                state
                    .leases
                    .get(&binding_id)
                    .ok_or_else(unavailable)?
                    .document
                    .clone()
            };
            original_source(&document).await
        })
    }
    fn close(
        &self,
        id: String,
    ) -> crate::delivery::BoxFuture<Result<CloseArtifactPreviewResult, ArtifactError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let mut state = inner.state.lock().await;
            state.sessions.remove(&id);
            inner.prune(&mut state).await?;
            Ok(CloseArtifactPreviewResult { ok: true })
        })
    }
}

impl ArtifactPreviewLifecycle for ArtifactPreviews {
    fn reconcile_startup(&self) -> crate::delivery::BoxFuture<Result<(), ArtifactError>> {
        let this = self.clone();
        Box::pin(async move { this.reconcile_startup().await })
    }
    fn sweep(&self) -> crate::delivery::BoxFuture<Result<(), ArtifactError>> {
        let this = self.clone();
        Box::pin(async move { this.sweep().await })
    }
    fn close_all(&self) -> crate::delivery::BoxFuture<Result<(), ArtifactError>> {
        let this = self.clone();
        Box::pin(async move { this.close_all().await })
    }
}

impl Inner {
    pub fn secret(&self) -> Result<String, ArtifactError> {
        let mut bytes = [0u8; 32];
        self.config.entropy.fill(&mut bytes)?;
        Ok(BASE64_URL_SAFE_NO_PAD.encode(bytes))
    }

    async fn create(
        self: &Arc<Self>,
        request: CreateArtifactPreviewRequest,
    ) -> Result<ArtifactPreviewSession, ArtifactError> {
        let parent = match &request {
            CreateArtifactPreviewRequest::File { parent_origin, .. }
            | CreateArtifactPreviewRequest::Html { parent_origin, .. } => parent_origin.clone(),
        };
        let uri = exact_origin(&parent)?;
        if !self.config.allowed_parent_origins.contains(&parent) {
            return Err(ArtifactError::Forbidden(
                "parent origin is not permitted".into(),
            ));
        }
        let remote = uri.scheme_str() == Some("https");
        let host = uri.host().ok_or_else(unavailable)?.to_owned();
        let document = Arc::new(prepare_document(self.policy.clone(), request, None).await?);
        let mut state = self.state.lock().await;
        if !state.ready {
            return Err(ArtifactError::Unavailable(
                "artifact startup reconciliation is required".into(),
            ));
        }
        let maintenance = self.prune(&mut state).await;
        if remote {
            maintenance?;
        }
        if state.sessions.len() >= 16 {
            return Err(ArtifactError::Unavailable(
                "too many artifact sessions".into(),
            ));
        }
        let permission = if remote {
            let exposure = self.config.exposure.as_ref().ok_or_else(unavailable)?;
            let permission = exposure.current_permission().await?;
            if !permission.enabled {
                return Err(denied());
            }
            Some(permission)
        } else {
            None
        };
        let id = self.secret()?;
        if state.sessions.contains_key(&id) {
            return Err(unavailable());
        }
        let grant = self.secret()?;
        let expires_at = timestamp(
            self.config
                .clock
                .now()
                .checked_add(IDLE)
                .ok_or_else(unavailable)?,
        )?;
        let existing = state
            .leases
            .iter()
            .find(|(_, lease)| {
                lease.document.identity == document.identity && {
                    let bound: Uri = lease.binding.origin.parse().expect("validated binding");
                    bound.host() == Some(host.as_str()) && bound.scheme_str() == uri.scheme_str()
                }
            })
            .map(|(id, _)| id.clone());
        let binding_id = match existing {
            Some(id) => id,
            None => {
                let lease = self
                    .open_lease(document.clone(), &host, permission, &mut state)
                    .await?;
                let id = lease.binding.binding_id.clone();
                state.leases.insert(id.clone(), lease);
                id
            }
        };
        let now = self.config.clock.monotonic();
        let lease = state.leases.get(&binding_id).ok_or_else(unavailable)?;
        let response = ArtifactPreviewSession {
            id: id.clone(),
            origin: lease.binding.origin.clone(),
            bootstrap_url: format!(
                "{}/__conductor_preview/bootstrap?session={id}",
                lease.binding.origin
            ),
            entry_path: document.entry_path.clone(),
            grant: grant.clone(),
            expires_at,
            path: document.source.path.clone(),
        };
        state.sessions.insert(
            id,
            Session {
                binding_id,
                parent,
                grant: Some(grant),
                created: now,
                touched: now,
                authorized: false,
                failures: VecDeque::new(),
            },
        );
        Ok(response)
    }

    async fn open_lease(
        self: &Arc<Self>,
        document: Arc<PreparedArtifact>,
        host: &str,
        permission: Option<ArtifactExposurePermission>,
        state: &mut State,
    ) -> Result<Lease, ArtifactError> {
        let remote = permission.is_some();
        let mut excluded = self.config.allowed_parent_origins.clone();
        for attempt in 0..10 {
            let cookie = self.secret()?;
            let cookie_name = format!(
                "conductor_preview_{}",
                super::files::digest(&[self.secret()?.as_bytes()])
            );
            let store = self.store.clone();
            let identity = document.identity.clone();
            let host = host.to_owned();
            let exclusions = excluded.clone();
            let now = self.config.clock.now();
            let (binding, listener) = blocking(move || {
                allocate(
                    &store,
                    &identity,
                    &host,
                    remote,
                    &exclusions,
                    &cookie_name,
                    now,
                )
            })
            .await?;
            let listener = TcpListener::from_std(listener).map_err(|_| unavailable())?;
            let port = listener.local_addr().map_err(|_| unavailable())?.port();
            let receipt = permission.map(|permission| ArtifactExposureReceipt {
                binding_id: binding.binding_id.clone(),
                loopback_port: port,
                https_port: binding
                    .origin
                    .parse::<Uri>()
                    .expect("validated origin")
                    .port_u16()
                    .expect("allocated port"),
                origin: binding.origin.clone(),
                generation: permission.generation,
            });
            if let Some(receipt) = &receipt {
                let exposure = self.config.exposure.as_ref().ok_or_else(unavailable)?;
                // The adapter owns its lock, pending receipt and permission recheck. Allocation
                // races leave this origin reserved to the same identity; never overwrite a peer.
                if let Err(error) = exposure.publish(receipt.clone()).await {
                    drop(listener);
                    if let Err(cleanup_error) = exposure.unpublish(receipt.clone()).await {
                        state.cleanup.push(receipt.clone());
                        return Err(cleanup_error);
                    }
                    let store = self.store.clone();
                    let binding_id = binding.binding_id.clone();
                    blocking(move || store.lock()?.tombstone_binding(&binding_id)).await?;
                    if matches!(error, ArtifactError::Unavailable(_)) && attempt < 9 {
                        excluded.insert(binding.origin);
                        continue;
                    }
                    return Err(error);
                }
            }
            let task =
                super::server::start(listener, Arc::downgrade(self), binding.binding_id.clone());
            return Ok(Lease {
                binding,
                document,
                cookie,
                receipt,
                task,
            });
        }
        Err(unavailable())
    }

    pub async fn prune(&self, state: &mut State) -> Result<(), ArtifactError> {
        let now = self.config.clock.monotonic();
        state
            .sessions
            .retain(|_, session| now.saturating_sub(session.touched) < IDLE);
        let mut failure = None;
        if state.leases.values().any(|lease| lease.receipt.is_some()) {
            let permission = match &self.config.exposure {
                Some(exposure) => exposure.current_permission().await,
                None => Err(unavailable()),
            };
            let revoked: BTreeSet<_> = state
                .leases
                .iter()
                .filter(|(_, lease)| {
                    lease.receipt.as_ref().is_some_and(|receipt| {
                        permission
                            .as_ref()
                            .map_or(true, |p| !p.enabled || p.generation != receipt.generation)
                    })
                })
                .map(|(id, _)| id.clone())
                .collect();
            state
                .sessions
                .retain(|_, session| !revoked.contains(&session.binding_id));
            if let Err(error) = permission {
                failure = Some(error);
            }
        }
        let dead: Vec<_> = state
            .leases
            .keys()
            .filter(|id| !state.sessions.values().any(|s| &s.binding_id == *id))
            .cloned()
            .collect();
        for id in dead {
            if let Some(mut lease) = state.leases.remove(&id) {
                lease.task.abort();
                let _ = (&mut lease.task).await;
                if let Some(receipt) = lease.receipt.take() {
                    state.cleanup.push(receipt);
                }
                let store = self.store.clone();
                if let Err(error) = blocking(move || store.lock()?.tombstone_binding(&id)).await {
                    failure = Some(error);
                }
            }
        }
        let mut pending = Vec::new();
        for receipt in state.cleanup.drain(..) {
            let result = match &self.config.exposure {
                Some(exposure) => exposure.unpublish(receipt.clone()).await,
                None => Err(unavailable()),
            };
            if let Err(error) = result {
                pending.push(receipt);
                failure = Some(error);
            }
        }
        state.cleanup = pending;
        failure.map_or(Ok(()), Err)
    }
}

pub(crate) async fn original_source(
    document: &PreparedArtifact,
) -> Result<ArtifactPreviewSource, ArtifactError> {
    if document.source.path.is_none() {
        return Ok(document.source.clone());
    }
    let resource = document
        .root
        .clone()
        .read_resource(document.entry_path.clone(), None, false)
        .await?;
    if resource.body.len() as u64 > DOCUMENT_MAX_BYTES {
        return Err(ArtifactError::TooLarge("HTML exceeds 8 MiB".into()));
    }
    let content = String::from_utf8(resource.body)
        .map_err(|_| ArtifactError::Unsupported("HTML source must be UTF-8".into()))?;
    Ok(ArtifactPreviewSource {
        path: document.source.path.clone(),
        content,
    })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ArtifactError> + Send + 'static,
) -> Result<T, ArtifactError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| unavailable())?
}

fn allocate(
    store: &ArtifactStore,
    identity: &str,
    host: &str,
    remote: bool,
    excluded: &BTreeSet<String>,
    cookie: &str,
    now: SystemTime,
) -> Result<(OriginBinding, std::net::TcpListener), ArtifactError> {
    let mut guard = store.lock()?;
    let snapshot = guard.snapshot()?;
    let scheme = if remote { "https" } else { "http" };
    let previous = snapshot.bindings.iter().find(|b| {
        b.identity == identity
            && !excluded.contains(&b.origin)
            && b.origin
                .parse::<Uri>()
                .is_ok_and(|u| u.host() == Some(host) && u.scheme_str() == Some(scheme))
    });
    let main_ports: BTreeSet<_> = excluded
        .iter()
        .filter_map(|s| {
            s.parse::<Uri>().ok().map(|u| {
                u.port_u16().unwrap_or(if u.scheme_str() == Some("https") {
                    443
                } else {
                    80
                })
            })
        })
        .collect();
    let used: BTreeSet<_> = snapshot
        .reserved_origins()
        .chain(excluded.iter().map(String::as_str))
        .filter_map(|s| {
            s.parse::<Uri>().ok().map(|u| {
                u.port_u16().unwrap_or(if u.scheme_str() == Some("https") {
                    443
                } else {
                    80
                })
            })
        })
        .collect();
    let ports: Vec<u16> = if let Some(binding) = previous {
        vec![binding
            .origin
            .parse::<Uri>()
            .map_err(|_| unavailable())?
            .port_u16()
            .ok_or_else(unavailable)?]
    } else if remote {
        (30000..=39999).filter(|p| !used.contains(p)).collect()
    } else {
        (40000..=65535).filter(|p| !used.contains(p)).collect()
    };
    for port in ports {
        if main_ports.contains(&port) {
            continue;
        }
        let listener =
            match std::net::TcpListener::bind(("127.0.0.1", if remote { 0 } else { port })) {
                Ok(listener) => listener,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(_) => return Err(unavailable()),
            };
        let local_port = listener.local_addr().map_err(|_| unavailable())?.port();
        if remote && (used.contains(&local_port) || local_port == port) {
            continue;
        }
        listener.set_nonblocking(true).map_err(|_| unavailable())?;
        let origin = format!("{scheme}://{host}:{port}");
        let binding = guard.reserve_origin(identity, &origin, cookie, now)?;
        return Ok((binding, listener));
    }
    Err(ArtifactError::Unavailable(
        "artifact origin capacity is exhausted or reserved port is busy".into(),
    ))
}

fn timestamp(time: SystemTime) -> Result<String, ArtifactError> {
    let seconds: libc::time_t = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| unavailable())?
        .as_secs()
        .try_into()
        .map_err(|_| unavailable())?;
    let mut utc = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers reference appropriately sized, live storage; gmtime_r initializes tm.
    if unsafe { libc::gmtime_r(&seconds, utc.as_mut_ptr()) }.is_null() {
        return Err(unavailable());
    }
    // SAFETY: successful gmtime_r initialized utc.
    let utc = unsafe { utc.assume_init() };
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        utc.tm_year + 1900,
        utc.tm_mon + 1,
        utc.tm_mday,
        utc.tm_hour,
        utc.tm_min,
        utc.tm_sec
    ))
}

pub(crate) type Runtime = Weak<Inner>;
