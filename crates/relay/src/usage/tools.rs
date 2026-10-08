//! Tool usage: how many calls and tokens each tool cost, from the saved traffic.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::db::{ConductorDb, DbError};
use crate::transcript::context::{block_bytes, is_root, message_blocks, parse_frame};

/// The window a tool-usage snapshot covers: the web app's `ToolUsageRange`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ToolRange {
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
}

impl ToolRange {
    /// `None` or `24h` is `Day`, `7d` is `Week`, `30d` is `Month`, anything else `Err(())`.
    #[allow(clippy::result_unit_err)]
    pub fn parse(value: Option<&str>) -> Result<Self, ()> {
        match value {
            None | Some("24h") => Ok(Self::Day),
            Some("7d") => Ok(Self::Week),
            Some("30d") => Ok(Self::Month),
            Some(_) => Err(()),
        }
    }
}

/// The web app's `ToolUsageRow`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUsageRow {
    /// `None` (JSON `null`) when Conductor saved a result without its matching call.
    pub name: Option<String>,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub largest_call_tokens: u64,
}

/// The web app's `ToolUsageProvider`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUsageProvider {
    pub provider: String,
    pub session_count: u64,
    pub tools: Vec<ToolUsageRow>,
}

/// The web app's `ToolUsageSnapshot`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUsageSnapshot {
    pub range: ToolRange,
    /// UTC ISO timestamp where the saved traffic considered starts.
    pub since: String,
    /// UTC ISO timestamp where it ends.
    pub until: String,
    /// When the snapshot was made, as Unix time in milliseconds.
    pub fetched_at: i64,
    pub providers: Vec<ToolUsageProvider>,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolUsageError {
    #[error("the tool usage scan timed out")]
    TimedOut,
    #[error("the tool usage scan failed: {0}")]
    Read(String),
}

const DAY_MS: i64 = 86_400_000;
const BYTES_PER_TOKEN: u64 = 4;

/// Every chat touched since the start of the window. Hidden and archived chats are included.
const SESSIONS_SQL: &str = "\
SELECT id, agent_type FROM sessions WHERE julianday(updated_at) >= julianday(?)";

/// The rows of one chat that can matter: calls up to `until` (an earlier call names a later
/// result), and results from `since` on. `in_range` marks the rows whose bytes are counted.
/// `julianday` reads both of Conductor's timestamp formats. Parameters: `since`, the chat's
/// id, `until`, `since`.
const MESSAGES_SQL: &str = "\
SELECT rowid, role, content, julianday(created_at) >= julianday(?) AS in_range
 FROM session_messages
 WHERE session_id = ? AND julianday(created_at) <= julianday(?)
   AND (instr(content, '\"tool_use\"') > 0
        OR (julianday(created_at) >= julianday(?) AND instr(content, '\"tool_result\"') > 0))
 ORDER BY rowid ASC";

/// One tool call of one chat: its name and the largest saved size of its input and its output.
#[derive(Default)]
struct Call {
    name: Option<String>,
    input_bytes: u64,
    output_bytes: u64,
}

/// Joins the calls and the results of one chat, never of two chats that reuse a call id.
///
/// An earlier call gives a name only, so a result crossing the start of the window still has
/// an owner. Repeated snapshots of a call or a result count once, at their largest saved
/// size. Nothing but sizes and names leaves it.
#[derive(Default)]
struct ToolAccumulator {
    calls: HashMap<String, Call>,
}

impl ToolAccumulator {
    fn add(&mut self, rowid: i64, content: Option<&str>, in_range: bool) {
        let Some(frame) = parse_frame(content.unwrap_or("")) else {
            return;
        };
        if !is_root(&frame) {
            return;
        }
        let Some(blocks) = message_blocks(&frame) else {
            return;
        };
        for (index, block) in blocks.iter().enumerate() {
            if !block.is_object() {
                continue;
            }
            let kind = block.get("type").and_then(Value::as_str);
            let is_call = kind == Some("tool_use");
            if !is_call && (kind != Some("tool_result") || !in_range) {
                continue;
            }
            let id_field = if is_call { "id" } else { "tool_use_id" };
            let key = match block.get(id_field).and_then(Value::as_str) {
                Some(id) if !id.is_empty() => format!("id:{id}"),
                _ => format!("row:{rowid}:{index}"),
            };
            let call = self.calls.entry(key).or_default();
            if is_call {
                if let Some(name) = block
                    .get("name")
                    .and_then(Value::as_str)
                    .map(trim_js)
                    .filter(|name| !name.is_empty())
                {
                    call.name = Some(name.to_owned());
                }
            }
            if in_range {
                let bytes = u64::try_from(block_bytes(block)).unwrap_or(0);
                let slot = if is_call {
                    &mut call.input_bytes
                } else {
                    &mut call.output_bytes
                };
                *slot = (*slot).max(bytes);
            }
        }
    }

