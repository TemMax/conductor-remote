//! Conductor attachments, written from outside Conductor.
//!
//! An attachment is two things: a file at `<worktree>/.context/attachments/<6 chars>/<name>`,
//! and a token in the prompt text, `@⟦<name>⟧(<the path, percent-encoded>)`. Nothing else is
//! written, in particular nothing in Conductor's database.
//!
//! Files picked before a new workspace exists are kept under the same layout in a staging
//! root, and copied into the worktree before the token that refers to them is sent. The staging
//! directory is the authority on what is staged, so a restart between picking a file and
//! creating the workspace loses nothing.

use std::collections::HashSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path};
use std::time::{Duration, SystemTime};

/// The largest attachment the phone may upload.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
/// `.context/attachments` under a worktree or the staging root.
pub const ATTACHMENTS_DIR: &str = ".context/attachments";

/// The longest file name kept, in UTF-8 bytes: room for an extension on every filesystem.
const MAX_NAME_BYTES: usize = 120;
const ID_LENGTH: usize = 6;
const ID_ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
/// New ids tried after the first one collides with an existing directory.
const ID_RETRIES: usize = 5;

/// One attachment on disk and what the phone needs to refer to it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Written {
    /// The six-character directory id, kept out of the visible file name.
    pub id: String,
    /// The file name as the chip shows it.
    pub name: String,
    /// Worktree-relative, `/`-separated.
    pub path: String,
    pub bytes: u64,
    /// The `@⟦…⟧(…)` token to drop into the prompt.
    pub token: String,
}

/// A file name that cannot leave the directory it was meant for.
///
/// `/` and `\` become `-`, control characters are removed, leading dots are stripped (a leading dot
/// hides the file, two climb out of the directory), the result is trimmed and clipped to 120 UTF-8
/// bytes on a character boundary, and an empty result is `attachment`.
pub fn attachment_name(raw: &str) -> String {
    let flat: String = raw
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .filter(|c| !is_name_control(*c))
        .collect();
    let flat = trim(flat.trim_start_matches('.'));
    let mut clipped = String::new();
    for c in flat.chars() {
        if clipped.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        clipped.push(c);
    }
    let clipped = trim(&clipped);
    if clipped.is_empty() {
        "attachment".to_owned()
    } else {
        clipped.to_owned()
    }
}

/// U+0000 to U+001F and U+007F: the characters that must not reach a path.
fn is_name_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}')
}

/// Trims white space and the byte order mark, as the reference's `String.prototype.trim` does.
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Six characters of `A-Za-z0-9` from random bytes. The id names a directory and guards nothing,
/// so the slight bias of `byte % 62` does not matter, and a collision is retried by the caller.
pub fn attachment_id() -> String {
    let mut bytes = [0u8; ID_LENGTH];
    getrandom::fill(&mut bytes).expect("the system provides random bytes");
    bytes
        .iter()
        .map(|b| char::from(ID_ALPHABET[usize::from(*b) % ID_ALPHABET.len()]))
        .collect()
}

/// `@⟦<name>⟧(<path>)` with the whole relative path percent-encoded the way `encodeURIComponent`
/// does: slashes become `%2F`, and `A-Za-z0-9 - _ . ! ~ * ' ( )` stay.
pub fn attachment_token(name: &str, relative_path: &str) -> String {
    format!("@⟦{name}⟧({})", encode_uri_component(relative_path))
}

fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn is_id(text: &str) -> bool {
    text.len() == ID_LENGTH && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Create `path` and any missing parents; `private` makes every directory it creates 0700.
fn create_dirs(path: &Path, private: bool) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    if private {
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Writes `bytes` to `<root>/.context/attachments/<id>/<sanitised name>` and returns what the
/// phone needs. Each attachment gets its own directory, so two files with the same name never
/// collide. On a collision a new id is tried up to five times; after that the error is
/// `AlreadyExists`, so an existing attachment is never overwritten.
///
/// `private` is for the staging root: files are created 0600 and directories 0700. Without it
/// (a worktree) the default modes apply.
pub fn write_attachment(
    root: &Path,
    name: &str,
    bytes: &[u8],
    private: bool,
) -> io::Result<Written> {
    use std::io::Write;

    let safe = attachment_name(name);
    let base = root.join(ATTACHMENTS_DIR);
    create_dirs(&base, private)?;

    let mut attempts = 0;
    let id = loop {
        let id = attachment_id();
        let mut builder = DirBuilder::new();
        if private {
            builder.mode(0o700);
        }
        match builder.create(base.join(&id)) {
            Ok(()) => break id,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < ID_RETRIES => {
                attempts += 1;
            }
            Err(e) => return Err(e),
        }
    };

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    if private {
        options.mode(0o600);
    }
    options.open(base.join(&id).join(&safe))?.write_all(bytes)?;

    let path = format!("{ATTACHMENTS_DIR}/{id}/{safe}");
    Ok(Written {
        token: attachment_token(&safe, &path),
        id,
        name: safe,
        path,
        bytes: bytes.len() as u64,
    })
}

/// The staged files of `ids` under the staging root: `None` unless every id is unique, has the
/// six-character shape, and its directory holds exactly one regular file whose name is already
/// a safe attachment name.
pub fn staged_attachments(root: &Path, ids: &[String]) -> Option<Vec<Written>> {
    let unique: HashSet<&String> = ids.iter().collect();
    if unique.len() != ids.len() {
        return None;
    }
    let mut staged = Vec::with_capacity(ids.len());
    for id in ids {
        if !is_id(id) {
            return None;
        }
        let dir = root.join(ATTACHMENTS_DIR).join(id);
        let mut files = Vec::new();
        for entry in fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            if entry.file_type().ok()?.is_file() {
                files.push(entry);
            }
        }
        let [file] = files.as_slice() else {
            return None;
        };
        let name = file.file_name().into_string().ok()?;
        if name != attachment_name(&name) {
            return None;
        }
        let meta = fs::metadata(file.path()).ok()?;
        if !meta.is_file() {
            return None;
        }
        let path = format!("{ATTACHMENTS_DIR}/{id}/{name}");
        staged.push(Written {
            token: attachment_token(&name, &path),
            id: id.clone(),
            name,
            path,
            bytes: meta.len(),
        });
    }
    Some(staged)
}

/// Copies staged files into a worktree at the same relative path. A file already there with the
/// same bytes is accepted (the successful half of an earlier, interrupted run); a different one is
/// the error `an attachment already exists at <path>`. A staged path that is not a plain relative
/// path is refused.
pub fn materialize(staged: &[Written], staging_root: &Path, worktree: &Path) -> Result<(), String> {
    for attachment in staged {
        let relative = Path::new(&attachment.path);
        if !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(format!(
                "not a relative attachment path: {}",
                attachment.path
            ));
        }
        let source = staging_root.join(relative);
        let destination = worktree.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", attachment.path))?;
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
        {
            Ok(mut out) => {
                // This call created the destination, so a failed copy must not leave it behind:
                // the next try would find a different file and fail for good.
                let copied =
                    File::open(&source).and_then(|mut input| io::copy(&mut input, &mut out));
                if let Err(e) = copied {
                    drop(out);
                    let _ = fs::remove_file(&destination);
                    return Err(format!("{}: {e}", attachment.path));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let wanted = fs::read(&source).map_err(|e| format!("{}: {e}", attachment.path))?;
                let existing =
                    fs::read(&destination).map_err(|e| format!("{}: {e}", attachment.path))?;
                if wanted != existing {
                    return Err(format!(
                        "an attachment already exists at {}",
                        attachment.path
                    ));
                }
            }
            Err(e) => return Err(format!("{}: {e}", attachment.path)),
        }
    }
    Ok(())
}

/// Removes a staged id's directory; false when it was not there, could not be removed, or the id
/// is malformed.
pub fn discard_staged(root: &Path, id: &str) -> bool {
    if !is_id(id) {
        return false;
    }
    let dir = root.join(ATTACHMENTS_DIR).join(id);
    dir.symlink_metadata().is_ok() && fs::remove_dir_all(&dir).is_ok()
}

/// Removes six-character directories under the staging root that are older than `max_age` and not
/// in `keep`; returns how many. Anything else there, such as a stray file or a directory of another
/// shape, is left alone, and a directory that cannot be removed is skipped.
pub fn prune_staged(root: &Path, max_age: Duration, keep: &HashSet<String>) -> usize {
    let Ok(entries) = fs::read_dir(root.join(ATTACHMENTS_DIR)) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_id(&name) || keep.contains(&name) {
            continue;
        }
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        // A directory modified in the future is not old.
        if now.duration_since(modified).is_ok_and(|age| age >= max_age)
            && fs::remove_dir_all(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}
