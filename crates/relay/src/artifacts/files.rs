//! Blocking filesystem capabilities. Async entry points dispatch reads to `spawn_blocking`.
//! A prepared root pins its directory inode; request paths never reopen its pathname.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::{ArtifactError, ArtifactPreviewSource, CreateArtifactPreviewRequest};

pub const DOCUMENT_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const RESOURCE_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Consumers add these to every resource response, including HEAD and range failures.
pub const RESOURCE_HEADERS: [(&str, &str); 4] = [
    ("X-Content-Type-Options", "nosniff"),
    ("Cache-Control", "no-store"),
    ("Cross-Origin-Resource-Policy", "same-origin"),
    ("Accept-Ranges", "bytes"),
];

fn forbidden() -> ArtifactError {
    ArtifactError::Forbidden("artifact path is outside the selected safe directory".into())
}

pub(crate) fn io_error(error: std::io::Error) -> ArtifactError {
    match error.raw_os_error() {
        Some(libc::ENOENT) => ArtifactError::Missing("artifact file is missing".into()),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EACCES | libc::EPERM) => forbidden(),
        _ => ArtifactError::Unavailable("artifact filesystem operation failed".into()),
    }
}

fn c_name(name: &OsStr) -> Result<CString, ArtifactError> {
    CString::new(name.as_bytes()).map_err(|_| ArtifactError::Invalid("NUL in path".into()))
}

/// All opens, including the initial absolute traversal, refuse symlinks and special files.
pub(crate) fn open_at(
    directory: &File,
    name: &OsStr,
    directory_only: bool,
) -> Result<File, ArtifactError> {
    let name = c_name(name)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory_only { libc::O_DIRECTORY } else { 0 };
    // SAFETY: name is NUL terminated; directory owns a live descriptor; openat returns a new fd.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    // SAFETY: fd was successfully opened and ownership is transferred exactly once.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata().map_err(io_error)?;
    if (directory_only && !metadata.is_dir()) || (!directory_only && !metadata.is_file()) {
        return Err(forbidden());
    }
    Ok(file)
}

pub(crate) fn open_absolute_directory(path: &Path) -> Result<File, ArtifactError> {
    if !path.is_absolute() {
        return Err(ArtifactError::Invalid("absolute directory required".into()));
    }
    // SAFETY: constant NUL-terminated path; returned fd is uniquely owned below.
    let fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    // SAFETY: successful open transfers one owned descriptor.
    let mut directory = unsafe { File::from_raw_fd(fd) };
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => directory = open_at(&directory, name, true)?,
            _ => return Err(forbidden()),
        }
    }
    Ok(directory)
}

fn protected(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return true;
    };
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        ".git"
            | ".local"
            | ".ssh"
            | ".aws"
            | ".azure"
            | ".gnupg"
            | ".config"
            | ".codex"
            | ".claude"
            | ".kube"
            | ".docker"
            | ".npmrc"
            | ".yarnrc"
            | ".yarnrc.yml"
            | ".pypirc"
            | ".netrc"
            | ".git-credentials"
            | ".gitconfig"
            | ".password-store"
            | ".1password"
            | "keychains"
            | "keyrings"
            | "agents.local.md"
            | "local.properties"
            | "gradle.properties"
            | "credentials"
            | "credentials.json"
            | "id_rsa"
            | "id_ed25519"
            | "id_ecdsa"
            | "id_dsa"
            | "__conductor_preview"
    ) || name.starts_with(".env")
        || matches!(
            Path::new(&name).extension().and_then(OsStr::to_str),
            Some(
                "pem"
                    | "key"
                    | "pub"
                    | "p12"
                    | "pfx"
                    | "crt"
                    | "cer"
                    | "der"
                    | "keystore"
                    | "jks"
                    | "kdbx"
                    | "keychain"
                    | "keychain-db"
                    | "zip"
                    | "tar"
                    | "gz"
                    | "tgz"
                    | "bz2"
                    | "xz"
                    | "7z"
                    | "rar"
            )
        )
}

fn check_path(path: &Path) -> Result<(), ArtifactError> {
    for part in path.components() {
        match part {
            Component::Normal(name) if !protected(name) => {}
            Component::RootDir => {}
            _ => return Err(forbidden()),
        }
    }
    Ok(())
}

