//! Which files the phone may preview, and reading them.
//!
//! A chat links to source files and shows local images. The path arrives from a
//! remote client, so every read is gated by the same policy: the target's real
//! path must lie inside one of the directories this relay may read from, and a
//! refusal never depends on whether the file exists.

pub mod attachments;

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// A preview stays small enough to render smoothly in the phone's source sheet.
const PREVIEW_MAX_BYTES: u64 = 512 * 1024;
/// A local image larger than this is not sent.
const IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Lines shown on each side of the requested line.
const CONTEXT_LINES: usize = 100;
/// Lines shown when no line is requested.
const FIRST_LINES: usize = 500;
/// The largest integer a JavaScript number holds exactly: `2^53 - 1`.
const MAX_SAFE_LINE: u64 = 9_007_199_254_740_991;

const PUBLIC_REFUSAL: &str =
    "this relay is reachable from the internet, so it previews files inside Conductor workspaces only";
const TAILNET_REFUSAL: &str = "outside the files this relay may read";

const SOURCE_EXTENSIONS: &[&str] = &[
    ".bash", ".c", ".cc", ".cpp", ".css", ".go", ".h", ".hpp", ".html", ".java", ".js", ".json",
    ".jsx", ".md", ".mjs", ".mts", ".php", ".py", ".rb", ".rs", ".scss", ".sh", ".sql", ".svg",
    ".swift", ".toml", ".ts", ".tsx", ".txt", ".yaml", ".yml",
];

const IMAGE_EXTENSIONS: &[&str] = &[".avif", ".gif", ".jpeg", ".jpg", ".png", ".webp"];

/// Where the relay is reachable from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExposeMode {
    Tailnet,
    Public,
}

impl ExposeMode {
    /// `public` or `funnel` (any case, trimmed) is `Public`; anything else, or nothing, is `Tailnet`.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_lowercase()).as_deref() {
            Some("public" | "funnel") => ExposeMode::Public,
            _ => ExposeMode::Tailnet,
        }
    }

    fn refusal(self) -> &'static str {
        match self {
            ExposeMode::Public => PUBLIC_REFUSAL,
            ExposeMode::Tailnet => TAILNET_REFUSAL,
        }
    }
}

/// The directories a preview may read from, kept as given; they are canonicalised on every check (a root that cannot be canonicalised then is used as given), because the relay may start before Conductor creates its workspaces root.
#[derive(Clone, Debug)]
pub struct PreviewRoots {
    workspaces: PathBuf,
    home: PathBuf,
    skills: Option<PathBuf>,
    temp: Vec<PathBuf>,
}

impl PreviewRoots {
    /// `skills` is Conductor's bundled skills directory, `temp` the temporary directories local images may also come from.
    pub fn new(workspaces_root: &Path, home: &Path, skills: Option<&Path>, temp: &[&Path]) -> Self {
        Self {
            workspaces: workspaces_root.to_path_buf(),
            home: home.to_path_buf(),
            skills: skills.map(Path::to_path_buf),
            temp: temp.iter().map(|p| p.to_path_buf()).collect(),
        }
    }

    /// The roots of this Mac: `skills` = `/Applications/Conductor.app/Contents/Resources/conductor-skill/skills`,
    /// `temp` = `std::env::temp_dir()` and `/tmp`.
    pub fn system(workspaces_root: &Path, home: &Path) -> Self {
        let skills =
            Path::new("/Applications/Conductor.app/Contents/Resources/conductor-skill/skills");
        let temp_dir = std::env::temp_dir();
        Self::new(
            workspaces_root,
            home,
            Some(skills),
            &[temp_dir.as_path(), Path::new("/tmp")],
        )
    }

    /// The roots a path may lie in, as given. `images` adds the temporary directories.
    fn allowed(&self, mode: ExposeMode, images: bool) -> Vec<&Path> {
        let mut roots = vec![self.workspaces.as_path()];
        if mode == ExposeMode::Tailnet {
            roots.push(self.home.as_path());
            roots.extend(self.skills.as_deref());
        }
        if images {
            roots.extend(self.temp.iter().map(PathBuf::as_path));
        }
        roots
    }