    /// The chat's tools by name, largest first. A call with no bytes in range is left out.
    fn tools(&self) -> Vec<ToolUsageRow> {
        let mut tools = HashMap::new();
        for call in self.calls.values() {
            if call.input_bytes == 0 && call.output_bytes == 0 {
                continue;
            }
            let input_tokens = call.input_bytes.div_ceil(BYTES_PER_TOKEN);
            let output_tokens = call.output_bytes.div_ceil(BYTES_PER_TOKEN);
            merge(
                &mut tools,
                ToolUsageRow {
                    name: call.name.clone(),
                    calls: 1,
                    input_tokens,
                    output_tokens,
                    total_tokens: input_tokens + output_tokens,
                    largest_call_tokens: input_tokens + output_tokens,
                },
            );
        }
        sorted(tools)
    }
}

/// `String.prototype.trim`: JavaScript's whitespace set, which is not `char::is_whitespace`
/// (it has U+FEFF and not U+0085).
fn trim_js(text: &str) -> &str {
    text.trim_matches(|c| {
        matches!(
            c,
            '\u{0009}'..='\u{000D}'
                | '\u{0020}'
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    })
}

/// Adds `row` to the tool of its name: counts and tokens add up, the largest call is kept.
fn merge(tools: &mut HashMap<Option<String>, ToolUsageRow>, row: ToolUsageRow) {
    match tools.get_mut(&row.name) {
        Some(previous) => {
            previous.calls += row.calls;
            previous.input_tokens += row.input_tokens;
            previous.output_tokens += row.output_tokens;
            previous.total_tokens += row.total_tokens;
            previous.largest_call_tokens =
                previous.largest_call_tokens.max(row.largest_call_tokens);
        }
        None => {
            tools.insert(row.name.clone(), row);
        }
    }
}

/// Total tokens descending, then name (a call without one first).
fn sorted(tools: HashMap<Option<String>, ToolUsageRow>) -> Vec<ToolUsageRow> {
    let mut rows: Vec<ToolUsageRow> = tools.into_values().collect();
    rows.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// What the tools of the chats of one provider add up to.
#[derive(Default)]
struct ProviderGroup {
    session_count: u64,
    tools: HashMap<Option<String>, ToolUsageRow>,
}

fn range_days(range: ToolRange) -> i64 {
    match range {
        ToolRange::Day => 1,
        ToolRange::Week => 7,
        ToolRange::Month => 30,
    }
}

/// `new Date(ms).toISOString()`: `YYYY-MM-DDTHH:MM:SS.mmmZ`, in UTC.
fn iso_millis(ms: i64) -> String {
    let days = ms.div_euclid(DAY_MS);
    let in_day = ms.rem_euclid(DAY_MS);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    )
}

/// The proleptic Gregorian date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn read_error(error: DbError) -> ToolUsageError {
    ToolUsageError::Read(error.to_string())
}