fn descend(
    directory: &File,
    relative: &Path,
    final_directory: bool,
) -> Result<File, ArtifactError> {
    let mut current = directory.try_clone().map_err(io_error)?;
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() && !final_directory {
        return Err(forbidden());
    }
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(forbidden());
        };
        if protected(name) {
            return Err(forbidden());
        }
        current = open_at(&current, name, final_directory || index + 1 < parts.len())?;
    }
    Ok(current)
}

#[derive(Debug)]
struct SourceRoot {
    path: PathBuf,
    directory: File,
}

/// Construct once from the already selected source policy, never from an HTTP path/Host.
#[derive(Debug)]
pub struct ArtifactFilePolicy {
    roots: Vec<SourceRoot>,
    broad_roots: Vec<PathBuf>,
}

impl ArtifactFilePolicy {
    pub fn new(source_roots: &[PathBuf], broad_roots: &[PathBuf]) -> Result<Self, ArtifactError> {
        let roots = source_roots
            .iter()
            .map(|path| {
                let path = path.canonicalize().map_err(io_error)?;
                check_path(&path)?;
                let directory = open_absolute_directory(&path)?;
                Ok(SourceRoot { path, directory })
            })
            .collect::<Result<Vec<_>, ArtifactError>>()?;
        let broad_roots = broad_roots
            .iter()
            .map(|path| path.canonicalize().map_err(io_error))
            .collect::<Result<_, _>>()?;
        Ok(Self { roots, broad_roots })
    }

    fn selected_root(&self, path: &Path) -> Result<ResourceRoot, ArtifactError> {
        check_path(path)?;
        if path == Path::new("/") || self.broad_roots.iter().any(|broad| path == broad) {
            return Err(forbidden());
        }
        let root = self
            .roots
            .iter()
            .filter(|root| path.starts_with(&root.path))
            .max_by_key(|root| root.path.components().count())
            .ok_or_else(forbidden)?;
        let relative = path.strip_prefix(&root.path).map_err(|_| forbidden())?;
        Ok(ResourceRoot {
            canonical_path: Some(path.to_owned()),
            directory: Some(descend(&root.directory, relative, true)?),
        })
    }

    /// Already decoded reference; home expansion is explicit and injectable, never inferred.
    /// A location suffix selects the UI's code mode, but never clips this document.
    pub fn prepare_file(
        &self,
        reference: &str,
        home: Option<&Path>,
    ) -> Result<PreparedArtifact, ArtifactError> {
        let reference = strip_location(reference)?;
        let path = if let Some(relative) = reference.strip_prefix("~/") {
            home.ok_or_else(|| ArtifactError::Invalid("home directory is unavailable".into()))?
                .join(relative)
        } else {
            PathBuf::from(reference)
        };
        if !path.is_absolute() {
            return Err(ArtifactError::Invalid(
                "absolute artifact path required".into(),
            ));
        }
        check_path(&path)?;
        // Canonicalization selects an authorized identity; all actual reads walk pinned fds.
        let path = path.canonicalize().map_err(io_error)?;
        check_path(&path)?;
        if !matches!(
            path.extension()
                .and_then(OsStr::to_str)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("html" | "htm")
        ) {
            return Err(ArtifactError::Unsupported("HTML document required".into()));
        }
        let parent = path.parent().ok_or_else(forbidden)?;
        let root = Arc::new(self.selected_root(parent)?);
        let name = path.file_name().ok_or_else(forbidden)?;
        let file = descend(
            root.directory.as_ref().ok_or_else(forbidden)?,
            Path::new(name),
            false,
        )?;
        let content = String::from_utf8(read_limited(file, DOCUMENT_MAX_BYTES)?)
            .map_err(|_| ArtifactError::Unsupported("HTML source must be UTF-8".into()))?;
        let entry_path = format!("/{}", encode_segment(name.as_bytes()));
        let identity = digest(&[b"file", parent.as_os_str().as_bytes(), name.as_bytes()]);
        Ok(PreparedArtifact {
            identity,
            entry_path,
            source: ArtifactPreviewSource {
                path: Some(path.to_str().ok_or_else(forbidden)?.to_owned()),
                content,
            },
            root,
        })
    }

