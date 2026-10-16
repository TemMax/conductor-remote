//! Nonsecret durable lifecycle metadata, serialized under a cross-process flock.
//! Blocking helpers: async hosts must dispatch whole lock/transaction/exposure sequences off
//! their async worker threads. Hold the guard across permission checks and actual publication
//! or revocation. The runtime calls its exposure adapter and must not nest this lock.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::files::{digest, io_error, open_absolute_directory, open_at, read_limited};
use super::{ArtifactError, ArtifactExposurePermission, ArtifactExposureReceipt};

const STATE: &str = "artifact-state.json";
const LOCK: &str = "artifact-lifecycle.lock";
const STORE_MAX_BYTES: u64 = 8 * 1024 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn unavailable(message: &str) -> ArtifactError {
    ArtifactError::Unavailable(message.into())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn origin_uri(origin: &str) -> Result<axum::http::Uri, ArtifactError> {
    let uri: axum::http::Uri = origin
        .parse()
        .map_err(|_| unavailable("invalid stored origin"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| unavailable("invalid stored origin"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| unavailable("invalid stored origin"))?;
    if !matches!(scheme, "http" | "https")
        || authority.as_str().contains('@')
        || authority.port_u16() == Some(0)
        || uri.host().is_none_or(str::is_empty)
        || format!("{scheme}://{authority}") != origin
    {
        return Err(unavailable("invalid stored origin"));
    }
    Ok(uri)
}

fn valid_cookie_name(name: &str) -> bool {
    name.strip_prefix("conductor_preview_")
        .is_some_and(|suffix| {
            (32..=64).contains(&suffix.len()) && suffix.bytes().all(|b| b.is_ascii_hexdigit())
        })
}

/// Stable origin/cookie *name* binding; this type has no field for cookie values or HTML.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OriginBinding {
    pub binding_id: String,
    pub identity: String,
    pub origin: String,
    pub cookie_name: String,
    pub reserved_at_millis: u64,
    /// Inactive reservations are never deleted or assigned to a different identity.
    pub tombstoned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReceiptPhase {
    Pending,
    Active,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnedMappingReceipt {
    pub receipt: ArtifactExposureReceipt,
    pub phase: ReceiptPhase,
}

/// An atomic snapshot, not a live session registry. No grants, cookie values or source text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactStoreSnapshot {
    version: u32,
    pub generation: u64,
    pub remote_revoked: bool,
    pub bindings: Vec<OriginBinding>,
    pub receipts: Vec<OwnedMappingReceipt>,
}

impl Default for ArtifactStoreSnapshot {
    fn default() -> Self {
        Self {
            version: 1,
            generation: 0,
            remote_revoked: false,
            bindings: Vec::new(),
            receipts: Vec::new(),
        }
    }
}

impl ArtifactStoreSnapshot {
    pub fn permission(&self, configured_enabled: bool) -> ArtifactExposurePermission {
        ArtifactExposurePermission {
            enabled: configured_enabled && !self.remote_revoked && self.generation != u64::MAX,
            generation: self.generation,
        }
    }

    /// Startup removes *all* these exact owned targets, pending and active. Only successful
    /// cleanup calls `finish_cleanup`; failures remain durable for the next retry/restart.
    pub fn recovery_receipts(&self) -> &[OwnedMappingReceipt] {
        &self.receipts
    }

    /// Includes tombstones. Local allocation must exclude these and configured main origins.
    pub fn reserved_origins(&self) -> impl Iterator<Item = &str> {
        self.bindings.iter().map(|binding| binding.origin.as_str())
    }

    fn validate(&self) -> Result<(), ArtifactError> {
        if self.version != 1 {
            return Err(unavailable("unsupported artifact state version"));
        }
        let mut origins = std::collections::BTreeSet::new();
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for binding in &self.bindings {
            origin_uri(&binding.origin)?;
            if !valid_digest(&binding.identity)
                || !valid_cookie_name(&binding.cookie_name)
                || binding.binding_id
                    != digest(&[
                        b"binding",
                        binding.identity.as_bytes(),
                        binding.origin.as_bytes(),
                    ])
                || !origins.insert(&binding.origin)
                || !ids.insert(&binding.binding_id)
                || !names.insert(&binding.cookie_name)
            {
                return Err(unavailable("invalid artifact origin reservations"));
            }
        }
        let mut ports = std::collections::BTreeSet::new();
        for stored in &self.receipts {
            self.validate_receipt(&stored.receipt)?;
            if stored.receipt.generation > self.generation
                || !ports.insert(stored.receipt.https_port)
            {
                return Err(unavailable("invalid artifact ownership receipts"));
            }
        }
        Ok(())
    }

    fn validate_receipt(&self, receipt: &ArtifactExposureReceipt) -> Result<(), ArtifactError> {
        let binding = self
            .bindings
            .iter()
            .find(|binding| binding.binding_id == receipt.binding_id)
            .ok_or_else(|| unavailable("receipt has no reserved binding"))?;
        let uri = origin_uri(&receipt.origin)?;
        if binding.origin != receipt.origin
            || uri.scheme_str() != Some("https")
            || uri.port_u16() != Some(receipt.https_port)
            || !(30000..=39999).contains(&receipt.https_port)
            || receipt.loopback_port == 0
            || receipt.loopback_port == receipt.https_port
        {
            return Err(unavailable("receipt does not match its reserved origin"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ArtifactStore {
    directory: Arc<File>,
}

impl ArtifactStore {
    /// Uses only artifact-state.json / artifact-lifecycle.lock in this private state directory.
    /// Resolve the configured existing parent (including system aliases such as /var) once;
    /// all actual opens walk its canonical path with O_NOFOLLOW. The final state directory,
    /// missing descendants and state/lock files never follow symlinks. New directories are 0700.
    pub fn open(state_dir: &Path) -> Result<Self, ArtifactError> {
        if !state_dir.is_absolute()
            || state_dir
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(unavailable("absolute artifact state directory required"));
        }
        // Canonicalize only an existing configured ancestor, never the state directory itself.
        // Keep the final component for no-follow opening even when it already exists.
        let mut ancestor = state_dir
            .parent()
            .ok_or_else(|| unavailable("artifact state directory cannot be filesystem root"))?;
        let mut missing = vec![state_dir
            .file_name()
            .ok_or_else(|| unavailable("invalid artifact state directory"))?
            .to_owned()];
        let mut resolved = loop {
            match ancestor.canonicalize() {
                Ok(path) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(
                        ancestor
                            .file_name()
                            .ok_or_else(|| unavailable("state directory ancestor is missing"))?
                            .to_owned(),
                    );
                    ancestor = ancestor
                        .parent()
                        .ok_or_else(|| unavailable("state directory ancestor is missing"))?;
                }
                Err(error) => return Err(io_error(error)),
            }
        };
        for name in missing.into_iter().rev() {
            resolved.push(name);
        }
        let mut directory = open_absolute_directory(Path::new("/"))?;
        for part in resolved.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    match open_at(&directory, name, true) {
                        Ok(next) => directory = next,
                        Err(ArtifactError::Missing(_)) => {
                            use std::os::unix::ffi::OsStrExt;
                            let name = CString::new(name.as_bytes())
                                .map_err(|_| unavailable("invalid state path"))?;
                            // SAFETY: live parent descriptor and NUL-terminated single component.
                            let result = unsafe {
                                libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700)
                            };
                            if result < 0
                                && std::io::Error::last_os_error().raw_os_error()
                                    != Some(libc::EEXIST)
                            {
                                return Err(io_error(std::io::Error::last_os_error()));
                            }
                            // Persist the new directory entry before relying on its contents.
                            directory.sync_all().map_err(io_error)?;
                            directory = open_at(
                                &directory,
                                OsStr::new(
                                    name.to_str()
                                        .map_err(|_| unavailable("invalid state path"))?,
                                ),
                                true,
                            )?;
                        }
                        Err(error) => return Err(error),
                    }
                }
                _ => return Err(unavailable("invalid state directory component")),
            }
        }
        let metadata = directory.metadata().map_err(io_error)?;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(unavailable(
                "artifact state directory is not private to its owner",
            ));
        }
        let store = Self {
            directory: Arc::new(directory),
        };
        // Serialize initial creation too; do not replace any malformed/insecure existing state.
        let guard = store.lock()?;
        match open_at(&store.directory, OsStr::new(STATE), false) {
            Err(ArtifactError::Missing(_)) => guard.save(&ArtifactStoreSnapshot::default())?,
            Ok(_) => {
                guard.snapshot()?;
            }
            Err(error) => return Err(error),
        }
        Ok(store)
    }

    /// Readers see either complete old or complete new state through atomic rename.
    pub fn snapshot(&self) -> Result<ArtifactStoreSnapshot, ArtifactError> {
        load(&self.directory)
    }

    pub fn current_permission(
        &self,
        configured_enabled: bool,
    ) -> Result<ArtifactExposurePermission, ArtifactError> {
        Ok(self.snapshot()?.permission(configured_enabled))
    }

    /// Blocks until the cross-process lifecycle lock is available. Every call opens an
    /// independent file description, so separate store handles/threads also serialize.
    pub fn lock(&self) -> Result<LifecycleGuard, ArtifactError> {
        let lock = create_file(&self.directory, LOCK, false)?;
        loop {
            // SAFETY: lock owns a live file descriptor. No pointer arguments.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(io_error(error));
            }
        }
        let guard = LifecycleGuard {
            directory: self.directory.clone(),
            lock,
        };
        guard.ensure_current_lock()?;
        Ok(guard)
    }
}

