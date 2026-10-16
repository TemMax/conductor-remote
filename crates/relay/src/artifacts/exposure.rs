//! Owned Tailscale Serve publication. No project commands or main-auth credentials.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::delivery::BoxFuture;
use crate::dev::tailscale::{parse_serve_status, ServeStatus, Tailscale};
use crate::reads::extras::commands::{Commands, Limits};
use crate::service::{CommandOutput, CommandRunner};

use super::store::{ArtifactStore, LifecycleGuard};
use super::{
    ArtifactError, ArtifactExposure, ArtifactExposurePermission, ArtifactExposureReceipt,
    ArtifactParentOrigins,
};

fn unavailable(message: &str) -> ArtifactError {
    ArtifactError::Unavailable(message.into())
}

/// Exact supported main origins, from listener configuration and an observed main HTTPS mapping.
/// The remote origin must come from Tailscale status, never an HTTP request header.
pub fn parent_origins(
    relay_port: u16,
    web_port: u16,
    remote_origin: Option<&str>,
) -> Result<BTreeSet<String>, ArtifactError> {
    if relay_port == 0 || web_port == 0 {
        return Err(ArtifactError::Invalid(
            "main listener port is required".into(),
        ));
    }
    let mut origins = BTreeSet::new();
    for host in ["localhost", "127.0.0.1"] {
        for port in [relay_port, web_port] {
            origins.insert(if port == 80 {
                format!("http://{host}")
            } else {
                format!("http://{host}:{port}")
            });
        }
    }
    if let Some(origin) = remote_origin {
        let uri = super::controller::exact_origin(origin)?;
        if uri.scheme_str() != Some("https") {
            return Err(ArtifactError::Invalid(
                "tailnet origin must use HTTPS".into(),
            ));
        }
        origins.insert(origin.to_owned());
    }
    Ok(origins)
}

/// Read-only discovery of the currently configured main HTTPS mappings.
#[derive(Clone)]
pub struct TailscaleArtifactParentOrigins {
    relay_port: u16,
    tailscale: Option<PathBuf>,
    commands: Arc<dyn Commands>,
}

impl TailscaleArtifactParentOrigins {
    pub fn new(relay_port: u16, tailscale: Option<PathBuf>, commands: Arc<dyn Commands>) -> Self {
        Self {
            relay_port,
            tailscale,
            commands,
        }
    }

    fn read(&self) -> Result<BTreeSet<String>, ArtifactError> {
        if self.relay_port == 0 {
            return Err(ArtifactError::Invalid(
                "main listener port is required".into(),
            ));
        }
        let bin = self
            .tailscale
            .as_ref()
            .ok_or_else(|| unavailable("tailscale is unavailable"))?;
        let host = Tailscale::new(bin.clone(), self.commands.clone())
            .host()
            .ok_or_else(|| unavailable("main tailnet hostname could not be read"))?;
        let base = format!("https://{host}");
        let uri = super::controller::exact_origin(&base)?;
        if uri.host() != Some(host.as_str()) || uri.port_u16().is_some() {
            return Err(unavailable("main tailnet hostname is invalid"));
        }
        let output = self
            .commands
            .run(
                &bin.to_string_lossy(),
                &["serve", "status", "--json"],
                None,
                Limits {
                    timeout: Duration::from_secs(15),
                    max_stdout: 1024 * 1024,
                },
            )
            .map_err(|_| unavailable("main serve status could not be read"))?;
        if output.code != Some(0) {
            return Err(unavailable("main serve status could not be read"));
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| unavailable("main serve status could not be read"))?;
        if !value.is_object() && !value.is_null() {
            return Err(unavailable("main serve status could not be read"));
        }
        let target = format!("http://127.0.0.1:{}", self.relay_port);
        let mut origins = BTreeSet::new();
        for port in std::iter::once(443).chain(8443..=8450) {
            // The general ServeStatus helper reduces Web entries to ports. Admission also
            // needs the exact current Self hostname, so inspect that entry before trusting it.
            let key = format!("{host}:{port}");
            if value["TCP"][port.to_string()]["HTTPS"] == true
                && value["Web"][&key]["Handlers"]["/"]["Proxy"].as_str() == Some(target.as_str())
            {
                let origin = if port == 443 {
                    base.clone()
                } else {
                    format!("{base}:{port}")
                };
                super::controller::exact_origin(&origin)?;
                origins.insert(origin);
            }
        }
        Ok(origins)
    }
}