    /// No base means an empty virtual root. An invalid supplied base never falls back to it.
    pub fn prepare_inline(
        &self,
        html: String,
        base_directory: Option<&Path>,
    ) -> Result<PreparedArtifact, ArtifactError> {
        if html.len() as u64 > DOCUMENT_MAX_BYTES {
            return Err(ArtifactError::TooLarge("HTML exceeds 8 MiB".into()));
        }
        let root = match base_directory {
            Some(path) => {
                if !path.is_absolute() {
                    return Err(ArtifactError::Invalid(
                        "absolute inline base required".into(),
                    ));
                }
                check_path(path)?;
                self.selected_root(&path.canonicalize().map_err(io_error)?)?
            }
            None => ResourceRoot {
                canonical_path: None,
                directory: None,
            },
        };
        let base = root
            .canonical_path
            .as_deref()
            .map(|path| path.as_os_str().as_bytes())
            .unwrap_or_default();
        let identity = digest(&[b"inline", base, html.as_bytes()]);
        Ok(PreparedArtifact {
            identity,
            entry_path: "/index.html".into(),
            source: ArtifactPreviewSource {
                path: None,
                content: html,
            },
            root: Arc::new(root),
        })
    }
}

/// Original source and identity are memory-only. Persist the identity digest, never `source`.
#[derive(Debug)]
pub struct PreparedArtifact {
    pub identity: String,
    pub entry_path: String,
    pub source: ArtifactPreviewSource,
    pub root: Arc<ResourceRoot>,
}

/// Call this from async control handlers; source reads and directory traversal stay off workers.
pub async fn prepare_document(
    policy: Arc<ArtifactFilePolicy>,
    request: CreateArtifactPreviewRequest,
    home: Option<PathBuf>,
) -> Result<PreparedArtifact, ArtifactError> {
    tokio::task::spawn_blocking(move || match request {
        CreateArtifactPreviewRequest::File { reference, .. } => {
            policy.prepare_file(&reference, home.as_deref())
        }
        CreateArtifactPreviewRequest::Html {
            html,
            base_directory,
            ..
        } => policy.prepare_inline(html, base_directory.as_deref().map(Path::new)),
    })
    .await
    .map_err(|_| ArtifactError::Unavailable("artifact reader stopped".into()))?
}

#[derive(Debug)]
pub struct ResourceRoot {
    canonical_path: Option<PathBuf>,
    directory: Option<File>,
}

impl ResourceRoot {
    pub fn canonical_path(&self) -> Option<&Path> {
        self.canonical_path.as_deref()
    }

    pub async fn read_resource(
        self: Arc<Self>,
        request_path: String,
        range: Option<String>,
        head: bool,
    ) -> Result<StaticResource, ArtifactError> {
        tokio::task::spawn_blocking(move || {
            self.read_resource_blocking(&request_path, range.as_deref(), head)
        })
        .await
        .map_err(|_| ArtifactError::Unavailable("artifact reader stopped".into()))?
    }

