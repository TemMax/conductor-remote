//! Plan usage: how much of each provider's rolling allowance is consumed.

use std::cmp::Ordering;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};

/// The agent harnesses Conductor currently offers: the web app's `PlanUsageProviderId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanUsageProviderId {
    Claude,
    Codex,
    Cursor,
    Opencode,
}

/// The web app's `PlanUsageWindow`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanUsageWindow {
    pub id: String,
    pub label: String,
    /// Percentage of this rolling allowance consumed, clamped to 0 to 100.
    pub used_percent: f64,
    /// Unix time in milliseconds, or `None` (JSON `null`) when the provider omits it.
    pub resets_at: Option<i64>,
    /// Provider-reported window size. Left out when unknown (`None`), `null` when `Some(None)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<Option<i64>>,
    /// Claude marks the bucket currently constraining requests. Left out when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

/// The web app's `PlanUsageBucket`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanUsageBucket {
    pub id: String,
    pub label: String,
    pub windows: Vec<PlanUsageWindow>,
}

/// The `status` of a `ProviderPlanUsage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanUsageStatus {
    Available,
    Unavailable,
    Error,
}

/// The web app's `ProviderPlanUsage`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderPlanUsage {
    pub provider: PlanUsageProviderId,
    pub label: String,
    pub status: PlanUsageStatus,
    pub plan: Option<String>,
    pub buckets: Vec<PlanUsageBucket>,
    /// Safe, user-facing explanation. Left out when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The web app's `PlanUsageSnapshot`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanUsageSnapshot {
    pub providers: Vec<ProviderPlanUsage>,
    /// When these provider reads completed, as Unix time in milliseconds.
    pub fetched_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    NotInstalled,
    Failed(String),
}

/// Asks a provider's CLI for its raw usage answer. The real one spawns the CLIs; tests pass a
/// fake.
pub trait PlanProbe: Send + Sync + 'static {
    /// The `response.response` payload of Claude's `get_usage` control answer.
    fn claude(&self) -> Result<serde_json::Value, ProbeError>;
    /// The `result` of Codex's `account/rateLimits/read`.
    fn codex(&self) -> Result<serde_json::Value, ProbeError>;
}

/// The label of Claude's provider entry.
const CLAUDE_LABEL: &str = "Claude Code";
const CODEX_LABEL: &str = "Codex";
const CURSOR_LABEL: &str = "Cursor Agent";
const OPENCODE_LABEL: &str = "OpenCode";

const CLAUDE_ARGS: [&str; 8] = [
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--no-session-persistence",
    "--safe-mode",
];
const CODEX_ARGS: [&str; 1] = ["app-server"];

const CLAUDE_TIMEOUT: Duration = Duration::from_secs(10);
const CODEX_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// Finds the provider CLIs and spawns them.
pub struct SystemProbe {
    agent_binaries: PathBuf,
    search_path: bool,
}

impl SystemProbe {
    /// `with_lookup(agent_binaries, true)`.
    pub fn new(agent_binaries: PathBuf) -> Self {
        Self::with_lookup(agent_binaries, true)
    }

    /// With `search_path` false only the agent-binaries directory is searched (tests: no real CLI
    /// can be found).
    pub fn with_lookup(agent_binaries: PathBuf, search_path: bool) -> Self {
        Self {
            agent_binaries,
            search_path,
        }
    }

    /// The CLI of `provider`: the first executable `<agent_binaries>/<provider>/<version>/<provider>`
    /// by descending version, else the bare name found on `PATH` (when searching it).
    fn binary(&self, provider: &str) -> Option<PathBuf> {
        let root = self.agent_binaries.join(provider);
        let mut versions: Vec<String> = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        versions.sort_by(|a, b| compare_versions(b, a).then_with(|| b.cmp(a)));
        let bundled = versions
            .iter()
            .map(|version| root.join(version).join(provider))
            .find(|candidate| is_executable_file(candidate));
        if bundled.is_some() || !self.search_path {
            return bundled;
        }
        let path_env = std::env::var("PATH").ok();
        crate::reads::extras::commands::resolve_program(provider, path_env.as_deref())
    }

