//! Contracts for isolated HTML previews. No listener or exposure is started by this module.
//!
//! The runtime will provide `ArtifactPreviews::new(config: ArtifactPreviewConfig)
//! -> Result<Self, ArtifactError>` and implement both service and lifecycle traits below.

pub mod files;
pub mod store;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::delivery::BoxFuture;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum CreateArtifactPreviewRequest {
    File {
        reference: String,
        #[serde(rename = "parentOrigin")]
        parent_origin: String,
    },
    Html {
        html: String,
        #[serde(
            rename = "baseDirectory",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        base_directory: Option<String>,
        #[serde(rename = "parentOrigin")]
        parent_origin: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPreviewSession {
    pub id: String,
    pub origin: String,
    pub bootstrap_url: String,
    pub entry_path: String,
    pub grant: String,
    /// RFC 3339 timestamp.
    pub expires_at: String,
    pub path: Option<String>,
}

/// Full original HTML, without the source-preview endpoint's line clipping or instrumentation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPreviewSource {
    pub path: Option<String>,
    pub content: String,
}

/// A successful close always carries `ok: true`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseArtifactPreviewResult {
    pub ok: bool,
}

/// Typed failures for control routes; messages must be safe to return to the caller.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Missing(String),
    #[error("{0}")]
    TooLarge(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Unavailable(String),
}

impl ArtifactError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::Forbidden(_) => 403,
            Self::Missing(_) => 404,
            Self::TooLarge(_) => 413,
            Self::Unsupported(_) => 415,
            Self::Unavailable(_) => 503,
        }
    }
}

pub trait ArtifactPreviewService: Send + Sync + 'static {
    fn create(
        &self,
        request: CreateArtifactPreviewRequest,
    ) -> BoxFuture<Result<ArtifactPreviewSession, ArtifactError>>;
    fn source(&self, id: String) -> BoxFuture<Result<ArtifactPreviewSource, ArtifactError>>;
    fn close(&self, id: String) -> BoxFuture<Result<CloseArtifactPreviewResult, ArtifactError>>;
}

/// Startup cleanup must complete before accepting creates; shutdown awaits all owned cleanup.
pub trait ArtifactPreviewLifecycle: Send + Sync + 'static {
    fn reconcile_startup(&self) -> BoxFuture<Result<(), ArtifactError>>;
    fn sweep(&self) -> BoxFuture<Result<(), ArtifactError>>;
    fn close_all(&self) -> BoxFuture<Result<(), ArtifactError>>;
}

/// Wall time is for wire timestamps; monotonic elapsed time is for grants and idle expiry.
pub trait ArtifactClock: Send + Sync + 'static {
    fn now(&self) -> SystemTime;
    fn monotonic(&self) -> Duration;
}

/// Production implementations must fill the entire buffer with cryptographic randomness.
pub trait ArtifactEntropy: Send + Sync + 'static {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), ArtifactError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactExposurePermission {
    pub enabled: bool,
    /// Durable revocation generation, checked again under the publication lock.
    pub generation: u64,
}

/// Nonsecret ownership metadata. A binding's HTTPS origin is never reassigned to another identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactExposureReceipt {
    pub binding_id: String,
    pub loopback_port: u16,
    pub https_port: u16,
    pub origin: String,
    pub generation: u64,
}

/// Owns cross-process publication/revocation locking and durable pending/active receipts.
/// The runtime must not nest that lock. No method may replace an unrelated mapping.
pub trait ArtifactExposure: Send + Sync + 'static {
    /// Read effective permission dynamically, including explicit Off and its generation.
    fn current_permission(&self) -> BoxFuture<Result<ArtifactExposurePermission, ArtifactError>>;
    /// Recheck permission/generation under the lock, persist pending ownership, then publish.
    /// `https_port` must be in 30000..=39999; the target is this receipt's loopback port.
    fn publish(&self, receipt: ArtifactExposureReceipt) -> BoxFuture<Result<(), ArtifactError>>;
    /// Remove only the exact owned target; retain the receipt if cleanup fails.
    fn unpublish(&self, receipt: ArtifactExposureReceipt) -> BoxFuture<Result<(), ArtifactError>>;
    /// Recover pending receipts and remove stale owned proxies after a relay restart.
    fn reconcile_startup(&self) -> BoxFuture<Result<(), ArtifactError>>;
}

/// Owned inputs for `ArtifactPreviews::new(config) -> Result<Self, ArtifactError>`.
/// Construction validates these inputs; it never executes project commands.
#[derive(Clone)]
pub struct ArtifactPreviewConfig {
    /// Canonical directories permitted by the relay's current source-read policy.
    pub source_roots: Vec<PathBuf>,
    /// Canonical home/workspaces roots which must never themselves be served as preview roots.
    pub broad_roots: Vec<PathBuf>,
    /// Nonsecret origin bindings and exposure receipts only; no HTML, grants or cookies.
    pub state_dir: PathBuf,
    /// Exact canonical HTTP(S) main-app origins, including explicit scheme and port.
    /// Supplies local relay/Vite and configured tailnet origins; never inferred from Host headers.
    pub allowed_parent_origins: BTreeSet<String>,
    pub clock: Arc<dyn ArtifactClock>,
    pub entropy: Arc<dyn ArtifactEntropy>,
    /// `None` supports local previews only.
    pub exposure: Option<Arc<dyn ArtifactExposure>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactPreviewChannel {
    #[serde(rename = "conductor-artifact-preview")]
    Preview,
}

/// Only protocol version 1 is accepted on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum ArtifactPreviewVersion {
    V1,
}

impl TryFrom<u8> for ArtifactPreviewVersion {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::V1),
            _ => Err("unsupported artifact preview protocol version"),
        }
    }
}

impl From<ArtifactPreviewVersion> for u8 {
    fn from(_: ArtifactPreviewVersion) -> Self {
        1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArtifactDiagnosticKind {
    Javascript,
    Promise,
    Resource,
    Http,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ArtifactPreviewMessage {
    Ready,
    /// Parent-to-bootstrap only; send to an exact target origin, never `*`.
    Authorize {
        grant: String,
    },
    Loaded,
    /// Display-only untrusted data; consumers must bound count and field lengths.
    Diagnostic {
        kind: ArtifactDiagnosticKind,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<u32>,
    },
}

/// Receiving code must also validate the exact origin, source window and active session ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPreviewEnvelope {
    pub channel: ArtifactPreviewChannel,
    pub version: ArtifactPreviewVersion,
    pub session_id: String,
    #[serde(flatten)]
    pub message: ArtifactPreviewMessage,
}