/// RAII flock. Keep alive across external publish/unpublish; dropping also unlocks on error.
#[derive(Debug)]
pub struct LifecycleGuard {
    directory: Arc<File>,
    lock: File,
}

impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        // SAFETY: lock owns a live fd. Closing it also releases flock if unlock is interrupted.
        unsafe {
            libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl LifecycleGuard {
    fn ensure_current_lock(&self) -> Result<(), ArtifactError> {
        let named_lock = open_at(&self.directory, OsStr::new(LOCK), false)?;
        secure_file(&named_lock, false)?;
        let held = self.lock.metadata().map_err(io_error)?;
        let named = named_lock.metadata().map_err(io_error)?;
        if held.dev() != named.dev() || held.ino() != named.ino() {
            return Err(unavailable("artifact lifecycle lock was replaced"));
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<ArtifactStoreSnapshot, ArtifactError> {
        self.ensure_current_lock()?;
        load(&self.directory)
    }

    /// Persist before listener/exposure allocation. Same identity/origin reuses its original
    /// cookie name; a tombstone can reactivate only for that identity. Other identities fail.
    pub fn reserve_origin(
        &mut self,
        identity: &str,
        origin: &str,
        cookie_name: &str,
        now: SystemTime,
    ) -> Result<OriginBinding, ArtifactError> {
        if !valid_digest(identity) || !valid_cookie_name(cookie_name) {
            return Err(unavailable("invalid artifact identity or cookie name"));
        }
        origin_uri(origin)?;
        let mut state = self.snapshot()?;
        if let Some(index) = state
            .bindings
            .iter()
            .position(|binding| binding.origin == origin)
        {
            if state.bindings[index].identity != identity {
                return Err(unavailable("artifact origin is permanently reserved"));
            }
            state.bindings[index].tombstoned = false;
            let binding = state.bindings[index].clone();
            self.save(&state)?;
            return Ok(binding);
        }
        let reserved_at_millis = now
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| elapsed.as_millis().try_into().ok())
            .ok_or_else(|| unavailable("invalid origin reservation time"))?;
        let binding = OriginBinding {
            binding_id: digest(&[b"binding", identity.as_bytes(), origin.as_bytes()]),
            identity: identity.into(),
            origin: origin.into(),
            cookie_name: cookie_name.into(),
            reserved_at_millis,
            tombstoned: false,
        };
        state.bindings.push(binding.clone());
        self.save(&state)?;
        Ok(binding)
    }

    pub fn tombstone_binding(&mut self, binding_id: &str) -> Result<(), ArtifactError> {
        let mut state = self.snapshot()?;
        let binding = state
            .bindings
            .iter_mut()
            .find(|binding| binding.binding_id == binding_id)
            .ok_or_else(|| unavailable("unknown artifact binding"))?;
        binding.tombstoned = true;
        self.save(&state)
    }

    /// Off/Disable MUST call this before removing proxies. Receipts and origins survive even
    /// when no sessions are live; overflow fails closed instead of reusing a generation.
    pub fn revoke_remote(&mut self) -> Result<ArtifactExposurePermission, ArtifactError> {
        let mut state = self.snapshot()?;
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| unavailable("artifact revocation generation exhausted"))?;
        state.remote_revoked = true;
        self.save(&state)?;
        Ok(state.permission(true))
    }

    /// Only after a successful explicit Enable/Ensure. A saved-off Ensure must never call it.
    pub fn allow_remote(&mut self) -> Result<ArtifactExposurePermission, ArtifactError> {
        let mut state = self.snapshot()?;
        if state.generation == u64::MAX {
            return Err(unavailable("artifact revocation generation exhausted"));
        }
        state.remote_revoked = false;
        self.save(&state)?;
        Ok(state.permission(true))
    }

    /// Call with effective configured permission read while holding this guard. Persists the
    /// pending receipt BEFORE any external publication. Caller still checks exact mapping
    /// ownership/allocation races through its real exposure adapter; this method publishes nothing.
    pub fn begin_publication(
        &mut self,
        receipt: ArtifactExposureReceipt,
        configured_enabled: bool,
    ) -> Result<OwnedMappingReceipt, ArtifactError> {
        let mut state = self.snapshot()?;
        let permission = state.permission(configured_enabled);
        if !permission.enabled || permission.generation != receipt.generation {
            return Err(ArtifactError::Forbidden(
                "remote artifact permission was revoked".into(),
            ));
        }
        state.validate_receipt(&receipt)?;
        if state
            .bindings
            .iter()
            .any(|binding| binding.binding_id == receipt.binding_id && binding.tombstoned)
        {
            return Err(unavailable("artifact binding is inactive"));
        }
        if let Some(stored) = state
            .receipts
            .iter()
            .find(|stored| stored.receipt.https_port == receipt.https_port)
        {
            if stored.receipt == receipt {
                return Ok(stored.clone());
            }
            return Err(unavailable("artifact mapping still requires cleanup"));
        }
        let stored = OwnedMappingReceipt {
            receipt,
            phase: ReceiptPhase::Pending,
        };
        state.receipts.push(stored.clone());
        self.save(&state)?;
        Ok(stored)
    }

    /// Only after the real adapter reports successful publication, still under this guard.
    pub fn mark_active(&mut self, receipt: &ArtifactExposureReceipt) -> Result<(), ArtifactError> {
        let mut state = self.snapshot()?;
        if !state.permission(true).enabled || state.generation != receipt.generation {
            return Err(ArtifactError::Forbidden(
                "remote artifact permission was revoked".into(),
            ));
        }
        if state
            .bindings
            .iter()
            .any(|binding| binding.binding_id == receipt.binding_id && binding.tombstoned)
        {
            return Err(unavailable("artifact binding is inactive"));
        }
        let stored = state
            .receipts
            .iter_mut()
            .find(|stored| &stored.receipt == receipt)
            .ok_or_else(|| unavailable("pending artifact receipt is missing"))?;
        stored.phase = ReceiptPhase::Active;
        self.save(&state)
    }

    /// Call only after exact-owned-target removal, or confirmation that target is absent or
    /// belongs to somebody else. On cleanup failure call nothing: retain the receipt for retry.
    pub fn finish_cleanup(
        &mut self,
        receipt: &ArtifactExposureReceipt,
    ) -> Result<bool, ArtifactError> {
        let mut state = self.snapshot()?;
        let before = state.receipts.len();
        state.receipts.retain(|stored| &stored.receipt != receipt);
        if before == state.receipts.len() {
            return Ok(false);
        }
        self.save(&state)?;
        Ok(true)
    }

    fn save(&self, state: &ArtifactStoreSnapshot) -> Result<(), ArtifactError> {
        self.ensure_current_lock()?;
        state.validate()?;
        let bytes =
            serde_json::to_vec(state).map_err(|_| unavailable("cannot encode artifact state"))?;
        if bytes.len() as u64 > STORE_MAX_BYTES {
            return Err(unavailable("artifact state capacity exhausted"));
        }
        let (temp, mut file) = loop {
            let temp = format!(
                ".artifact-state-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            );
            match create_file(&self.directory, &temp, true) {
                Ok(file) => break (temp, file),
                Err(ArtifactError::Unavailable(message))
                    if message == "artifact temporary state already exists" =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
        };
        let result = (|| {
            file.write_all(&bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            self.ensure_current_lock()?;
            let from = CString::new(temp.as_str())
                .map_err(|_| unavailable("invalid temporary state name"))?;
            // SAFETY: live directory descriptor and NUL-terminated file names in that directory.
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    from.as_ptr(),
                    self.directory.as_raw_fd(),
                    c"artifact-state.json".as_ptr(),
                )
            } < 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            self.directory.sync_all().map_err(io_error)
        })();
        // Unlink only our exact temporary name (rename already removed it on success).
        let temp = CString::new(temp).map_err(|_| unavailable("invalid temporary state name"))?;
        // SAFETY: single component, live directory descriptor, no symlink traversal.
        unsafe {
            libc::unlinkat(self.directory.as_raw_fd(), temp.as_ptr(), 0);
        }
        result
    }
}

fn secure_file(file: &File, allow_unlinked_snapshot: bool) -> Result<(), ArtifactError> {
    let metadata = file.metadata().map_err(io_error)?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || (metadata.nlink() != 1 && !(allow_unlinked_snapshot && metadata.nlink() == 0))
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(unavailable(
            "artifact state file must be owned, unlinked elsewhere and mode 0600",
        ));
    }
    Ok(())
}

fn create_file(directory: &File, name: &str, exclusive: bool) -> Result<File, ArtifactError> {
    let name = CString::new(name).map_err(|_| unavailable("invalid state name"))?;
    let flags = libc::O_RDWR
        | libc::O_CREAT
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if exclusive { libc::O_EXCL } else { 0 };
    // SAFETY: live directory descriptor, NUL-terminated single name, valid mode for O_CREAT.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if exclusive && error.raw_os_error() == Some(libc::EEXIST) {
            return Err(unavailable("artifact temporary state already exists"));
        }
        return Err(io_error(error));
    }
    // SAFETY: successful openat returns a uniquely owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    secure_file(&file, false)?;
    Ok(file)
}

fn load(directory: &File) -> Result<ArtifactStoreSnapshot, ArtifactError> {
    let file = open_at(directory, OsStr::new(STATE), false)?;
    // A concurrent atomic rename can unlink the old inode after open. It remains a complete
    // immutable snapshot; reject multiple hardlinks, but allow this already-open old version.
    secure_file(&file, true)?;
    let bytes = read_limited(file, STORE_MAX_BYTES)?;
    let state: ArtifactStoreSnapshot = serde_json::from_slice(&bytes)
        .map_err(|_| unavailable("invalid artifact state; refusing to overwrite"))?;
    state.validate()?;
    Ok(state)
}