    fn located(&self, provider: &str) -> Result<PathBuf, ProbeError> {
        self.binary(provider).ok_or(ProbeError::NotInstalled)
    }
}

impl PlanProbe for SystemProbe {
    fn claude(&self) -> Result<Value, ProbeError> {
        let binary = self.located("claude")?;
        let request = json!({
            "type": "control_request",
            "request_id": "plan-usage",
            "request": { "subtype": "get_usage" },
        });
        let session = Session {
            name: "Claude",
            binary: &binary,
            args: &CLAUDE_ARGS,
            timeout: CLAUDE_TIMEOUT,
            max_output: MAX_OUTPUT,
        };
        session.converse(&[request], true, |message| {
            let response = message.get("response");
            let answered = message.get("type").and_then(Value::as_str) == Some("control_response")
                && response
                    .and_then(|r| r.get("request_id"))
                    .and_then(Value::as_str)
                    == Some("plan-usage");
            let Some(response) = response.filter(|_| answered) else {
                return Ok(Step::Continue);
            };
            if response.get("subtype").and_then(Value::as_str) != Some("success") {
                return Err(failed("Claude rejected the plan-usage request"));
            }
            Ok(Step::Done(
                response.get("response").cloned().unwrap_or(Value::Null),
            ))
        })
    }

    fn codex(&self) -> Result<Value, ProbeError> {
        let binary = self.located("codex")?;
        let initialize = json!({
            "id": 1,
            "method": "initialize",
            "params": { "clientInfo": { "name": "conductor-remote", "version": "1" } },
        });
        let session = Session {
            name: "Codex",
            binary: &binary,
            args: &CODEX_ARGS,
            timeout: CODEX_TIMEOUT,
            max_output: MAX_OUTPUT,
        };
        let mut requested = false;
        session.converse(&[initialize], false, |message| {
            let id = message.get("id").and_then(Value::as_f64);
            if id == Some(1.0) && !requested {
                requested = true;
                return Ok(Step::Write(vec![
                    json!({ "method": "initialized" }),
                    json!({ "id": 2, "method": "account/rateLimits/read", "params": null }),
                ]));
            }
            if id != Some(2.0) {
                return Ok(Step::Continue);
            }
            if message.get("error").is_some_and(|error| !error.is_null()) {
                return Err(failed("Codex rejected the plan-usage request"));
            }
            Ok(Step::Done(
                message.get("result").cloned().unwrap_or(Value::Null),
            ))
        })
    }
}

fn failed(detail: &str) -> ProbeError {
    ProbeError::Failed(detail.to_owned())
}

/// What the handler of a CLI's message wants next.
enum Step<T> {
    Continue,
    Write(Vec<Value>),
    Done(T),
}

/// One CLI conversation: JSON lines both ways.
struct Session<'a> {
    name: &'a str,
    binary: &'a Path,
    args: &'a [&'a str],
    timeout: Duration,
    max_output: usize,
}

