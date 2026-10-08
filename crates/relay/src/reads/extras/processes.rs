//! Live processes of workspaces and chats, read from the process list.
//!
//! Only the arguments of a process are ever requested from `ps`, never its environment: the
//! environment of Conductor's processes carries tokens.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::commands::{Commands, Limits};
use super::swr::Swr;
use super::Shared;

/// How long a listing is trusted.
const FRESH: Duration = Duration::from_secs(5);

const LIMITS: Limits = Limits {
    timeout: Duration::from_secs(10),
    max_stdout: 16 * 1024 * 1024,
};

/// Run tasks: the arguments of every process.
const RUN_PS_ARGS: [&str; 3] = ["-axww", "-o", "args="];
/// Agent processes: pid, elapsed time and arguments.
const AGENT_PS_ARGS: [&str; 2] = ["-axo", "pid=,etime=,args="];

/// The computed start of a chat's process moves by up to a second between listings, since `etime`
/// has whole seconds; a new start this close to the previous one is the same process.
const START_TOLERANCE_MS: i64 = 2_000;

/// Source of process facts: which Run tasks and which agent processes are alive.
pub struct ProcessSource {
    shared: Arc<Shared>,
    /// Keys of the worktrees whose Run task is alive.
    runs: Arc<Swr<(), BTreeSet<String>>>,
    /// Chat id to the start of its agent process, in milliseconds since the epoch.
    agents: Arc<Swr<(), BTreeMap<String, i64>>>,
}

/// The output of `ps`, or `None` when it could not be run or failed.
fn list(commands: &dyn Commands, args: &[&str]) -> Option<String> {
    let output = commands.run("ps", args, None, LIMITS).ok()?;
    if output.code != Some(0) {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl ProcessSource {
    pub fn new(shared: Arc<Shared>) -> Self {
        let runs = Swr::new(shared.pool.clone(), shared.revision.clone());
        let agents = Swr::new(shared.pool.clone(), shared.revision.clone());
        Self {
            shared,
            runs: Arc::new(runs),
            agents: Arc::new(agents),
        }
    }

    /// Whether Conductor's Run task of this worktree is alive. `false` before the first listing.
    pub fn run_active(&self, worktree: Option<&str>) -> bool {
        let commands = self.shared.commands.clone();
        let home = self.shared.home.clone();
        let cache = self.runs.clone();
        let keys = self.runs.get(
            &(),
            |entry| entry.at.elapsed() > FRESH,
            move || {
                match list(commands.as_ref(), &RUN_PS_ARGS) {
                    Some(text) => run_task_keys(&text, &home),
                    // A failing `ps` says nothing about any workspace: keep what was known.
                    None => cache.peek(&()).unwrap_or_default(),
                }
            },
        );
        match (keys, worktree) {
            (Some(keys), Some(worktree)) => keys.contains(&worktree.replace('/', "--")),
            _ => false,
        }
    }

    /// When the agent process of a chat started, in milliseconds since the epoch, if one is alive.
    pub fn agent_started_at(&self, session_id: &str) -> Option<i64> {
        let commands = self.shared.commands.clone();
        let cache = self.agents.clone();
        let starts = self.agents.get(
            &(),
            |entry| entry.at.elapsed() > FRESH,
            move || {
                let previous = cache.peek(&()).unwrap_or_default();
                let ran_at = now_ms();
                match list(commands.as_ref(), &AGENT_PS_ARGS) {
                    Some(text) => agent_starts(&text, ran_at, &previous),
                    None => previous,
                }
            },
        )?;
        starts.get(session_id).copied()
    }
}

fn run_task_keys(text: &str, home: &Path) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| run_task_key(line, home))
        .collect()
}

/// The chats of a listing with the start of their latest process. A start within the tolerance of
/// the previous one is replaced by it, so an unchanged process list stores an equal value.
fn agent_starts(
    text: &str,
    ran_at_ms: i64,
    previous: &BTreeMap<String, i64>,
) -> BTreeMap<String, i64> {
    let mut starts: BTreeMap<String, i64> = BTreeMap::new();
    for (id, start) in text
        .lines()
        .filter_map(|line| agent_process(line, ran_at_ms))
    {
        let latest = starts.entry(id).or_insert(start);
        *latest = (*latest).max(start);
    }
    for (id, start) in &mut starts {
        if let Some(known) = previous.get(id) {
            if (*start - *known).abs() <= START_TOLERANCE_MS {
                *start = *known;
            }
        }
    }
    starts
}

