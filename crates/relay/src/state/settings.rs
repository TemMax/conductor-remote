//! The relay's persisted settings.
//!
//! Every setting has one name, used both as the environment variable and as the key in
//! `<state_dir>/settings.json`. A value comes from the environment first, then the file, then the
//! default; an empty value counts as unset. Values are validated the same way wherever they come
//! from, and are resolved once, at startup.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::contract::DEFAULT_PORT;

const FILE_NAME: &str = "settings.json";
const PUBLIC_REFUSED: &str =
    "public mode is not supported; the relay is reachable on your tailnet only";

/// The relay's settings, as resolved: environment first, then `<state_dir>/settings.json`, then
/// the default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `RELAY_PORT`, default 8790, 1..=65535.
    pub port: u16,
    /// `EXPOSE`: tailnet (default) or off.
    pub expose: Expose,
    /// `PREVENT_SCREEN_LOCK`: on or off, default off.
    pub prevent_screen_lock: bool,
    /// `PUSH_NOTIFY`: off, false or 0 turn it off; default on.
    pub push_notify: bool,
    /// `PUSH_SUBJECT`.
    pub push_subject: Option<String>,
    /// `CONDUCTOR_DB`.
    pub conductor_db: Option<PathBuf>,
    /// `CONDUCTOR_WORKSPACES`.
    pub conductor_workspaces: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expose {
    Tailnet,
    Off,
}

/// Where a value came from, for `conductor-remote config`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Environment,
    File,
    Default,
}

/// One resolved setting: its name, the value as shown, and where it came from.
pub type Row = (&'static str, String, Source);

pub const NAMES: [&str; 7] = [
    "RELAY_PORT",
    "EXPOSE",
    "PREVENT_SCREEN_LOCK",
    "PUSH_NOTIFY",
    "PUSH_SUBJECT",
    "CONDUCTOR_DB",
    "CONDUCTOR_WORKSPACES",
];

/// The value shown, and parsed back, for a setting nobody set. An empty one means "none".
fn default_value(name: &str) -> String {
    match name {
        "RELAY_PORT" => DEFAULT_PORT.to_string(),
        "EXPOSE" => "tailnet".to_owned(),
        "PREVENT_SCREEN_LOCK" => "off".to_owned(),
        "PUSH_NOTIFY" => "on".to_owned(),
        _ => String::new(),
    }
}

/// The canonical form of `raw` for `name`, or the rule it breaks.
fn normalize(name: &str, raw: &str) -> Result<String, String> {
    let value = raw.trim();
    let lower = value.to_lowercase();
    let broken = |rule: &str| Err(format!("{name}: {rule}"));
    match name {
        "RELAY_PORT" => match value.parse::<u16>() {
            Ok(port) if port != 0 => Ok(port.to_string()),
            _ => broken("must be a whole number from 1 to 65535"),
        },
        "EXPOSE" => match lower.as_str() {
            "tailnet" => Ok(lower),
            "off" => Ok(lower),
            "public" | "funnel" => broken(PUBLIC_REFUSED),
            _ => broken("must be tailnet or off"),
        },
        "PREVENT_SCREEN_LOCK" => match lower.as_str() {
            "on" | "off" => Ok(lower),
            _ => broken("must be on or off"),
        },
        "PUSH_NOTIFY" => match lower.as_str() {
            "on" | "true" | "1" => Ok("on".to_owned()),
            "off" | "false" | "0" => Ok("off".to_owned()),
            _ => broken("must be on, off, true, false, 1 or 0"),
        },
        "PUSH_SUBJECT" | "CONDUCTOR_DB" | "CONDUCTOR_WORKSPACES" => Ok(value.to_owned()),
        _ => Err(format!("{name}: not a setting of the relay")),
    }
}

/// Resolves every setting; `env` is injected (`std::env::var` in production).
///
/// Also returns, for every name in [`NAMES`] and in that order, the resolved value as shown to
/// the user and where it came from. An invalid value is an error naming the setting, wherever it
/// came from; it never falls through to the next source.
pub fn resolve(
    state_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Settings, Vec<Row>), String> {
    let file = read_file(state_dir)?;
    let mut rows = Vec::with_capacity(NAMES.len());
    for name in NAMES {
        let from_env = env(name).filter(|value| !value.trim().is_empty());
        let from_file = file
            .get(name)
            .map(file_text)
            .transpose()
            .map_err(|rule| format!("{FILE_NAME}: {name}: {rule}"))?;
        let from_file = from_file.filter(|value| !value.trim().is_empty());
        let (value, source) = match (from_env, from_file) {
            (Some(value), _) => (normalize(name, &value)?, Source::Environment),
            (None, Some(value)) => (
                normalize(name, &value).map_err(|error| format!("{FILE_NAME}: {error}"))?,
                Source::File,
            ),
            (None, None) => (default_value(name), Source::Default),
        };
        rows.push((name, value, source));
    }
    Ok((settings_from(&rows), rows))
}

fn settings_from(rows: &[Row]) -> Settings {
    let value = |name: &str| {
        rows.iter()
            .find(|(row, _, _)| *row == name)
            .map(|(_, value, _)| value.as_str())
            .unwrap_or_default()
    };
    let some = |name: &str| Some(value(name)).filter(|v| !v.is_empty());
    Settings {
        port: value("RELAY_PORT").parse().unwrap_or(DEFAULT_PORT),
        expose: match value("EXPOSE") {
            "off" => Expose::Off,
            _ => Expose::Tailnet,
        },
        prevent_screen_lock: value("PREVENT_SCREEN_LOCK") == "on",
        push_notify: value("PUSH_NOTIFY") != "off",
        push_subject: some("PUSH_SUBJECT").map(str::to_owned),
        conductor_db: some("CONDUCTOR_DB").map(PathBuf::from),
        conductor_workspaces: some("CONDUCTOR_WORKSPACES").map(PathBuf::from),
    }
}

/// Validates and writes one setting into `settings.json` (mode 0600, written atomically through a
/// temporary file and rename); an empty value removes it. Errors name the setting and the rule.
pub fn set(state_dir: &Path, name: &str, value: &str) -> Result<(), String> {
    if !NAMES.contains(&name) {
        return Err(format!(
            "{name}: not a setting of the relay; the settings are {}",
            NAMES.join(", ")
        ));
    }
    let mut entries = read_file(state_dir)?;
    if value.trim().is_empty() {
        if entries.remove(name).is_none() {
            return Ok(());
        }
    } else {
        entries.insert(name.to_owned(), Value::String(normalize(name, value)?));
    }
    write_file(state_dir, &entries).map_err(|error| {
        format!(
            "{name}: could not write {}: {error}",
            state_dir.join(FILE_NAME).display()
        )
    })
}

/// The text of a value in `settings.json`: a string, or a number or boolean written by hand.
fn file_text(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        _ => Err("must be a string".to_owned()),
    }
}

/// The file's object; a missing file is an empty one.
fn read_file(state_dir: &Path) -> Result<Map<String, Value>, String> {
    let path = state_dir.join(FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(entries)) => Ok(entries),
        Ok(_) => Err(format!("{} must hold a JSON object", path.display())),
        Err(error) => Err(format!("{} is not valid JSON: {error}", path.display())),
    }
}

fn write_file(state_dir: &Path, entries: &Map<String, Value>) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)?;
    let mut text = serde_json::to_string_pretty(entries).map_err(io::Error::other)?;
    text.push('\n');

    let temporary = state_dir.join(format!("{FILE_NAME}.{}.tmp", std::process::id()));
    // A leftover from an earlier crash of this process id may carry any mode.
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, state_dir.join(FILE_NAME))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}
