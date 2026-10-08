//! The model catalogue and the default effort of each model.
//!
//! Both are plain files next to Conductor: the catalogue the relay keeps in its state directory and
//! Conductor's own settings file, of which only the two default efforts are read.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use serde_json::{Map, Number, Value};

use super::Reads;

/// Serializes the read-modify-write of `model-cache.json` within the process.
static CACHE_LOCK: Mutex<()> = Mutex::new(());

/// A model picker the relay has seen, as the phone app's `CachedModelGroup`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedModelGroup {
    pub agent_type: String,
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// `None` when the key is absent, `Some(None)` for `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_at: Option<Option<Number>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selections: Option<Vec<ModelSelection>>,
    pub updated_at: Number,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub model: String,
    pub selected_at: Number,
}

/// `GET /api/models`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub groups: Vec<CachedModelGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

/// The default efforts of Conductor's settings; an absent one is `null`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DefaultEfforts {
    pub claude: Option<String>,
    pub codex: Option<String>,
}

/// `GET /api/models/defaults`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefaults {
    pub default_efforts: DefaultEfforts,
}

impl Reads {
    /// The catalogue in `<state_dir>/model-cache.json`; empty when the file is missing or broken, or there are no host paths.
    pub fn model_catalog(&self) -> ModelCatalog {
        let groups = self
            .host_paths()
            .and_then(|paths| fs::read_to_string(paths.state_dir.join("model-cache.json")).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|value| load_groups(&value))
            .unwrap_or_default();
        let mut default_model: Option<&String> = None;
        let mut newest: Option<f64> = None;
        for group in &groups {
            let (Some(model), Some(at)) = (&group.default_model, group.updated_at.as_f64()) else {
                continue;
            };
            if newest.is_none_or(|best| at > best) {
                newest = Some(at);
                default_model = Some(model);
            }
        }
        let default_model = default_model.cloned();
        ModelCatalog {
            groups,
            default_model,
        }
    }

    /// Records the model names read off Conductor's menu for `agent_type` in
    /// `<state_dir>/model-cache.json`. Nothing happens without host paths.
    pub fn record_models(
        &self,
        agent_type: &str,
        models: &[String],
        default_model: Option<&str>,
        now_ms: i64,
    ) -> io::Result<()> {
        let Some(paths) = self.host_paths() else {
            return Ok(());
        };
        if models.is_empty() {
            return Ok(());
        }
        let agent_type = match agent_type.trim() {
            "" => "unknown",
            trimmed => trimmed,
        };
        let _guard = CACHE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let path = paths.state_dir.join("model-cache.json");
        let mut entries = match fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        {
            Some(Value::Array(entries)) => entries,
            _ => Vec::new(),
        };
        let names: Vec<Value> = models.iter().cloned().map(Value::String).collect();
        let existing = entries.iter_mut().find_map(|entry| {
            let object = entry.as_object_mut()?;
            let found = object.get("agentType")?.as_str()?;
            let found = match found.trim() {
                "" => "unknown",
                trimmed => trimmed,
            };
            (found == agent_type).then_some(object)
        });
        match existing {
            Some(object) => {
                object.insert("agentType".into(), Value::from(agent_type));
                object.insert("models".into(), Value::Array(names));
                object.insert("updatedAt".into(), Value::from(now_ms));
                match default_model {
                    Some(model) => {
                        object.insert("defaultModel".into(), Value::from(model));
                    }
                    None => {
                        object.shift_remove("defaultModel");
                    }
                }
            }
            None => {
                let mut object = Map::new();
                object.insert("agentType".into(), Value::from(agent_type));
                object.insert("models".into(), Value::Array(names));
                if let Some(model) = default_model {
                    object.insert("defaultModel".into(), Value::from(model));
                }
                object.insert("updatedAt".into(), Value::from(now_ms));
                entries.push(Value::Object(object));
            }
        }
        let bytes = serde_json::to_vec_pretty(&Value::Array(entries))?;
        fs::create_dir_all(&paths.state_dir)?;
        let tmp = paths.state_dir.join("model-cache.json.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        drop(file);
        fs::rename(&tmp, &path)
    }

    /// The default efforts in `<home>/.conductor/settings.toml`; both absent when the file is missing or there are no host paths. Another read error is returned.
    pub fn model_defaults(&self) -> io::Result<ModelDefaults> {
        let mut efforts = DefaultEfforts {
            claude: None,
            codex: None,
        };
        if let Some(paths) = self.host_paths() {
            match fs::read_to_string(paths.home.join(".conductor").join("settings.toml")) {
                Ok(source) => {
                    efforts.claude =
                        read_value(&source, "models.claude_code", "default_effort_level");
                    efforts.codex = read_value(&source, "models.codex", "default_thinking_level");
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(ModelDefaults {
            default_efforts: efforts,
        })
    }
}

/// A picker label without its trailing ` NEW` marker, trimmed.
fn clean_label(label: &str) -> String {
    label
        .strip_suffix(" NEW")
        .unwrap_or(label)
        .trim()
        .to_string()
}

/// Cleaned, without empty labels and duplicates, ordered by lowercase form (the lowercase spelling
/// first between labels that differ only in case).
fn clean_labels(items: &[Value]) -> Vec<String> {
    let mut labels: Vec<String> = items
        .iter()
        .filter_map(Value::as_str)
        .map(clean_label)
        .filter(|label| !label.is_empty())
        .collect();
    labels.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| b.cmp(a))
    });
    labels.dedup();
    labels
}

fn load_groups(value: &Value) -> Vec<CachedModelGroup> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    entries.iter().filter_map(load_group).collect()
}

