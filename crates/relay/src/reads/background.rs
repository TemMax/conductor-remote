//! The fold of background-task frames into the state of each task.
//!
//! A `Bash` call with `run_in_background` (or a background subagent) is a task: the SDK writes a
//! `task_started` frame, the turn ends and the chat reads `idle`. Only a `task_notification` frame
//! or the death of the agent process that owns the task closes the wait. The process gate is the
//! `process_started_at_ms` argument: a task started before the current process belonged to one that
//! is gone, and its notification will never be written.

use serde_json::Value;

use crate::reads::sessions::BackgroundTask;

/// One row of the task-frame query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskFrameRow {
    pub created_at: String,
    pub content: String,
}

/// A stored timestamp as milliseconds since the epoch: `YYYY-MM-DD HH:MM:SS[.fff]` is UTC;
/// an ISO form with `T` and `Z` or an offset is read as written. `None` when it is neither.
pub fn timestamp_ms(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    let number = |from: usize, len: usize| -> Option<i64> {
        let part = bytes.get(from..from.checked_add(len)?)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(
            part.iter()
                .fold(0i64, |acc, d| acc * 10 + i64::from(d - b'0')),
        )
    };
    let punct = |at: usize, byte: u8| (bytes.get(at) == Some(&byte)).then_some(());

    let year = number(0, 4)?;
    punct(4, b'-')?;
    let month = number(5, 2)?;
    punct(7, b'-')?;
    let day = number(8, 2)?;
    let separator = *bytes.get(10)?;
    if separator != b' ' && separator != b'T' {
        return None;
    }
    let hour = number(11, 2)?;
    punct(13, b':')?;
    let minute = number(14, 2)?;
    punct(16, b':')?;
    let second = number(17, 2)?;

    let mut end = 19;
    let mut millis = 0i64;
    if bytes.get(end) == Some(&b'.') {
        let start = end + 1;
        end = start;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == start {
            return None;
        }
        // Digits beyond the third are dropped.
        let mut scale = 100;
        for digit in &bytes[start..end.min(start + 3)] {
            millis += i64::from(digit - b'0') * scale;
            scale /= 10;
        }
    }

    // Everything before `end` is ASCII, so it is a character boundary.
    let zone = &value[end..];
    let offset_seconds = match (separator, zone) {
        (b' ', "") | (b'T', "Z") => 0,
        (b'T', zone) => zone_offset_seconds(zone)?,
        _ => return None,
    };

    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
        - offset_seconds;
    Some(seconds * 1_000 + millis)
}

/// `+HH:MM`, `-HH:MM`, `+HHMM` or `-HHMM` as seconds east of UTC.
fn zone_offset_seconds(zone: &str) -> Option<i64> {
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = &zone[1..];
    let (hours, minutes) = match digits.len() {
        5 if digits.as_bytes()[2] == b':' => (&digits[..2], &digits[3..]),
        4 => (&digits[..2], &digits[2..]),
        _ => return None,
    };
    let two = |part: &str| -> Option<i64> {
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let (hours, minutes) = (two(hours)?, two(minutes)?);
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3_600 + minutes * 60))
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a civil date (proleptic Gregorian calendar).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// A non-blank string, trimmed.
fn text(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The state of the tasks of one chat, fed the task frames oldest first, one at a time. The fold
/// can be kept and fed the frames that arrive later. `process_started_at_ms` is when the agent
/// process of the chat came up: a task started before it (or at a time that cannot be read)
/// belongs to a process that is gone.
#[derive(Clone, Debug)]
pub struct BackgroundFold {
    process_started_at_ms: i64,
    open: Vec<BackgroundTask>,
}

impl BackgroundFold {
    pub fn new(process_started_at_ms: i64) -> Self {
        Self {
            process_started_at_ms,
            open: Vec::new(),
        }
    }

    /// Applies one frame.
    pub fn push(&mut self, row: &TaskFrameRow) {
        let Ok(frame) = serde_json::from_str::<Value>(&row.content) else {
            return;
        };
        if frame.get("type").and_then(Value::as_str) != Some("system") {
            return;
        }
        let Some(task_id) = text(frame.get("task_id")) else {
            return;
        };
        match frame.get("subtype").and_then(Value::as_str) {
            Some("task_notification") => self.open.retain(|task| task.task_id != task_id),
            Some("task_started") => {
                match timestamp_ms(&row.created_at) {
                    Some(at) if at >= self.process_started_at_ms => {}
                    _ => return,
                }
                let task_type = text(frame.get("task_type")).unwrap_or_else(|| "task".to_owned());
                let task = BackgroundTask {
                    task_id: task_id.clone(),
                    tool_use_id: text(frame.get("tool_use_id")),
                    description: text(frame.get("description"))
                        .unwrap_or_else(|| task_type.clone()),
                    task_type,
                    since: row.created_at.clone(),
                };
                match self.open.iter_mut().find(|known| known.task_id == task_id) {
                    Some(known) => *known = task,
                    None => self.open.push(task),
                }
            }
            _ => {}
        }
    }

    /// The tasks still open, in the order they were first started.
    pub fn tasks(&self) -> Vec<BackgroundTask> {
        self.open.clone()
    }
}

/// Folds the task frames of a chat, oldest first, into the tasks still open, in the order they
/// were first started. `process_started_at_ms` is when the agent process of the chat came up: a
/// task started before it (or at a time that cannot be read) belongs to a process that is gone.
pub fn open_background_tasks(
    rows: &[TaskFrameRow],
    process_started_at_ms: i64,
) -> Vec<BackgroundTask> {
    let mut fold = BackgroundFold::new(process_started_at_ms);
    for row in rows {
        fold.push(row);
    }
    fold.tasks()
}