/// Kills and reaps the child however the conversation ends.
struct Reaper(Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Session<'_> {
    /// Spawns the CLI, writes the `opening` lines (then closes its stdin when `close_stdin`), and
    /// hands every JSON line of its output to `on_message` until that answers.
    fn converse<T>(
        &self,
        opening: &[Value],
        close_stdin: bool,
        mut on_message: impl FnMut(&Value) -> Result<Step<T>, ProbeError>,
    ) -> Result<T, ProbeError> {
        let started = Instant::now();
        let name = self.name;
        let child = Command::new(self.binary)
            .args(self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| ProbeError::Failed(format!("could not start {name}")))?;
        let mut child = Reaper(child);
        let mut stdin = child.0.stdin.take();
        let Some(mut stdout) = child.0.stdout.take() else {
            return Err(ProbeError::Failed(format!("could not start {name}")));
        };

        // The reader sends chunks until the pipe closes; it is never joined, so a grandchild that
        // keeps the pipe open after the kill cannot hold this call.
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        write_lines(&mut stdin, opening, name)?;
        if close_stdin {
            stdin = None;
        }

        // `pending` holds the output not yet cut into lines; its first `scanned` bytes hold no line end.
        let mut pending: Vec<u8> = Vec::new();
        let mut scanned = 0usize;
        let mut total = 0usize;
        loop {
            let remaining = self.timeout.saturating_sub(started.elapsed());
            let disconnected = match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    total += chunk.len();
                    if total > self.max_output {
                        return Err(ProbeError::Failed(format!(
                            "{name} plan-usage response was too large"
                        )));
                    }
                    pending.extend_from_slice(&chunk);
                    false
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ProbeError::Failed(format!(
                        "{name} plan-usage read timed out"
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // A last line without its line end still counts.
                    if !pending.is_empty() && pending.last() != Some(&b'\n') {
                        pending.push(b'\n');
                    }
                    true
                }
            };
            while let Some(offset) = pending[scanned..].iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=scanned + offset).collect();
                scanned = 0;
                let Ok(message) = serde_json::from_slice::<Value>(line.trim_ascii()) else {
                    continue;
                };
                match on_message(&message)? {
                    Step::Continue => {}
                    Step::Write(lines) => write_lines(&mut stdin, &lines, name)?,
                    Step::Done(value) => return Ok(value),
                }
            }
            scanned = pending.len();
            if disconnected {
                return Err(ProbeError::Failed(format!(
                    "{name} exited before returning usage"
                )));
            }
        }
    }
}

fn write_lines(
    stdin: &mut Option<ChildStdin>,
    lines: &[Value],
    name: &str,
) -> Result<(), ProbeError> {
    let write_failed = || ProbeError::Failed(format!("could not write to {name}"));
    let pipe = stdin.as_mut().ok_or_else(write_failed)?;
    for line in lines {
        let mut bytes = serde_json::to_vec(line).map_err(|_| write_failed())?;
        bytes.push(b'\n');
        pipe.write_all(&bytes).map_err(|_| write_failed())?;
    }
    pipe.flush().map_err(|_| write_failed())
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Dot-separated parts compared one by one: digit-only parts by number, and before other parts.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = compare_part(x, y);
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

fn compare_part(a: &str, b: &str) -> Ordering {
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    match (numeric(a), numeric(b)) {
        (true, true) => {
            let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
            a.len().cmp(&b.len()).then_with(|| a.cmp(b))
        }
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.cmp(b),
    }
}

// ---------------------------------------------------------------- parsing

fn text(value: Option<&Value>) -> Option<String> {
    let trimmed = value?.as_str()?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn number(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|number| number.is_finite())
}

fn percent(value: Option<&Value>) -> Option<f64> {
    number(value).map(|number| number.clamp(0.0, 100.0))
}

/// Unix time in milliseconds of a number (seconds, or milliseconds from 1e10 on) or an ISO 8601
/// string; a string without an offset is read as UTC.
fn timestamp(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(_) => {
            let number = number(value)?;
            Some(if number < 10_000_000_000.0 {
                number * 1000.0
            } else {
                number
            } as i64)
        }
        Value::String(string) => parse_iso(string),
        _ => None,
    }
}