    /// Every root this relay knows of, whatever the mode.
    fn configured(&self) -> Vec<&Path> {
        let mut roots = vec![self.workspaces.as_path(), self.home.as_path()];
        roots.extend(self.skills.as_deref());
        roots.extend(self.temp.iter().map(PathBuf::as_path));
        roots
    }
}

/// The lowercased extension, from the last `.` of the file name, with its dot.
fn extension(path: &str) -> Option<String> {
    let name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
    name.rfind('.').map(|dot| name[dot..].to_lowercase())
}

/// Whether a path names a previewable source file: the extension (from the last `.` of the file name, lowercased) is in the reference's `SOURCE_EXTENSIONS`.
pub fn is_previewable_source(path: &str) -> bool {
    extension(path).is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext.as_str()))
}

/// The same for the reference's `IMAGE_EXTENSIONS`.
pub fn is_previewable_image(path: &str) -> bool {
    extension(path).is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// Resolve `.` and `..` components without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// True only for a descendant, comparing whole components, so `/w/root-other` is not inside `/w/root`.
fn inside(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

fn canonical_or_given(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| normalize(root))
}

/// Expand a leading `~/` and require an absolute path. `~/` goes through `normalize` first, as
/// joining it onto the home directory does, so `~/a.md/.` is a file named `a.md`.
fn expand(written: &str, home: &Path) -> Option<String> {
    let path = match written.strip_prefix("~/") {
        Some(rest) => normalize(&home.join(rest)).to_str()?.to_owned(),
        None => written.to_owned(),
    };
    path.starts_with('/').then_some(path)
}

/// A location suffix `:<line>[:<column>]` at the end, as the leftmost match of `:([1-9]\d*)(?::\d+)?$`.
/// The reference is rejected if the line is not a safe integer.
fn split_location(reference: &str) -> Result<(&str, Option<u64>), ()> {
    for (at, _) in reference.match_indices(':') {
        let rest = &reference[at + 1..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || rest.starts_with('0') {
            continue;
        }
        let after = &rest[digits..];
        let column_ok = match after.strip_prefix(':') {
            None => after.is_empty(),
            Some(col) => !col.is_empty() && col.bytes().all(|b| b.is_ascii_digit()),
        };
        if !column_ok {
            continue;
        }
        return match rest[..digits].parse::<u64>() {
            Ok(line) if line <= MAX_SAFE_LINE => Ok((&reference[..at], Some(line))),
            _ => Err(()),
        };
    }
    Ok((reference, None))
}

/// Why a target is unreadable, before its content is looked at.
enum Access {
    Allowed(PathBuf),
    Forbidden,
    NotFound,
}

/// Decide whether `target` (absolute, normalised) may be read.
///
/// An existing target is judged by its canonical path. One that cannot be canonicalised is judged
/// by its normalised path against each root as given and canonicalised, so a missing file answers
/// as an existing one in the same place would, and a refusal never shows whether a file exists.
/// So is an existing one whose canonical path is outside every `configured` root: behind a link
/// out of the roots, an existing file answers as a missing one does.
fn check_access(target: &Path, roots: &[&Path], configured: &[&Path]) -> Access {
    let canonical_roots: Vec<PathBuf> = roots.iter().map(|r| canonical_or_given(r)).collect();
    if let Ok(real) = target.canonicalize() {
        if !canonical_roots.iter().any(|root| inside(&real, root)) {
            if configured
                .iter()
                .any(|root| inside(&real, &canonical_or_given(root)))
            {
                return Access::Forbidden;
            }
            return by_path(target, roots, &canonical_roots);
        }
        return match std::fs::metadata(&real) {
            Ok(meta) if meta.is_file() => Access::Allowed(real),
            Ok(_) => Access::NotFound,
            Err(_) => by_path(target, roots, &canonical_roots),
        };
    }
    by_path(target, roots, &canonical_roots)
}

fn by_path(target: &Path, roots: &[&Path], canonical_roots: &[PathBuf]) -> Access {
    let given = roots.iter().map(|r| normalize(r));
    if given
        .chain(canonical_roots.iter().cloned())
        .any(|root| inside(target, &root))
    {
        Access::NotFound
    } else {
        Access::Forbidden
    }
}

/// Read at most `limit` bytes; `None` when the file holds more.
fn read_limited(path: &Path, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The web app's `FilePreviewResponse`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePreview {
    pub path: String,
    pub line: Option<i64>,
    pub line_start: i64,
    pub line_end: i64,
    pub total_lines: i64,
    pub content: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewError {
    NotFound,
    Forbidden(&'static str),
    TooLarge,
    NotText,
}

/// A source file a chat links to: `<absolute or ~/ path>[:line[:column]]`, already percent-decoded.
pub fn file_preview(
    reference: &str,
    roots: &PreviewRoots,
    mode: ExposeMode,
) -> Result<FilePreview, PreviewError> {
    let (written, line) = split_location(reference).map_err(|()| PreviewError::NotFound)?;
    let expanded = expand(written, &roots.home).ok_or(PreviewError::NotFound)?;
    if !is_previewable_source(&expanded) {
        return Err(PreviewError::NotFound);
    }
    let target = normalize(Path::new(&expanded));
    let path = target.to_str().ok_or(PreviewError::NotFound)?.to_owned();

    let real = match check_access(&target, &roots.allowed(mode, false), &roots.configured()) {
        Access::Allowed(real) => real,
        Access::NotFound => return Err(PreviewError::NotFound),
        Access::Forbidden => return Err(PreviewError::Forbidden(mode.refusal())),
    };

    // A file that cannot be read after the check passed is reported as not text.
    let raw = match read_limited(&real, PREVIEW_MAX_BYTES) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Err(PreviewError::TooLarge),
        Err(_) => return Err(PreviewError::NotText),
    };
    if raw.contains(&0) {
        return Err(PreviewError::NotText);
    }
    let content = String::from_utf8(raw).map_err(|_| PreviewError::NotText)?;

    let lines: Vec<&str> = content.split('\n').collect();
    let total = lines.len();
    let focus = line.map(|l| usize::try_from(l).unwrap_or(usize::MAX).min(total));
    let (start, end) = match focus {
        None => (0, total.min(FIRST_LINES)),
        Some(focus) => (
            focus.saturating_sub(CONTEXT_LINES + 1),
            total.min(focus + CONTEXT_LINES),
        ),
    };
    Ok(FilePreview {
        path,
        line: focus.map(|f| f as i64),
        line_start: start as i64 + 1,
        line_end: end as i64,
        total_lines: total as i64,
        content: lines[start..end].join("\n"),
        truncated: start > 0 || end < total,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalImage {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    NotFound,
    TooLarge,
}

fn image_content_type(path: &str) -> Option<&'static str> {
    Some(match extension(path)?.as_str() {
        ".avif" => "image/avif",
        ".gif" => "image/gif",
        ".jpeg" | ".jpg" => "image/jpeg",
        ".png" => "image/png",
        ".webp" => "image/webp",
        _ => return None,
    })
}

/// A local image a chat shows: an absolute or `~/` path, already percent-decoded.
pub fn local_image(
    path: &str,
    roots: &PreviewRoots,
    mode: ExposeMode,
) -> Result<LocalImage, ImageError> {
    let expanded = expand(path, &roots.home).ok_or(ImageError::NotFound)?;
    if !is_previewable_image(&expanded) {
        return Err(ImageError::NotFound);
    }
    let content_type = image_content_type(&expanded).ok_or(ImageError::NotFound)?;
    let target = normalize(Path::new(&expanded));

    // An image has no refusal of its own: a path it will not serve is simply not there.
    let real = match check_access(&target, &roots.allowed(mode, true), &roots.configured()) {
        Access::Allowed(real) => real,
        Access::NotFound | Access::Forbidden => return Err(ImageError::NotFound),
    };
    match read_limited(&real, IMAGE_MAX_BYTES) {
        Ok(Some(bytes)) => Ok(LocalImage {
            content_type,
            bytes,
        }),
        Ok(None) => Err(ImageError::TooLarge),
        Err(_) => Err(ImageError::NotFound),
    }
}