impl ArtifactParentOrigins for TailscaleArtifactParentOrigins {
    fn current(&self) -> BoxFuture<Result<BTreeSet<String>, ArtifactError>> {
        let this = self.clone();
        blocking(move || this.read())
    }
}

/// Effective settings are resolved on every request and once more under the publication lock.
type PermissionReader = Arc<dyn Fn() -> Result<bool, ArtifactError> + Send + Sync>;

#[derive(Clone)]
pub struct TailscaleArtifactExposure {
    store: ArtifactStore,
    tailscale: Option<PathBuf>,
    commands: Arc<dyn Commands>,
    configured_enabled: PermissionReader,
}

impl TailscaleArtifactExposure {
    /// `state_dir` is the relay state root; preview metadata has its own `artifacts` directory.
    pub fn new(
        state_dir: &Path,
        tailscale: Option<PathBuf>,
        commands: Arc<dyn Commands>,
        configured_enabled: PermissionReader,
    ) -> Result<Self, ArtifactError> {
        Ok(Self {
            store: ArtifactStore::open(&state_dir.join("artifacts"))?,
            tailscale,
            commands,
            configured_enabled,
        })
    }

    fn publish_locked(&self, receipt: ArtifactExposureReceipt) -> Result<(), ArtifactError> {
        let mut guard = self.store.lock()?;
        let enabled = (self.configured_enabled)()?;
        // Permission and durable generation are checked before any external publication.
        let permission = guard.snapshot()?.permission(enabled);
        if !permission.enabled || permission.generation != receipt.generation {
            return Err(ArtifactError::Forbidden(
                "remote artifact permission was revoked".into(),
            ));
        }
        let bin = self
            .tailscale
            .as_ref()
            .ok_or_else(|| unavailable("tailscale is unavailable"))?;
        let tailscale = Tailscale::new(bin.clone(), self.commands.clone());
        let status = tailscale
            .serve_status()
            .map_err(|_| unavailable("preview serve status could not be read"))?;
        let stored = guard
            .snapshot()?
            .receipts
            .into_iter()
            .any(|stored| stored.receipt == receipt);
        if occupied(&status, receipt.https_port) {
            if stored && exact_target(&status, &receipt) {
                return guard.mark_active(&receipt);
            }
            return Err(unavailable("preview HTTPS port is already occupied"));
        }
        guard.begin_publication(receipt.clone(), enabled)?;
        // Re-read immediately before the mutating command. Other preview publishers and the
        // CLI are serialized by flock; dev/main have disjoint ranges. A competing external
        // mapping observed here is preserved and the runtime retries another permanent origin.
        let status = tailscale
            .serve_status()
            .map_err(|_| unavailable("preview serve status could not be read"))?;
        if occupied(&status, receipt.https_port) {
            guard.finish_cleanup(&receipt)?;
            return Err(unavailable("preview HTTPS allocation changed"));
        }
        tailscale
            .serve(receipt.https_port, receipt.loopback_port)
            .map_err(|_| unavailable("preview mapping could not be made"))?;
        let status = tailscale
            .serve_status()
            .map_err(|_| unavailable("preview mapping could not be confirmed"))?;
        if !exact_target(&status, &receipt) {
            // Keep pending metadata: failed/ambiguous commands are recovered using exact target
            // checks, never by deleting whatever happens to occupy this port.
            return Err(unavailable("preview mapping could not be confirmed"));
        }
        guard.mark_active(&receipt)
    }
}

