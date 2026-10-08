//! Tool images and repository icons served to the phone.

use std::io::Read;

use rusqlite::OptionalExtension;

use super::workspaces::resolve_repo_icon;
use super::{ReadError, Reads};
use crate::transcript::images::{tool_image_at, ToolImage};

/// The row of a tool image reference.
const TOOL_IMAGE_SQL: &str = "SELECT content FROM session_messages WHERE rowid = ? LIMIT 1";

/// The root of a repository, by its name.
const REPO_ROOT_SQL: &str = "SELECT root_path FROM repos WHERE name = ? LIMIT 1";

/// The largest icon file served.
const MAX_ICON_BYTES: u64 = 5 * 1024 * 1024;

/// A repository's icon file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoIconFile {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

/// An unsigned decimal integer: digits only, no sign, no space.
fn decimal<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The content type an icon file's extension names.
fn icon_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

impl Reads {
    /// The image a reference `<rowid>.<index>` names, if there is one.
    ///
    /// The reference is split at its last `.`; both parts must be decimal integers without a
    /// sign, otherwise there is nothing. The index counts as the transcript parser numbers the
    /// `images` of a row.
    pub fn tool_image(&self, reference: &str) -> Result<Option<ToolImage>, ReadError> {
        let Some((row, index)) = reference.rsplit_once('.') else {
            return Ok(None);
        };
        let (Some(rowid), Some(index)) = (decimal::<i64>(row), decimal::<usize>(index)) else {
            return Ok(None);
        };
        let content = self.db().read("tool image", |conn| {
            conn.query_row(TOOL_IMAGE_SQL, [rowid], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()
        })?;
        Ok(content
            .flatten()
            .and_then(|content| tool_image_at(&content, index)))
    }

    /// The icon file of the repository with this name, if it has one.
    ///
    /// Nothing for an unknown repository, one without a root, one without an icon file, a file
    /// larger than 5 MiB or a file that cannot be read.
    pub fn repo_icon(&self, repo_name: &str) -> Result<Option<RepoIconFile>, ReadError> {
        let root = self.db().read("repo icon", |conn| {
            conn.query_row(REPO_ROOT_SQL, [repo_name], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()
        })?;
        let Some(root) = root.flatten().filter(|root| !root.is_empty()) else {
            return Ok(None);
        };
        let Some(path) = resolve_repo_icon(&root) else {
            return Ok(None);
        };
        Ok(read_icon(&path))
    }
}

/// The file's bytes, unless it is larger than the limit or cannot be read. At most one byte
/// past the limit is read, so a file that grows meanwhile is not read whole.
fn read_icon(path: &std::path::Path) -> Option<RepoIconFile> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_ICON_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_ICON_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_ICON_BYTES {
        return None;
    }
    Some(RepoIconFile {
        content_type: icon_content_type(path),
        bytes,
    })
}