    /// Raw URI path only (no query), percent-decoded exactly once here, never by the caller.
    /// Consumers must reject Service-Worker script requests before calling this helper.
    pub fn read_resource_blocking(
        &self,
        request_path: &str,
        range: Option<&str>,
        head: bool,
    ) -> Result<StaticResource, ArtifactError> {
        let relative = decode_request_path(request_path)?;
        let directory = self
            .directory
            .as_ref()
            .ok_or_else(|| ArtifactError::Missing("empty artifact root".into()))?;
        let mut file = descend(directory, &relative, false)?;
        let size = file.metadata().map_err(io_error)?.len();
        if size > RESOURCE_MAX_BYTES {
            return Err(ArtifactError::TooLarge("resource exceeds 32 MiB".into()));
        }
        // HEAD ignores Range as required for a non-GET request.
        let selected = if head {
            ByteRange::Full
        } else {
            select_range(range, size)
        };
        let (status, start, length, content_range) = match selected {
            ByteRange::Full => (200, 0, size, None),
            ByteRange::Partial { start, end } => (
                206,
                start,
                end - start + 1,
                Some(format!("bytes {start}-{end}/{size}")),
            ),
            ByteRange::Unsatisfiable => (416, 0, 0, Some(format!("bytes */{size}"))),
        };
        let mut body = Vec::new();
        if !head && status != 416 {
            file.seek(SeekFrom::Start(start)).map_err(io_error)?;
            // Full reads catch growth as well as truncation; partial reads stay bounded.
            if status == 200 {
                body = read_limited(file, RESOURCE_MAX_BYTES)?;
                if body.len() as u64 != size {
                    return Err(ArtifactError::Unavailable(
                        "resource changed while reading".into(),
                    ));
                }
            } else {
                file.take(length).read_to_end(&mut body).map_err(io_error)?;
                if body.len() as u64 != length {
                    return Err(ArtifactError::Unavailable(
                        "resource changed while reading".into(),
                    ));
                }
            }
        }
        Ok(StaticResource {
            status,
            mime: match relative.extension().and_then(OsStr::to_str) {
                Some(extension)
                    if extension.eq_ignore_ascii_case("js")
                        || extension.eq_ignore_ascii_case("mjs") =>
                {
                    "text/javascript".into()
                }
                _ => mime_guess::from_path(relative)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_owned(),
            },
            content_length: length,
            content_range,
            body,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct StaticResource {
    pub status: u16,
    pub mime: String,
    pub content_length: u64,
    pub content_range: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRange {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

/// Single byte ranges; malformed/unknown/multipart ranges are ignored with a full 200.
pub fn select_range(header: Option<&str>, size: u64) -> ByteRange {
    let Some(value) = header.and_then(|value| value.strip_prefix("bytes=")) else {
        return ByteRange::Full;
    };
    let Some((first, last)) = value.split_once('-') else {
        return ByteRange::Full;
    };
    let number = |value: &str| {
        if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
            value.parse::<u64>().ok()
        } else {
            None
        }
    };
    let (start, end) = if first.is_empty() {
        let Some(suffix) = number(last) else {
            return ByteRange::Full;
        };
        if suffix == 0 || size == 0 {
            return ByteRange::Unsatisfiable;
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        let Some(start) = number(first) else {
            return ByteRange::Full;
        };
        let end = if last.is_empty() {
            size.saturating_sub(1)
        } else {
            let Some(end) = number(last) else {
                return ByteRange::Full;
            };
            if end < start {
                return ByteRange::Full;
            }
            end.min(size.saturating_sub(1))
        };
        if start >= size {
            return ByteRange::Unsatisfiable;
        }
        (start, end)
    };
    ByteRange::Partial { start, end }
}

pub(crate) fn read_limited(file: File, limit: u64) -> Result<Vec<u8>, ArtifactError> {
    if file.metadata().map_err(io_error)?.len() > limit {
        return Err(ArtifactError::TooLarge(
            "artifact exceeds size limit".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > limit {
        return Err(ArtifactError::TooLarge(
            "artifact exceeds size limit".into(),
        ));
    }
    Ok(bytes)
}

fn strip_location(reference: &str) -> Result<&str, ArtifactError> {
    for (at, _) in reference.match_indices(':') {
        let rest = &reference[at + 1..];
        let mut parts = rest.split(':');
        let line = parts.next().unwrap_or_default();
        let column = parts.next();
        if line.is_empty()
            || line.starts_with('0')
            || !line.bytes().all(|b| b.is_ascii_digit())
            || parts.next().is_some()
            || column.is_some_and(|col| col.is_empty() || !col.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        if line
            .parse::<u64>()
            .ok()
            .is_none_or(|line| line > 9_007_199_254_740_991)
        {
            return Err(ArtifactError::Invalid(
                "invalid artifact line number".into(),
            ));
        }
        return Ok(&reference[..at]);
    }
    Ok(reference)
}

fn decode_request_path(path: &str) -> Result<PathBuf, ArtifactError> {
    let Some(path) = path.strip_prefix('/') else {
        return Err(forbidden());
    };
    if path.is_empty() || path.contains(['?', '#', '\\']) {
        return Err(forbidden());
    }
    let mut output = PathBuf::new();
    for segment in path.split('/') {
        let mut decoded = Vec::new();
        let bytes = segment.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let value = bytes
                    .get(i + 1..i + 3)
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| ArtifactError::Invalid("invalid URL encoding".into()))?;
                decoded.push(value);
                i += 3;
            } else {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
        if decoded.is_empty()
            || decoded == b"."
            || decoded == b".."
            || decoded.iter().any(|byte| matches!(byte, 0 | b'/' | b'\\'))
        {
            return Err(forbidden());
        }
        let name = OsStr::from_bytes(&decoded);
        if protected(name) {
            return Err(forbidden());
        }
        output.push(name);
    }
    Ok(output)
}

fn encode_segment(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            output.push(*byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

pub(crate) fn digest(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}