fn parse_iso(input: &str) -> Option<i64> {
    struct Cursor<'a> {
        bytes: &'a [u8],
        at: usize,
    }
    impl Cursor<'_> {
        fn digits(&mut self, count: usize) -> Option<i64> {
            let part = self.bytes.get(self.at..self.at + count)?;
            if !part.iter().all(u8::is_ascii_digit) {
                return None;
            }
            self.at += count;
            Some(part.iter().fold(0, |sum, d| sum * 10 + i64::from(d - b'0')))
        }
        fn eat(&mut self, byte: u8) -> bool {
            let found = self.bytes.get(self.at) == Some(&byte);
            if found {
                self.at += 1;
            }
            found
        }
    }

    let mut cursor = Cursor {
        bytes: input.as_bytes(),
        at: 0,
    };
    let year = cursor.digits(4)?;
    cursor.eat(b'-').then_some(())?;
    let month = cursor.digits(2)?;
    cursor.eat(b'-').then_some(())?;
    let day = cursor.digits(2)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + i64::from(leap),
        _ => return None,
    };
    if !(1..=month_days).contains(&day) {
        return None;
    }

    let (mut hour, mut minute, mut second, mut millis, mut offset_mins) = (0, 0, 0, 0, 0);
    if cursor.at < cursor.bytes.len() {
        (cursor.eat(b'T') || cursor.eat(b't')).then_some(())?;
        hour = cursor.digits(2)?;
        cursor.eat(b':').then_some(())?;
        minute = cursor.digits(2)?;
        if cursor.eat(b':') {
            second = cursor.digits(2)?;
            if cursor.eat(b'.') || cursor.eat(b',') {
                let start = cursor.at;
                while cursor.bytes.get(cursor.at).is_some_and(u8::is_ascii_digit) {
                    cursor.at += 1;
                }
                if cursor.at == start {
                    return None;
                }
                let fraction = &cursor.bytes[start..cursor.at];
                millis = (0..3).fold(0, |sum, i| {
                    sum * 10 + fraction.get(i).map_or(0, |d| i64::from(d - b'0'))
                });
            }
        }
        if hour > 24 || minute > 59 || second > 59 || (hour == 24 && minute + second + millis > 0) {
            return None;
        }
        if !(cursor.eat(b'Z') || cursor.eat(b'z')) && cursor.at < cursor.bytes.len() {
            let sign = if cursor.eat(b'+') {
                1
            } else if cursor.eat(b'-') {
                -1
            } else {
                return None;
            };
            let zone_hour = cursor.digits(2)?;
            cursor.eat(b':');
            let zone_minute = cursor.digits(2)?;
            if zone_hour > 23 || zone_minute > 59 {
                return None;
            }
            offset_mins = sign * (zone_hour * 60 + zone_minute);
        }
        if cursor.at != cursor.bytes.len() {
            return None;
        }
    }

    // Days since 1970-01-01 of a proleptic Gregorian date.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_mins * 60;
    Some(seconds * 1000 + millis)
}

fn unavailable(
    provider: PlanUsageProviderId,
    label: &str,
    plan: Option<String>,
    message: &str,
) -> ProviderPlanUsage {
    ProviderPlanUsage {
        provider,
        label: label.to_owned(),
        status: PlanUsageStatus::Unavailable,
        plan,
        buckets: Vec::new(),
        message: Some(message.to_owned()),
    }
}

fn codex_window_label(duration: Option<f64>, primary: bool) -> String {
    match duration {
        Some(300.0) => "5-hour limit".to_owned(),
        Some(1440.0) => "Daily limit".to_owned(),
        Some(10080.0) => "Weekly limit".to_owned(),
        Some(d) if d != 0.0 && d % 1440.0 == 0.0 => format!("{}-day limit", (d / 1440.0) as i64),
        Some(d) if d != 0.0 && d % 60.0 == 0.0 => format!("{}-hour limit", (d / 60.0) as i64),
        _ if primary => "Primary limit".to_owned(),
        _ => "Secondary limit".to_owned(),
    }
}

