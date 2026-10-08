//! Finding the ports a workspace's dev server listens on.
//!
//! Only pids, parent pids and arguments of processes are ever requested from `ps`, never their
//! environment: the environment of Conductor's processes carries tokens.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

use crate::reads::extras::commands::{Commands, Limits};

const PS_ARGS: [&str; 3] = ["-axww", "-o", "pid=,ppid=,args="];

/// `lsof` lives in `/usr/sbin`, which is not on the `PATH` of a LaunchAgent, so it is run by its
/// absolute path.
const LSOF: &str = "/usr/sbin/lsof";

const PS_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(10),
    max_stdout: 16 * 1024 * 1024,
};

const LSOF_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(10),
    max_stdout: 1024 * 1024,
};

/// How long one connection attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// How often `wait_for_port` looks.
const POLL: Duration = Duration::from_millis(250);

/// The wrapper processes of the worktree's Run task in a `ps -axww -o pid=,ppid=,args=` listing,
/// and every descendant of them: their pids.
pub fn run_task_pids(listing: &str, home: &Path, worktree: &str) -> Vec<u32> {
    let Some(home) = home.to_str() else {
        return Vec::new();
    };
    let marker = format!(
        "{home}/.conductor/projects/{}/run-",
        worktree.replace('/', "--")
    );

    let mut wrappers: BTreeSet<u32> = BTreeSet::new();
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for line in listing.lines() {
        let Some((pid, ppid, args)) = process_line(line) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
        if args.contains(&marker) {
            wrappers.insert(pid);
        }
    }

    let mut found = wrappers.clone();
    let mut pending: Vec<u32> = wrappers.into_iter().collect();
    while let Some(pid) = pending.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if found.insert(*child) {
                pending.push(*child);
            }
        }
    }
    found.into_iter().collect()
}

/// `pid`, `ppid` and the arguments of a line of `ps -o pid=,ppid=,args=`.
fn process_line(line: &str) -> Option<(u32, u32, &str)> {
    let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
    let (ppid, args) = rest.trim_start().split_once(char::is_whitespace)?;
    Some((pid.parse().ok()?, ppid.parse().ok()?, args))
}

/// The TCP ports in the output of `lsof -nP -iTCP -sTCP:LISTEN -a -p <pids>`, ascending, each once.
pub fn listening_ports_of(lsof_output: &str) -> Vec<u16> {
    lsof_output
        .lines()
        .filter_map(listening_port)
        .collect::<BTreeSet<u16>>()
        .into_iter()
        .collect()
}

/// The port of one row of the table: what follows the last `:` of the first word of `NAME`, which
/// is the word after the `TCP` of the `NODE` column. The header has no such word.
fn listening_port(line: &str) -> Option<u16> {
    let mut words = line.split_whitespace().skip(1);
    words.find(|word| *word == "TCP")?;
    let name = words.next()?;
    name.rsplit_once(':')?.1.parse().ok()
}

/// A `ps` listing kept for a short time and shared by every workspace.
#[derive(Default)]
pub struct ProcessSnapshot {
    kept: Mutex<Option<(Instant, String)>>,
}

impl ProcessSnapshot {
    pub fn new() -> ProcessSnapshot {
        ProcessSnapshot::default()
    }

    /// The listing, read now unless one younger than `max_age` is kept; `None` when `ps` fails.
    /// A caller that finds another one reading waits for it and uses its listing.
    pub fn listing(&self, commands: &dyn Commands, max_age: Duration) -> Option<String> {
        // The lock is held while `ps` runs: that is what makes a second caller wait.
        let mut kept = self.kept.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((at, listing)) = kept.as_ref() {
            if at.elapsed() < max_age {
                return Some(listing.clone());
            }
        }
        *kept = None;
        let output = commands.run("ps", &PS_ARGS, None, PS_LIMITS).ok()?;
        if output.code != Some(0) {
            return None;
        }
        let listing = String::from_utf8_lossy(&output.stdout).into_owned();
        *kept = Some((Instant::now(), listing.clone()));
        Some(listing)
    }
}

/// The ports the worktree's Run task listens on now; empty when it does not run or a program fails.
pub fn run_task_ports(commands: &dyn Commands, home: &Path, worktree: &str) -> Vec<u16> {
    let Some(listing) = ProcessSnapshot::new().listing(commands, Duration::ZERO) else {
        return Vec::new();
    };
    run_task_ports_in(&listing, commands, home, worktree)
}

/// `run_task_ports` over a listing that was already read.
pub fn run_task_ports_in(
    listing: &str,
    commands: &dyn Commands,
    home: &Path,
    worktree: &str,
) -> Vec<u16> {
    let pids = run_task_pids(listing, home, worktree);
    if pids.is_empty() {
        return Vec::new();
    }
    let pids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let args = ["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", pids.as_str()];
    let Ok(table) = commands.run(LSOF, &args, None, LSOF_LIMITS) else {
        return Vec::new();
    };
    // `lsof` exits 1 when nothing matches: an empty answer.
    match table.code {
        Some(0 | 1) => listening_ports_of(&String::from_utf8_lossy(&table.stdout)),
        _ => Vec::new(),
    }
}

/// Whether something accepts TCP connections on `127.0.0.1:<port>` or `[::1]:<port>` within 300 ms each.
pub async fn tcp_open(port: u16) -> bool {
    let connects = |host: &'static str| async move {
        matches!(
            tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port))).await,
            Ok(Ok(_))
        )
    };
    let (v4, v6) = tokio::join!(connects("127.0.0.1"), connects("::1"));
    v4 || v6
}

/// Waits until `tcp_open(port) == open`, looking every 250 ms, for at most `timeout`.
pub async fn wait_for_port(port: u16, open: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if tcp_open(port).await == open {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        tokio::time::sleep(POLL.min(remaining)).await;
    }
}