fn load_group(entry: &Value) -> Option<CachedModelGroup> {
    let agent_type = entry.get("agentType")?.as_str()?;
    let models = clean_labels(entry.get("models")?.as_array()?);
    if models.is_empty() {
        return None;
    }
    let agent_type = match agent_type.trim() {
        "" => "unknown".to_string(),
        trimmed => trimmed.to_string(),
    };
    let default_model = entry
        .get("defaultModel")
        .and_then(Value::as_str)
        .map(clean_label)
        .filter(|label| !label.is_empty());
    let snapshot_at = entry
        .get("snapshotAt")
        .map(|value| value.as_number().cloned());
    let snapshot_models = entry
        .get("snapshotModels")
        .and_then(Value::as_array)
        .map(|items| clean_labels(items));
    let selections = entry
        .get("selections")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(load_selection).collect());
    let updated_at = entry
        .get("updatedAt")
        .and_then(Value::as_number)
        .cloned()
        .unwrap_or_else(|| Number::from(0));
    Some(CachedModelGroup {
        agent_type,
        models,
        default_model,
        snapshot_at,
        snapshot_models,
        selections,
        updated_at,
    })
}

fn load_selection(item: &Value) -> Option<ModelSelection> {
    let model = clean_label(item.get("model")?.as_str()?);
    let selected_at = item.get("selectedAt")?.as_number()?.clone();
    (!model.is_empty()).then_some(ModelSelection { model, selected_at })
}

/// The name of a `[section]` line, quoted parts of a dotted name unquoted.
fn section_name(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix('[')?;
    let end = rest.find(']')?;
    let inner = &rest[..end];
    if inner.is_empty() {
        return None;
    }
    let after = rest[end + 1..].trim_start();
    if !after.is_empty() && !after.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = inner.split('.').map(|part| unquote(part.trim())).collect();
    Some(parts.join("."))
}

fn unquote(part: &str) -> &str {
    for quote in ['"', '\''] {
        if part.len() >= 2 && part.starts_with(quote) && part.ends_with(quote) {
            return &part[1..part.len() - 1];
        }
    }
    part
}

/// The string value of `key = "value"` (single or double quotes, on one line), if the line is one.
fn assignment(line: &str, key: &str) -> Option<String> {
    let mut rest = line.trim_start();
    let quoted = rest.starts_with(['"', '\'']);
    if quoted {
        rest = &rest[1..];
    }
    rest = rest.strip_prefix(key)?;
    if quoted {
        rest = rest.strip_prefix(['"', '\''])?;
    }
    rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = &rest[1..];
    let end = body.find(quote)?;
    let after = body[end + 1..].trim_start();
    (after.is_empty() || after.starts_with('#')).then(|| body[..end].to_string())
}

fn read_value(source: &str, section: &str, key: &str) -> Option<String> {
    let mut current: Option<String> = None;
    for line in source.lines() {
        if let Some(name) = section_name(line) {
            current = Some(name);
        }
        if current.as_deref() != Some(section) {
            continue;
        }
        if let Some(value) = assignment(line, key) {
            return Some(value);
        }
    }
    None
}
