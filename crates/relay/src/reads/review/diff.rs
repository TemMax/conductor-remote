//! The workspace diff, the diff of one file and the list of source files.
//!
//! Each read is a handful of `git` calls in the worktree. None of them returns an error: a call
//! that cannot run, or that exits with a code other than 0, contributes its empty result.

use super::super::extras::commands::{Commands, Output};
use super::basis::{
    diff_basis, tracked_diff_files, untracked_files, untracked_patch, DiffFile, GIT_LIMITS,
    MAX_UNTRACKED_FILES,
};
use crate::files::is_previewable_source;

/// The longest patch, in UTF-16 code units (JavaScript's `length`), that `workspace_diff` keeps whole.
const MAX_PATCH_UNITS: usize = 400_000;

/// How many source files `list_source_files` returns at most.
const MAX_LISTED_FILES: usize = 20_000;

/// What a workspace changed against its target branch: the web app's `WorkspaceDiff`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDiff {
    pub base: String,
    pub merge_base: Option<String>,
    pub files: Vec<DiffFile>,
    pub patch: String,
    pub truncated: bool,
    /// Uncommitted changes in the worktree.
    pub dirty: bool,
    /// Commits on `HEAD` that the remote-tracking branch does not have.
    pub unpushed: bool,
}

/// One changed file's complete patch: the web app's `WorkspaceFileDiff`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceFileDiff {
    pub path: String,
    pub patch: String,
}

/// The previewable source files of a worktree: the response of the files endpoint.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceFiles {
    pub files: Vec<String>,
    pub truncated: bool,
}

/// The standard output of a git call in `worktree` that exited 0; `None` for any other outcome.
fn git_ok(commands: &dyn Commands, worktree: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut full = vec!["-C", worktree];
    full.extend_from_slice(args);
    match commands.run("git", &full, None, GIT_LIMITS) {
        Ok(Output {
            code: Some(0),
            stdout,
            ..
        }) => Some(stdout),
        _ => None,
    }
}

/// The same, as text; git's output is read as UTF-8, lossily.
fn git_text(commands: &dyn Commands, worktree: &str, args: &[&str]) -> Option<String> {
    git_ok(commands, worktree, args).map(|stdout| String::from_utf8_lossy(&stdout).into_owned())
}

/// `git rev-list --count @{upstream}..HEAD` printed a number above 0.
fn has_unpushed(commands: &dyn Commands, worktree: &str) -> bool {
    git_text(
        commands,
        worktree,
        &["rev-list", "--count", "@{upstream}..HEAD"],
    )
    .and_then(|printed| printed.trim().parse::<u64>().ok())
    .is_some_and(|count| count > 0)
}

/// Keeps the first 400,000 UTF-16 code units of a longer patch and notes the cut.
///
/// A surrogate pair cut in half leaves U+FFFD where JavaScript leaves a lone surrogate.
fn truncate_patch(patch: String) -> (String, bool) {
    let units: Vec<u16> = patch.encode_utf16().collect();
    if units.len() <= MAX_PATCH_UNITS {
        return (patch, false);
    }
    let kept = String::from_utf16_lossy(&units[..MAX_PATCH_UNITS]);
    (
        format!("{kept}\n\n… diff truncated ({} bytes) …", units.len()),
        true,
    )
}

/// Everything the workspace changed against its target branch, committed or not.
///
/// The git calls, in order: `rev-parse --verify --quiet` of `origin/<base>` and then of `<base>`
/// (the first that names a commit is the base, else `<base>` as given), `merge-base <base> HEAD`,
/// `diff --numstat -z <against>`, `diff <against>`, `ls-files --others --exclude-standard -z`
/// and one `diff --no-index` per untracked file (the first 500), `status --porcelain` and
/// `rev-list --count @{upstream}..HEAD`. A failed call gives the empty answer: no merge base, no
/// tracked files, no tracked patch (also when the output is too large), no untracked files, a
/// clean worktree, nothing unpushed.
pub fn workspace_diff(commands: &dyn Commands, worktree: &str, base_branch: &str) -> WorkspaceDiff {
    let basis = diff_basis(commands, worktree, base_branch);
    let mut files = tracked_diff_files(commands, worktree, &basis.against);
    let mut patch = git_text(
        commands,
        worktree,
        &["diff", "--end-of-options", &basis.against],
    )
    .unwrap_or_default();

    for path in untracked_files(commands, worktree)
        .into_iter()
        .take(MAX_UNTRACKED_FILES)
    {
        let Some((untracked, added)) = untracked_patch(commands, worktree, &path) else {
            continue;
        };
        files.push(DiffFile {
            path,
            old_path: None,
            added,
            removed: 0,
        });
        patch.push_str(&untracked);
    }

    let (patch, truncated) = truncate_patch(patch);
    let dirty = git_text(commands, worktree, &["status", "--porcelain"])
        .is_some_and(|status| !status.trim().is_empty());
    let unpushed = has_unpushed(commands, worktree);

    WorkspaceDiff {
        base: basis.base,
        merge_base: basis.merge_base,
        files,
        patch,
        truncated,
        dirty,
        unpushed,
    }
}