/// One scan of the saved traffic for a range, on `db`. Checks `deadline` before each chat.
fn scan(
    db: &ConductorDb,
    range: ToolRange,
    now: i64,
    deadline: Option<Instant>,
) -> Result<ToolUsageSnapshot, ToolUsageError> {
    let since = iso_millis(now - range_days(range) * DAY_MS);
    let until = iso_millis(now);
    let sessions = db
        .read("usage.tools.sessions", |conn| {
            let mut stmt = conn.prepare(SESSIONS_SQL)?;
            let rows = stmt.query_map([&since], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(read_error)?;

    let mut providers: BTreeMap<String, ProviderGroup> = BTreeMap::new();
    for (id, agent_type) in sessions {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ToolUsageError::TimedOut);
        }
        // A new accumulator for every chat: call ids are not unique across chats.
        let mut accumulator = ToolAccumulator::default();
        db.read("usage.tools.messages", |conn| {
            let mut stmt = conn.prepare(MESSAGES_SQL)?;
            let mut rows = stmt.query(rusqlite::params![since, id, until, since])?;
            while let Some(row) = rows.next()? {
                let rowid: i64 = row.get(0)?;
                let content: Option<String> = row.get(2)?;
                let in_range = row.get::<_, Option<i64>>(3)?.is_some_and(|flag| flag != 0);
                accumulator.add(rowid, content.as_deref(), in_range);
            }
            Ok(())
        })
        .map_err(read_error)?;
        let tools = accumulator.tools();
        if tools.is_empty() {
            continue;
        }
        let provider = match agent_type.as_deref() {
            Some("acp") => "opencode".to_owned(),
            Some(other) => other.to_owned(),
            None => "unknown".to_owned(),
        };
        let group = providers.entry(provider).or_default();
        group.session_count += 1;
        for tool in tools {
            merge(&mut group.tools, tool);
        }
    }
    Ok(ToolUsageSnapshot {
        range,
        since,
        until,
        fetched_at: now,
        providers: providers
            .into_iter()
            .map(|(provider, group)| ToolUsageProvider {
                provider,
                session_count: group.session_count,
                tools: sorted(group.tools),
            })
            .collect(),
    })
}

/// A scan that is under way, or queued, for one range. Readers of the range wait on it.
#[derive(Default)]
struct Flight {
    outcome: Mutex<Option<Result<ToolUsageSnapshot, ToolUsageError>>>,
    done: Condvar,
}

impl Flight {
    fn publish(&self, outcome: Result<ToolUsageSnapshot, ToolUsageError>) {
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        self.done.notify_all();
    }

    fn wait(&self) -> Result<ToolUsageSnapshot, ToolUsageError> {
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(outcome) = outcome.as_ref() {
                return duplicate(outcome);
            }
            outcome = self.done.wait(outcome).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// A copy of an outcome: the error type is not `Clone`.
fn duplicate(
    outcome: &Result<ToolUsageSnapshot, ToolUsageError>,
) -> Result<ToolUsageSnapshot, ToolUsageError> {
    match outcome {
        Ok(snapshot) => Ok(snapshot.clone()),
        Err(ToolUsageError::TimedOut) => Err(ToolUsageError::TimedOut),
        Err(ToolUsageError::Read(message)) => Err(ToolUsageError::Read(message.clone())),
    }
}

/// A snapshot and when its scan started.
struct Kept {
    started: Instant,
    snapshot: ToolUsageSnapshot,
}

/// Per range, indexed by `slot`.
#[derive(Default)]
struct State {
    kept: [Option<Kept>; 3],
    flights: [Option<Arc<Flight>>; 3],
}

fn slot(range: ToolRange) -> usize {
    match range {
        ToolRange::Day => 0,
        ToolRange::Week => 1,
        ToolRange::Month => 2,
    }
}

/// Scans the saved traffic for tool usage and keeps the answer for a while.
///
/// The history is large, so the service reads it through a handle of its own and never
/// holds the shared one. One scan runs at a time; a snapshot of a range is reused for `ttl`;
/// reads of a range that arrive while its scan is under way wait for that scan.
pub struct ToolUsageService {
    db: ConductorDb,
    ttl: Duration,
    timeout: Duration,
    state: Mutex<State>,
    /// Held for the length of a scan.
    scanning: Mutex<()>,
}

impl ToolUsageService {
    /// `with_limits(db_path, 60 s, 60 s)`.
    pub fn new(db_path: PathBuf) -> Self {
        Self::with_limits(db_path, Duration::from_secs(60), Duration::from_secs(60))
    }

    /// `ttl` keeps a snapshot; `timeout` bounds one scan.
    pub fn with_limits(db_path: PathBuf, ttl: Duration, timeout: Duration) -> Self {
        Self {
            db: ConductorDb::new(db_path),
            ttl,
            timeout,
            state: Mutex::new(State::default()),
            scanning: Mutex::new(()),
        }
    }

    /// The snapshot of `range`.
    ///
    /// A scan of the range that is under way or queued is joined, `force` or not. Otherwise
    /// a snapshot younger than the ttl is returned unless `force` is set, and a new scan is
    /// queued behind any other. An error is returned to everyone who waited for that scan
    /// and is not kept.
    pub fn read(&self, range: ToolRange, force: bool) -> Result<ToolUsageSnapshot, ToolUsageError> {
        let flight = {
            let mut state = self.lock_state();
            if let Some(running) = &state.flights[slot(range)] {
                let running = Arc::clone(running);
                drop(state);
                return running.wait();
            }
            if !force {
                if let Some(kept) = &state.kept[slot(range)] {
                    if kept.started.elapsed() < self.ttl {
                        return Ok(kept.snapshot.clone());
                    }
                }
            }
            let flight = Arc::new(Flight::default());
            state.flights[slot(range)] = Some(Arc::clone(&flight));
            flight
        };
        let mut lead = Lead {
            service: self,
            range,
            flight,
            finished: false,
        };
        let (started, outcome) = {
            let _alone = self.scanning.lock().unwrap_or_else(|e| e.into_inner());
            // The deadline runs from here, not from the queue.
            let started = Instant::now();
            let deadline = started.checked_add(self.timeout);
            let outcome = scan(&self.db, range, unix_millis(), deadline);
            // The handle is not kept between scans.
            self.db.close();
            (started, outcome)
        };
        lead.finish(started, &outcome);
        outcome
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The reader that runs the scan of a range. However it ends, the range stops being under way
/// and the readers waiting on it get an answer.
struct Lead<'a> {
    service: &'a ToolUsageService,
    range: ToolRange,
    flight: Arc<Flight>,
    finished: bool,
}

impl Lead<'_> {
    fn finish(&mut self, started: Instant, outcome: &Result<ToolUsageSnapshot, ToolUsageError>) {
        self.finished = true;
        {
            let mut state = self.service.lock_state();
            if let Ok(snapshot) = outcome {
                state.kept[slot(self.range)] = Some(Kept {
                    started,
                    snapshot: snapshot.clone(),
                });
            }
            state.flights[slot(self.range)] = None;
        }
        self.flight.publish(duplicate(outcome));
    }
}

impl Drop for Lead<'_> {
    /// The scan panicked: nothing is kept and the waiting readers fail.
    fn drop(&mut self) {
        if !self.finished {
            let failed = Err(ToolUsageError::Read(
                "the tool usage scan stopped unexpectedly".to_owned(),
            ));
            self.finish(Instant::now(), &failed);
        }
    }
}