impl ArtifactExposure for TailscaleArtifactExposure {
    fn current_permission(&self) -> BoxFuture<Result<ArtifactExposurePermission, ArtifactError>> {
        let this = self.clone();
        blocking(move || {
            this.store
                .current_permission((this.configured_enabled)()? && this.tailscale.is_some())
        })
    }
    fn publish(&self, receipt: ArtifactExposureReceipt) -> BoxFuture<Result<(), ArtifactError>> {
        let this = self.clone();
        blocking(move || this.publish_locked(receipt))
    }
    fn unpublish(&self, receipt: ArtifactExposureReceipt) -> BoxFuture<Result<(), ArtifactError>> {
        let this = self.clone();
        blocking(move || {
            let mut guard = this.store.lock()?;
            cleanup_receipt(
                &mut guard,
                &CommandAdapter(this.commands.as_ref()),
                this.tailscale.as_deref(),
                &receipt,
            )
        })
    }
    fn reconcile_startup(&self) -> BoxFuture<Result<(), ArtifactError>> {
        let this = self.clone();
        blocking(move || {
            let mut guard = this.store.lock()?;
            cleanup_owned(
                &mut guard,
                &CommandAdapter(this.commands.as_ref()),
                this.tailscale.as_deref(),
            )
        })
    }
}

fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ArtifactError> + Send + 'static,
) -> BoxFuture<Result<T, ArtifactError>> {
    Box::pin(async move {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| unavailable("preview exposure worker failed"))?
    })
}

struct CommandAdapter<'a>(&'a dyn Commands);
impl CommandRunner for CommandAdapter<'_> {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
        let output = self
            .0
            .run(
                program,
                args,
                None,
                Limits {
                    timeout: Duration::from_secs(15),
                    max_stdout: 1024 * 1024,
                },
            )
            .map_err(|_| std::io::Error::other("preview tailscale command failed"))?;
        Ok(CommandOutput {
            success: output.code == Some(0),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::new(),
        })
    }
}

fn occupied(status: &ServeStatus, port: u16) -> bool {
    status.ports.contains(&port) || status.proxies.contains_key(&port)
}
fn exact_target(status: &ServeStatus, receipt: &ArtifactExposureReceipt) -> bool {
    status.proxies.get(&receipt.https_port)
        == Some(&format!("http://127.0.0.1:{}", receipt.loopback_port))
}
fn status(runner: &dyn CommandRunner, bin: &Path) -> Result<ServeStatus, ArtifactError> {
    let out = runner
        .run(&bin.to_string_lossy(), &["serve", "status", "--json"])
        .map_err(|_| unavailable("preview serve status could not be read"))?;
    if !out.success {
        return Err(unavailable("preview serve status could not be read"));
    }
    parse_serve_status(&out.stdout)
        .ok_or_else(|| unavailable("preview serve status could not be read"))
}

/// Blocking; caller holds the shared lifecycle guard across revocation and all cleanup.
/// Used by the separate tailnet CLI as well as restart cleanup; retains every failed receipt.
pub fn cleanup_owned(
    guard: &mut LifecycleGuard,
    runner: &dyn CommandRunner,
    bin: Option<&Path>,
) -> Result<(), ArtifactError> {
    let receipts = guard.snapshot()?.receipts;
    let mut failure = None;
    for stored in receipts {
        if let Err(error) = cleanup_receipt(guard, runner, bin, &stored.receipt) {
            failure = Some(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

fn cleanup_receipt(
    guard: &mut LifecycleGuard,
    runner: &dyn CommandRunner,
    bin: Option<&Path>,
    receipt: &ArtifactExposureReceipt,
) -> Result<(), ArtifactError> {
    // A caller-provided port/target is never sufficient authority to remove a mapping.
    if !guard
        .snapshot()?
        .receipts
        .iter()
        .any(|stored| &stored.receipt == receipt)
    {
        return Ok(());
    }
    let bin = bin.ok_or_else(|| unavailable("preview cleanup requires tailscale"))?;
    if exact_target(&status(runner, bin)?, receipt) {
        let https = format!("--https={}", receipt.https_port);
        // Remove only our root handler, preserving unrelated handlers on the same port.
        let out = runner
            .run(
                &bin.to_string_lossy(),
                &["serve", "--yes", &https, "--set-path=/", "off"],
            )
            .map_err(|_| unavailable("preview mapping could not be removed"))?;
        if !out.success {
            return Err(unavailable("preview mapping could not be removed"));
        }
        if exact_target(&status(runner, bin)?, receipt) {
            return Err(unavailable("preview mapping is still active"));
        }
    }
    guard.tombstone_binding(&receipt.binding_id)?;
    guard.finish_cleanup(receipt)?;
    Ok(())
}