/// The path of `requested` relative to `worktree`, resolved lexically (no file-system access) as
/// `path.relative(root, path.resolve(root, requested))` does; `None` when the request is empty,
/// holds a NUL, is absolute, or leaves the worktree.
fn relative_to_worktree(worktree: &str, requested: &str) -> Option<String> {
    if requested.is_empty() || requested.contains('\0') || requested.starts_with('/') {
        return None;
    }
    let absolute = if worktree.starts_with('/') {
        worktree.to_owned()
    } else {
        let cwd = std::env::current_dir().ok()?;
        format!("{}/{worktree}", cwd.to_str()?)
    };
    let push = |stack: &mut Vec<String>, segment: &str| match segment {
        "" | "." => {}
        ".." => {
            stack.pop();
        }
        other => stack.push(other.to_owned()),
    };
    let mut root: Vec<String> = Vec::new();
    for segment in absolute.split('/') {
        push(&mut root, segment);
    }
    let mut resolved = root.clone();
    for segment in requested.split('/') {
        push(&mut resolved, segment);
    }
    // Leaving the worktree shows as a path that no longer starts with the root.
    let inside = resolved.len() > root.len() && resolved[..root.len()] == root[..];
    inside.then(|| resolved[root.len()..].join("/"))
}

/// The complete patch of one changed file, however large; `None` when the path is invalid or
/// names no changed file.
///
/// A tracked file is read with `git diff --no-color <against> -- [:(literal)<old path>]
/// :(literal)<path>`, the old path included for a rename so that git still sees the move. Any
/// other path must be listed by `git ls-files --others --exclude-standard -z -- :(literal)<path>`
/// exactly, and is read as `workspace_diff` reads an untracked file.
pub fn workspace_file_diff(
    commands: &dyn Commands,
    worktree: &str,
    base_branch: &str,
    requested: &str,
) -> Option<WorkspaceFileDiff> {
    let relative = relative_to_worktree(worktree, requested)?;
    let basis = diff_basis(commands, worktree, base_branch);
    let literal = format!(":(literal){relative}");

    let tracked = tracked_diff_files(commands, worktree, &basis.against)
        .into_iter()
        .find(|file| file.path == relative);
    if let Some(file) = tracked {
        let old_literal = file.old_path.map(|old| format!(":(literal){old}"));
        let mut args = vec![
            "diff",
            "--no-color",
            "--end-of-options",
            basis.against.as_str(),
            "--",
        ];
        args.extend(old_literal.as_deref());
        args.push(&literal);
        let patch = git_text(commands, worktree, &args).filter(|patch| !patch.is_empty())?;
        return Some(WorkspaceFileDiff {
            path: relative,
            patch,
        });
    }

    let listed = git_ok(
        commands,
        worktree,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            &literal,
        ],
    )?;
    if !listed
        .split(|byte| *byte == 0)
        .any(|path| path == relative.as_bytes())
    {
        return None;
    }
    let (patch, _) = untracked_patch(commands, worktree, &relative)?;
    Some(WorkspaceFileDiff {
        path: relative,
        patch,
    })
}

/// The previewable source files of the worktree: tracked ones and untracked ones git does not
/// ignore, plus everything under the root `.context/` directory even when ignored. The first
/// 20,000; `truncated` when there were more. A failed listing gives an empty list.
pub fn list_source_files(commands: &dyn Commands, worktree: &str) -> WorkspaceFiles {
    let Some(listing) = git_ok(
        commands,
        worktree,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            // Unignore the directory itself to let git descend, then all its contents.
            "--exclude=!/.context/",
            "--exclude=!/.context/**",
            "-z",
        ],
    ) else {
        return WorkspaceFiles {
            files: Vec::new(),
            truncated: false,
        };
    };
    let mut files: Vec<String> = listing
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .filter(|path| is_previewable_source(path))
        .collect();
    let truncated = files.len() > MAX_LISTED_FILES;
    files.truncate(MAX_LISTED_FILES);
    WorkspaceFiles { files, truncated }
}
