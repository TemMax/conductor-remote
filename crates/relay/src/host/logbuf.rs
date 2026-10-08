//! The relay's log: a ring of recent lines and the launchd log files.
//!
//! Nothing here changes what the relay logs; it changes where that output can be read from. A
//! tracing [`layer`] copies every event into a bounded in-memory ring ([`LogBuffer`], served by
//! `GET /api/logs`), and [`tail`] reads the end of the LaunchAgent's stdout / stderr files, which is
//! how "what happened before this process started" (a crash and a KeepAlive respawn) gets read
//! from the phone.
//!
//! The daemon's output contains the access token (the startup banner prints the phone URL with
//! `#token=…`). The whole point of the feature is copy-pasting logs into a bug report, so every
//! line that enters the ring goes through [`Redactor`] first.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Kept small enough that a phone on a slow link can pull the whole thing; about a day of a quiet relay.
pub const MAX_ENTRIES: usize = 600;
/// One long failure (a stack, an AppleScript error) must not crowd out the rest: a line is clipped to this many characters.
pub const MAX_TEXT: usize = 4000;
/// Read window for a file tail: enough for a few restarts, small enough to read per request.
pub const TAIL_BYTES: u64 = 256 * 1024;

/// What a clipped line ends with; it counts towards [`MAX_TEXT`].
const TRUNCATED: &str = "… [truncated]";
/// What a secret is replaced with.
const REDACTED: &str = "[redacted]";

/// One line of the relay's log as the phone reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Epoch milliseconds.
    pub t: i64,
    /// `"info"`, `"warn"` or `"error"`.
    pub level: &'static str,
    pub text: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// `text` cut to at most [`MAX_TEXT`] characters, ending with [`TRUNCATED`] when anything was cut.
fn clip(text: String) -> String {
    if text.chars().count() <= MAX_TEXT {
        return text;
    }
    let keep = MAX_TEXT - TRUNCATED.chars().count();
    let mut clipped: String = text.chars().take(keep).collect();
    clipped.push_str(TRUNCATED);
    clipped
}

struct Ring {
    started_at: i64,
    entries: Mutex<VecDeque<Entry>>,
}

/// The recent log, shared: clones see the same ring. Holds at most [`MAX_ENTRIES`] lines, dropping the oldest first.
#[derive(Clone)]
pub struct LogBuffer {
    ring: Arc<Ring>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            ring: Arc::new(Ring {
                started_at: now_ms(),
                entries: Mutex::new(VecDeque::with_capacity(MAX_ENTRIES)),
            }),
        }
    }

    /// Append one line stamped now, clipped to [`MAX_TEXT`] characters.
    pub fn push(&self, level: &'static str, text: impl Into<String>) {
        let entry = Entry {
            t: now_ms(),
            level,
            text: clip(text.into()),
        };
        let mut entries = self
            .ring
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while entries.len() >= MAX_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// The newest `limit` lines, oldest first (newest last).
    pub fn entries(&self, limit: usize) -> Vec<Entry> {
        let entries = self
            .ring
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let skip = entries.len().saturating_sub(limit);
        entries.iter().skip(skip).cloned().collect()
    }

    /// When this buffer (in practice, the relay process) started, in epoch milliseconds: the phone's
    /// cue that a restart, not a network blip, ate the history.
    pub fn started_at(&self) -> i64 {
        self.ring.started_at
    }
}

/// Strips secrets out of a line before it can leave the machine. Shared: clones see the same token.
///
/// The tracing layer is installed before the access token is loaded, so the token is set later
/// with [`Redactor::set_token`]; until then (and for an empty token) only the patterns apply.
#[derive(Clone, Default)]
pub struct Redactor {
    token: Arc<RwLock<String>>,
}

impl Redactor {
    /// A redactor that knows no token yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The access token to mask from now on. An empty token masks nothing.
    pub fn set_token(&self, token: &str) {
        let mut current = self.token.write().unwrap_or_else(PoisonError::into_inner);
        current.clear();
        current.push_str(token);
    }