/// Elapsed time as `ps` prints it, `[[dd-]hh:]mm:ss`, in seconds.
pub fn parse_etime(value: &str) -> Option<i64> {
    let value = value.trim();
    let number = |part: &str| -> Option<i64> {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (days, hours, minutes, seconds) = match value.split_once('-') {
        Some((days, rest)) => match rest.split(':').collect::<Vec<_>>().as_slice() {
            [h, m, s] => (number(days)?, number(h)?, number(m)?, number(s)?),
            _ => return None,
        },
        None => match value.split(':').collect::<Vec<_>>().as_slice() {
            [h, m, s] => (0, number(h)?, number(m)?, number(s)?),
            [m, s] => (0, 0, number(m)?, number(s)?),
            _ => return None,
        },
    };
    days.checked_mul(86_400)?
        .checked_add(hours.checked_mul(3_600)?)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)
}

/// The worktree key of a line of `ps -axww -o args=` that is a Run task: `zsh` (bare or with a
/// path before it), white space, `<home>/.conductor/projects/<key>/run-run:<digits>.sh`, then
/// white space or the end. The key has no `/`.
pub fn run_task_key(line: &str, home: &Path) -> Option<String> {
    let line = line.trim();
    let shell_end = line.find(char::is_whitespace)?;
    let shell = &line[..shell_end];
    if shell != "zsh" && !shell.ends_with("/zsh") {
        return None;
    }
    let script = line[shell_end..].trim_start();

    let projects = home.join(".conductor").join("projects");
    let rest = script.strip_prefix(projects.to_str()?)?.strip_prefix('/')?;
    let (key, rest) = rest.split_once('/')?;
    if key.is_empty() || key.contains(['\r', '\n']) {
        return None;
    }
    let digits = rest.strip_prefix("run-run:")?;
    let digits_len = digits.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 {
        return None;
    }
    let tail = digits[digits_len..].strip_prefix(".sh")?;
    if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
        return None;
    }
    Some(key.to_owned())
}

/// The chat of a line of `ps -axo pid=,etime=,args=` that is an agent process, and the start of the
/// process: `now_ms` minus `etime`, or 0 when `etime` cannot be read.
pub fn agent_process(line: &str, now_ms: i64) -> Option<(String, i64)> {
    let line = line.trim_start();
    let (pid, rest) = line.split_once(char::is_whitespace)?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (etime, args) = rest.trim_start().split_once(char::is_whitespace)?;
    let args = args.trim();

    // The executable runs up to the first flag: its path holds a space ("Application Support").
    let flags_at = args.char_indices().find_map(|(at, c)| {
        (c.is_whitespace() && args[at + c.len_utf8()..].starts_with("--")).then_some(at)
    });
    let (executable, flags) = match flags_at {
        Some(at) => (args[..at].trim_end(), &args[at..]),
        None => (args, ""),
    };
    if executable != "claude" && !executable.ends_with("/claude") {
        return None;
    }

    let id = session_id(flags)?;
    let started = parse_etime(etime).map_or(0, |seconds| now_ms.saturating_sub(seconds * 1_000));
    Some((id.to_owned(), started))
}

/// The UUID after the first `--resume` or `--session-id` (any case), joined by `=` or white space.
fn session_id(flags: &str) -> Option<&str> {
    let lower = flags.to_ascii_lowercase();
    for at in 0..lower.len() {
        if !lower.is_char_boundary(at) {
            continue;
        }
        let name = ["--resume", "--session-id"]
            .into_iter()
            .find(|name| lower[at..].starts_with(name));
        let Some(name) = name else { continue };
        let after = at + name.len();
        let candidate = match flags[after..].strip_prefix('=') {
            Some(value) => value,
            None => {
                let value = flags[after..].trim_start();
                if value.len() == flags.len() - after {
                    continue;
                }
                value
            }
        };
        if let Some(uuid) = leading_uuid(candidate) {
            return Some(uuid);
        }
    }
    None
}

/// A UUID at the start of `text`, followed by a word boundary.
fn leading_uuid(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let uuid = bytes.get(..36)?;
    let shaped = uuid.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    });
    let bounded = bytes
        .get(36)
        .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'));
    (shaped && bounded).then(|| &text[..36])
}