fn codex_window(bucket_id: &str, slot: &str, raw: Option<&Value>) -> Option<PlanUsageWindow> {
    let raw = raw.filter(|raw| raw.is_object())?;
    let used_percent = percent(raw.get("usedPercent"))?;
    let duration = number(raw.get("windowDurationMins"));
    Some(PlanUsageWindow {
        id: format!("{bucket_id}:{slot}"),
        label: codex_window_label(duration, slot == "primary"),
        used_percent,
        resets_at: timestamp(raw.get("resetsAt")),
        window_duration_mins: Some(duration.filter(|d| d.fract() == 0.0).map(|d| d as i64)),
        active: None,
    })
}

/// The `result` of Codex's `account/rateLimits/read` reduced to the provider-neutral shape.
fn parse_codex(payload: &Value) -> ProviderPlanUsage {
    let legacy = payload.get("rateLimits").filter(|v| v.is_object());
    let by_limit = payload
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty());
    let entries: Vec<(&str, &Value)> = match (by_limit, legacy) {
        (Some(map), _) => map
            .iter()
            .map(|(key, value)| (key.as_str(), value))
            .collect(),
        (None, Some(legacy)) => vec![("codex", legacy)],
        (None, None) => Vec::new(),
    };
    let mut plan: Option<String> = None;
    let mut buckets: Vec<PlanUsageBucket> = Vec::new();
    for (key, snapshot) in entries {
        if !snapshot.is_object() {
            continue;
        }
        if plan.is_none() {
            plan = text(snapshot.get("planType"));
        }
        let id = text(snapshot.get("limitId")).unwrap_or_else(|| key.to_owned());
        let windows: Vec<PlanUsageWindow> = [
            codex_window(&id, "primary", snapshot.get("primary")),
            codex_window(&id, "secondary", snapshot.get("secondary")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if windows.is_empty() {
            continue;
        }
        let label = text(snapshot.get("limitName")).unwrap_or_else(|| {
            if id == "codex" {
                CODEX_LABEL.to_owned()
            } else {
                id.clone()
            }
        });
        buckets.push(PlanUsageBucket { id, label, windows });
    }
    if buckets.is_empty() {
        return unavailable(
            PlanUsageProviderId::Codex,
            CODEX_LABEL,
            plan,
            "Codex returned no rolling plan limits for this account.",
        );
    }
    // `codex` first, then by label (case-insensitive, lower case first on a tie).
    buckets.sort_by(|a, b| {
        (b.id == "codex")
            .cmp(&(a.id == "codex"))
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
            .then_with(|| b.label.cmp(&a.label))
    });
    ProviderPlanUsage {
        provider: PlanUsageProviderId::Codex,
        label: CODEX_LABEL.to_owned(),
        status: PlanUsageStatus::Available,
        plan,
        buckets,
        message: None,
    }
}

fn claude_window_label(limit: &Value) -> String {
    let kind = text(limit.get("kind"));
    let model = claude_model(limit);
    match (kind.as_deref(), model) {
        (Some("session"), _) => "Current session".to_owned(),
        (Some("weekly_all"), _) => "Current week".to_owned(),
        (Some("weekly_scoped"), Some(model)) => format!("Current week ({model})"),
        (_, Some(model)) => model,
        (Some(kind), None) => kind.replace('_', " "),
        (None, None) => "Plan limit".to_owned(),
    }
}

fn claude_model(limit: &Value) -> Option<String> {
    text(limit.get("scope")?.get("model")?.get("display_name"))
}

fn claude_window(
    id: String,
    label: String,
    raw: Option<&Value>,
    active: Option<bool>,
) -> Option<PlanUsageWindow> {
    let raw = raw.filter(|raw| raw.is_object())?;
    let used = match raw.get("utilization") {
        None | Some(Value::Null) => raw.get("percent"),
        some => some,
    };
    Some(PlanUsageWindow {
        id,
        label,
        used_percent: percent(used)?,
        resets_at: timestamp(raw.get("resets_at")),
        window_duration_mins: None,
        active,
    })
}

/// The `response.response` of Claude's `get_usage` control answer reduced to the provider-neutral
/// shape; the older named-window shape is the fallback.
fn parse_claude(payload: &Value) -> ProviderPlanUsage {
    let plan = text(payload.get("subscription_type"));
    if payload.get("rate_limits_available") == Some(&Value::Bool(false)) {
        return unavailable(
            PlanUsageProviderId::Claude,
            CLAUDE_LABEL,
            plan,
            "Plan limits are not available for API-key or third-party-provider sessions.",
        );
    }

    let rate_limits = payload.get("rate_limits").filter(|v| v.is_object());
    let mut windows: Vec<PlanUsageWindow> = Vec::new();
    let limits = rate_limits
        .and_then(|r| r.get("limits"))
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for (index, limit) in limits.iter().enumerate() {
        if !limit.is_object() {
            continue;
        }
        let Some(kind) = text(limit.get("kind")) else {
            continue;
        };
        // The rolling plan allowances only: spend and credit records have another unit.
        if !["session", "weekly_all", "weekly_scoped"].contains(&kind.as_str()) {
            continue;
        }
        let model = claude_model(limit);
        let id = format!(
            "claude:{kind}:{}",
            model.clone().unwrap_or_else(|| index.to_string())
        );
        let active = Some(limit.get("is_active") == Some(&Value::Bool(true)));
        windows.extend(claude_window(
            id,
            claude_window_label(limit),
            Some(limit),
            active,
        ));
    }

    if windows.is_empty() {
        if let Some(rate_limits) = rate_limits {
            let named = [
                ("five_hour", "Current session"),
                ("seven_day", "Current week"),
                ("seven_day_opus", "Current week (Opus)"),
                ("seven_day_sonnet", "Current week (Sonnet)"),
            ];
            for (key, label) in named {
                windows.extend(claude_window(
                    format!("claude:{key}"),
                    label.to_owned(),
                    rate_limits.get(key),
                    None,
                ));
            }
            let scoped = rate_limits
                .get("model_scoped")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            for (index, candidate) in scoped.iter().enumerate() {
                let model = text(candidate.get("display_name"));
                windows.extend(claude_window(
                    format!(
                        "claude:model:{}:{index}",
                        model.clone().unwrap_or_else(|| index.to_string())
                    ),
                    format!("Current week ({})", model.as_deref().unwrap_or("model")),
                    Some(candidate),
                    None,
                ));
            }
        }
    }

    if windows.is_empty() {
        return unavailable(
            PlanUsageProviderId::Claude,
            CLAUDE_LABEL,
            plan,
            "Claude Code returned no rolling plan limits for this account.",
        );
    }
    ProviderPlanUsage {
        provider: PlanUsageProviderId::Claude,
        label: CLAUDE_LABEL.to_owned(),
        status: PlanUsageStatus::Available,
        plan,
        buckets: vec![PlanUsageBucket {
            id: "claude".to_owned(),
            label: CLAUDE_LABEL.to_owned(),
            windows,
        }],
        message: None,
    }
}

// ---------------------------------------------------------------- service

/// Turns a probe's answer into the provider's entry; a failure's detail goes to the log only.
fn provider_entry(
    provider: PlanUsageProviderId,
    label: &str,
    answer: Result<Value, ProbeError>,
    parse: fn(&Value) -> ProviderPlanUsage,
) -> ProviderPlanUsage {
    match answer {
        Ok(payload) => parse(&payload),
        Err(ProbeError::NotInstalled) => unavailable(
            provider,
            label,
            None,
            &format!("{label} is not installed on this Mac."),
        ),
        Err(ProbeError::Failed(detail)) => {
            tracing::warn!("{label} plan usage failed: {detail}");
            ProviderPlanUsage {
                provider,
                label: label.to_owned(),
                status: PlanUsageStatus::Error,
                plan: None,
                buckets: Vec::new(),
                message: Some(format!("Could not read plan usage from {label}.")),
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A read in progress: the readers that joined it wait for its snapshot.
struct Flight {
    snapshot: Mutex<Option<PlanUsageSnapshot>>,
    ready: Condvar,
}

impl Flight {
    fn wait(&self) -> PlanUsageSnapshot {
        let mut snapshot = lock(&self.snapshot);
        loop {
            if let Some(done) = snapshot.as_ref() {
                return done.clone();
            }
            snapshot = self
                .ready
                .wait(snapshot)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn finish(&self, snapshot: PlanUsageSnapshot) {
        *lock(&self.snapshot) = Some(snapshot);
        self.ready.notify_all();
    }
}

#[derive(Default)]
struct State {
    cached: Option<(Instant, PlanUsageSnapshot)>,
    flight: Option<Arc<Flight>>,
}

/// Reads the plan usage of every provider and keeps the answer for a while.
pub struct PlanUsageService {
    probe: Arc<dyn PlanProbe>,
    ttl: Duration,
    state: Mutex<State>,
}

impl PlanUsageService {
    /// `with_ttl(probe, 60 s)`.
    pub fn new(probe: Arc<dyn PlanProbe>) -> Self {
        Self::with_ttl(probe, Duration::from_secs(60))
    }

    pub fn with_ttl(probe: Arc<dyn PlanProbe>, ttl: Duration) -> Self {
        Self {
            probe,
            ttl,
            state: Mutex::new(State::default()),
        }
    }

    /// The snapshot: the cached one while it is younger than the ttl, else a fresh read that
    /// concurrent callers share. `force` skips the cache but still joins a read in flight.
    pub fn read(&self, force: bool) -> PlanUsageSnapshot {
        let flight = {
            let mut state = lock(&self.state);
            if !force {
                if let Some((at, snapshot)) = &state.cached {
                    if at.elapsed() < self.ttl {
                        return snapshot.clone();
                    }
                }
            }
            if let Some(flight) = state.flight.clone() {
                drop(state);
                return flight.wait();
            }
            let flight = Arc::new(Flight {
                snapshot: Mutex::new(None),
                ready: Condvar::new(),
            });
            state.flight = Some(Arc::clone(&flight));
            flight
        };

        let snapshot = self.fetch();
        {
            let mut state = lock(&self.state);
            state.cached = Some((Instant::now(), snapshot.clone()));
            state.flight = None;
        }
        flight.finish(snapshot.clone());
        snapshot
    }

    /// Runs the two probes, each on its own thread.
    fn fetch(&self) -> PlanUsageSnapshot {
        let probe: &dyn PlanProbe = &*self.probe;
        let (claude, codex) = std::thread::scope(|scope| {
            let claude = scope.spawn(|| probe.claude());
            let codex = scope.spawn(|| probe.codex());
            (claude.join(), codex.join())
        });
        let panicked = || Err(ProbeError::Failed("the probe panicked".to_owned()));
        let claude = claude.unwrap_or_else(|_| panicked());
        let codex = codex.unwrap_or_else(|_| panicked());
        let unsupported =
            |provider, label: &str, message: &str| unavailable(provider, label, None, message);
        PlanUsageSnapshot {
            providers: vec![
                provider_entry(
                    PlanUsageProviderId::Claude,
                    CLAUDE_LABEL,
                    claude,
                    parse_claude,
                ),
                provider_entry(PlanUsageProviderId::Codex, CODEX_LABEL, codex, parse_codex),
                unsupported(
                    PlanUsageProviderId::Cursor,
                    CURSOR_LABEL,
                    "Cursor Agent does not expose plan limits through its CLI.",
                ),
                unsupported(
                    PlanUsageProviderId::Opencode,
                    OPENCODE_LABEL,
                    "OpenCode reports local token and cost totals, not provider plan limits.",
                ),
            ],
            fetched_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as i64),
        }
    }
}