    /// `text` with the access token, `sk-…` keys, `whsec_…` secrets and the value of any
    /// `token=…` (a rotated token in an older line, a URL) replaced by `[redacted]`.
    pub fn redact(&self, text: &str) -> String {
        let masked = {
            let token = self.token.read().unwrap_or_else(PoisonError::into_inner);
            if token.is_empty() {
                text.to_owned()
            } else {
                text.replace(token.as_str(), REDACTED)
            }
        };
        let masked = mask_prefixed(&masked, "sk-", is_key_char, 8);
        let masked = mask_prefixed(&masked, "whsec_", is_secret_char, 8);
        mask_token_values(&masked)
    }
}

/// A word character as a regex `\b` sees it.
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The body of an `sk-` key: `[A-Za-z0-9_-]`.
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The body of a `whsec_` secret: `[A-Za-z0-9+/=_-]`.
fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-')
}

/// Replace every `\b<prefix><body>{min,}` (the body run taken greedily) with [`REDACTED`].
fn mask_prefixed(text: &str, prefix: &str, body: fn(char) -> bool, min: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        let tail = &text[i..];
        let at_boundary = !text[..i].chars().next_back().is_some_and(is_word);
        if at_boundary && tail.starts_with(prefix) {
            // Body characters are ASCII, so the run's length in bytes is its length in characters.
            let run: usize = tail[prefix.len()..]
                .chars()
                .take_while(|&c| body(c))
                .map(char::len_utf8)
                .sum();
            if run >= min {
                out.push_str(REDACTED);
                i += prefix.len() + run;
                continue;
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Where a `token=` value ends: whitespace, `&`, or a quote.
fn ends_token_value(c: char) -> bool {
    c.is_whitespace() || matches!(c, '&' | '"' | '\'' | '`')
}

/// Replace the value after every `token=` (any case) with [`REDACTED`], keeping the `token=` itself.
fn mask_token_values(text: &str) -> String {
    const KEY: &[u8] = b"token=";
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        let tail = &text[i..];
        let bytes = tail.as_bytes();
        if bytes.len() > KEY.len() && bytes[..KEY.len()].eq_ignore_ascii_case(KEY) {
            let run: usize = tail[KEY.len()..]
                .chars()
                .take_while(|&c| !ends_token_value(c))
                .map(char::len_utf8)
                .sum();
            if run > 0 {
                out.push_str(&tail[..KEY.len()]);
                out.push_str(REDACTED);
                i += KEY.len() + run;
                continue;
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A tracing layer that copies every event into `buffer`: the message and its fields on one line,
/// redacted, clipped to [`MAX_TEXT`] characters. It only formats into memory and takes the ring's
/// lock for one push, so it never waits on I/O. ERROR reads as `"error"`, WARN as `"warn"`, every
/// other level as `"info"`; filtering is the caller's (`LevelFilter::INFO` on the registry).
pub fn layer<S: Subscriber>(buffer: LogBuffer, redactor: Redactor) -> impl Layer<S> {
    BufferLayer { buffer, redactor }
}

struct BufferLayer {
    buffer: LogBuffer,
    redactor: Redactor,
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = match *event.metadata().level() {
            Level::ERROR => "error",
            Level::WARN => "warn",
            _ => "info",
        };
        let mut line = Line::default();
        event.record(&mut line);
        self.buffer
            .push(level, self.redactor.redact(&line.finish()));
    }
}

/// Collects an event as `message key=value key=value`.
#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Line {
    fn field(&mut self, name: &str, value: fmt::Arguments<'_>) {
        if name == "message" {
            let _ = self.message.write_fmt(value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{name}={value}");
        }
    }

    fn finish(self) -> String {
        match (self.message.is_empty(), self.fields.is_empty()) {
            (_, true) => self.message,
            (true, false) => self.fields,
            (false, false) => format!("{} {}", self.message, self.fields),
        }
    }
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.field(field.name(), format_args!("{value}"));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.field(field.name(), format_args!("{value}"));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.field(field.name(), format_args!("{value:?}"));
    }
}

/// The last `limit` lines within the last [`TAIL_BYTES`] of `path`, oldest first. When the window
/// starts mid-file, its leading partial line is dropped (which also drops a split UTF-8 character).
/// A missing file is an empty list.
pub fn tail(path: &Path, limit: usize) -> io::Result<Vec<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let size = file.metadata()?.len();
    let start = size.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = match text.find('\n') {
        Some(newline) if start > 0 => &text[newline + 1..],
        _ => &text[..],
    };
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(limit);
    Ok(lines[skip..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect())
}
