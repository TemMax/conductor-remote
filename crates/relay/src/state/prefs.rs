//! Synced phone preferences, kept in the relay's database.
//!
//! The phone keeps its own live copy; this is the sync peer that survives a change of the web
//! app's origin and lets another phone pick up where the first stopped. The document holds
//! `readMarks` and `drafts` and nothing device-scoped. It is stored as JSON under the meta key
//! `prefs`.

use std::cmp::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{json, Map, Value};

use crate::contract::PrefsService;
use crate::files::attachments::{attachment_name, attachment_token, MAX_ATTACHMENT_BYTES};
use crate::state::store::Store;

const META_KEY: &str = "prefs";

const MAX_KEYS: usize = 50_000;
const MAX_KEY_LENGTH: usize = 256;
const MAX_MARK_LENGTH: usize = 128;
const MAX_DRAFT_LENGTH: usize = 1_000_000;
const MAX_AGENT_LABEL_LENGTH: usize = 256;
const MAX_ATTACHMENTS: usize = 100;
const MAX_ATTACHMENT_NAME_LENGTH: usize = 256;
const MAX_ATTACHMENT_PATH_LENGTH: usize = 1024;
const MAX_ATTACHMENT_TOKEN_LENGTH: usize = 4096;
const ATTACHMENT_DIR_PREFIX: &str = ".context/attachments/";
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

const AGENT_EFFORTS: [&str; 7] = ["none", "low", "medium", "high", "xhigh", "max", "ultracode"];

const NOT_AN_OBJECT: &str = "preferences must be an object";
const NOTHING_TO_SYNC: &str = "nothing to sync";

/// The synced preferences over the relay's database.
pub struct Prefs {
    store: Arc<Store>,
    /// The document as last read or written. The lock also makes a merge atomic.
    cache: Mutex<Option<Doc>>,
}

impl Prefs {
    pub fn new(store: Arc<Store>) -> Prefs {
        Prefs {
            store,
            cache: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Doc>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached document, read from the store the first time. An unreadable or malformed
    /// stored document is an empty one; a store that cannot be read is an error.
    fn current<'a>(&self, cache: &'a mut Option<Doc>) -> Result<&'a Doc, String> {
        if cache.is_none() {
            let stored = self
                .store
                .meta(META_KEY)
                .map_err(|error| format!("could not read synced preferences: {error}"))?;
            let doc = stored
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .map(|value| sanitize_doc(&value))
                .unwrap_or_default();
            *cache = Some(doc);
        }
        Ok(cache.get_or_insert_with(Doc::default))
    }

    fn persist(&self, doc: &Doc) {
        let text = doc.to_value().to_string();
        if let Err(error) = self.store.set_meta(META_KEY, &text) {
            tracing::warn!("could not persist synced preferences ({error})");
        }
    }
}

impl PrefsService for Prefs {
    fn get(&self) -> Value {
        let mut cache = self.lock();
        match self.current(&mut cache) {
            Ok(doc) => doc.to_value(),
            Err(error) => {
                tracing::warn!("{error}");
                Doc::default().to_value()
            }
        }
    }

    /// Marks take the larger value; a draft revision is last-writer-wins on `updatedAt`, a
    /// deletion winning an exact tie.
    fn patch(&self, patch: Value) -> Result<Value, String> {
        let Some(input) = patch.as_object() else {
            return Err(NOT_AN_OBJECT.to_owned());
        };
        if !input.contains_key("readMarks") && !input.contains_key("drafts") {
            return Err(NOTHING_TO_SYNC.to_owned());
        }
        let mut cache = self.lock();
        let current = self.current(&mut cache)?;
        let mut next = current.clone();
        let mut changed = false;

        if let Some(raw_marks) = input.get("readMarks") {
            for (key, mark) in sanitize_read_marks(Some(raw_marks)) {
                let held = next.read_marks.get(&key).and_then(Value::as_str);
                if held.is_some_and(|held| compare_js(held, &mark) != Ordering::Less) {
                    continue;
                }
                next.read_marks.insert(key, Value::String(mark));
                changed = true;
            }
        }

        if let Some(raw_drafts) = input.get("drafts") {
            let sanitized = sanitize_drafts(Some(raw_drafts));
            let raw_drafts = raw_drafts.as_object();
            for (key, mut draft) in sanitized {
                let previous = next.drafts.get(&key).and_then(sanitize_draft);
                let raw_draft = raw_drafts
                    .and_then(|drafts| drafts.get(&key))
                    .and_then(Value::as_object);
                // A client from before attachment sync does not know the field. Keep what it
                // cannot represent while letting its newer text and settings win. Tombstones
                // always clear the whole intent.
                if let (Some(previous), Some(raw_draft)) = (&previous, raw_draft) {
                    if !draft.deleted && !raw_draft.contains_key("attachments") {
                        draft.attachments = previous.attachments.clone();
                    }
                }
                let wins = match &previous {
                    None => true,
                    Some(previous) => {
                        draft.updated_at > previous.updated_at
                            || (draft.updated_at == previous.updated_at
                                && draft.deleted
                                && !previous.deleted)
                    }
                };
                if !wins || previous.as_ref().is_some_and(|p| same_draft(p, &draft)) {
                    continue;
                }
                next.drafts.insert(key, draft.to_value());
                changed = true;
            }
        }

        if !changed {
            return Ok(current.to_value());
        }
        self.persist(&next);
        let value = next.to_value();
        *cache = Some(next);
        Ok(value)
    }
}

#[derive(Clone, Default)]
struct Doc {
    read_marks: Map<String, Value>,
    drafts: Map<String, Value>,
}

impl Doc {
    fn to_value(&self) -> Value {
        json!({ "readMarks": self.read_marks, "drafts": self.drafts })
    }
}

#[derive(Clone)]
struct Draft {
    text: String,
    agent: Agent,
    attachments: Vec<Attachment>,
    updated_at: i64,
    deleted: bool,
}

impl Draft {
    fn to_value(&self) -> Value {
        json!({
            "text": self.text,
            "agent": self.agent.to_value(),
            "attachments": self.attachments.iter().map(Attachment::to_value).collect::<Vec<_>>(),
            "updatedAt": self.updated_at,
            "deleted": self.deleted,
        })
    }
}

#[derive(Clone, Default, PartialEq)]
struct Agent {
    auto: Option<bool>,
    model: Option<String>,
    effort: Option<String>,
    plan: Option<bool>,
    fast: Option<bool>,
}

impl Agent {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        if let Some(auto) = self.auto {
            map.insert("auto".into(), auto.into());
        }
        if let Some(model) = &self.model {
            map.insert("model".into(), model.clone().into());
        }
        if let Some(effort) = &self.effort {
            map.insert("effort".into(), effort.clone().into());
        }
        if let Some(plan) = self.plan {
            map.insert("plan".into(), plan.into());
        }
        if let Some(fast) = self.fast {
            map.insert("fast".into(), fast.into());
        }
        Value::Object(map)
    }
}

#[derive(Clone, PartialEq)]
struct Attachment {
    name: String,
    path: String,
    bytes: u64,
    token: String,
    fork: bool,
    stage_id: Option<String>,
}

impl Attachment {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("name".into(), self.name.clone().into());
        map.insert("path".into(), self.path.clone().into());
        map.insert("bytes".into(), self.bytes.into());
        map.insert("token".into(), self.token.clone().into());
        if self.fork {
            map.insert("source".into(), "fork".into());
        }
        if let Some(stage_id) = &self.stage_id {
            map.insert("stageId".into(), stage_id.clone().into());
        }
        Value::Object(map)
    }
}

/// Length in UTF-16 code units, as the phone and the reference count it.
fn length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Order by UTF-16 code units, as comparing strings with `<` does there.
fn compare_js(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn valid_key(key: &str) -> bool {
    let units = length(key);
    units > 0 && units <= MAX_KEY_LENGTH
}

/// `Number(value)` for the JSON values a client sends, `None` for NaN. Arrays and objects are
/// not coerced and count as NaN.
fn js_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        Value::Null => Some(0.0),
        Value::String(text) => js_string_number(text),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn js_string_number(text: &str) -> Option<f64> {
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if text.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if text.len() > 2 && text[..2].eq_ignore_ascii_case(prefix) {
            return u64::from_str_radix(&text[2..], radix)
                .ok()
                .map(|number| number as f64);
        }
    }
    match text {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if !text.chars().all(|c| "0123456789+-.eE".contains(c)) {
        return None;
    }
    text.parse::<f64>().ok()
}

/// `Number.isSafeInteger(Number(value))`, and the integer.
fn safe_integer(value: Option<&Value>) -> Option<i64> {
    let number = js_number(value)?;
    (number.is_finite() && number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER)
        .then_some(number as i64)
}

fn sanitize_doc(raw: &Value) -> Doc {
    let object = raw.as_object();
    let drafts = sanitize_drafts(object.and_then(|o| o.get("drafts")));
    Doc {
        read_marks: sanitize_read_marks(object.and_then(|o| o.get("readMarks")))
            .into_iter()
            .map(|(key, mark)| (key, Value::String(mark)))
            .collect(),
        drafts: drafts
            .into_iter()
            .map(|(key, draft)| (key, draft.to_value()))
            .collect(),
    }
}

/// Valid entries, newest mark first, at most `MAX_KEYS`.
fn sanitize_read_marks(raw: Option<&Value>) -> Vec<(String, String)> {
    let Some(object) = raw.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, String)> = object
        .iter()
        .filter_map(|(key, mark)| {
            let mark = mark.as_str()?;
            let units = length(mark);
            (valid_key(key) && units > 0 && units <= MAX_MARK_LENGTH)
                .then(|| (key.clone(), mark.to_owned()))
        })
        .collect();
    entries.sort_by(|a, b| compare_js(&b.1, &a.1));
    entries.truncate(MAX_KEYS);
    entries
}

fn sanitize_agent(raw: Option<&Value>) -> Agent {
    let Some(value) = raw.and_then(Value::as_object) else {
        return Agent::default();
    };
    Agent {
        auto: value.get("auto").and_then(Value::as_bool),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| length(model) <= MAX_AGENT_LABEL_LENGTH)
            .map(str::to_owned),
        effort: value
            .get("effort")
            .and_then(Value::as_str)
            .filter(|effort| AGENT_EFFORTS.contains(effort))
            .map(str::to_owned),
        plan: value.get("plan").and_then(Value::as_bool),
        fast: value.get("fast").and_then(Value::as_bool),
    }
}

fn sanitize_attachment(raw: &Value) -> Option<Attachment> {
    let value = raw.as_object()?;
    let name = value.get("name")?.as_str()?;
    let path = value.get("path")?.as_str()?;
    let token = value.get("token")?.as_str()?;
    let bytes = u64::try_from(safe_integer(value.get("bytes"))?).ok()?;
    let fork = value.get("source").and_then(Value::as_str) == Some("fork");
    if name.is_empty()
        || length(name) > MAX_ATTACHMENT_NAME_LENGTH
        || length(path) > MAX_ATTACHMENT_PATH_LENGTH
        || length(token) > MAX_ATTACHMENT_TOKEN_LENGTH
        // Forks reference an already-written transcript, which can exceed the upload limit.
        || (bytes > MAX_ATTACHMENT_BYTES as u64 && !fork)
    {
        return None;
    }
    let (stage, file) = attachment_path_parts(path)?;
    if file != name || attachment_name(name) != name || token != attachment_token(name, path) {
        return None;
    }
    let stage_id = match value.get("stageId") {
        None => None,
        Some(Value::String(id)) if id == stage => Some(id.clone()),
        Some(_) => return None,
    };
    Some(Attachment {
        name: name.to_owned(),
        path: path.to_owned(),
        bytes,
        token: token.to_owned(),
        fork,
        stage_id,
    })
}

/// The stage id and file name of `.context/attachments/<6 letters or digits>/<name>`.
fn attachment_path_parts(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix(ATTACHMENT_DIR_PREFIX)?;
    let (stage, file) = rest.split_once('/')?;
    let stage_ok = stage.len() == 6 && stage.bytes().all(|b| b.is_ascii_alphanumeric());
    (stage_ok && !file.is_empty() && !file.contains('/')).then_some((stage, file))
}

/// Valid attachments without repeated paths, at most `MAX_ATTACHMENTS`.
fn sanitize_attachments(raw: Option<&Value>) -> Vec<Attachment> {
    let Some(candidates) = raw.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut attachments: Vec<Attachment> = Vec::new();
    for candidate in candidates {
        let Some(attachment) = sanitize_attachment(candidate) else {
            continue;
        };
        if attachments.iter().any(|seen| seen.path == attachment.path) {
            continue;
        }
        attachments.push(attachment);
        if attachments.len() == MAX_ATTACHMENTS {
            break;
        }
    }
    attachments
}

fn sanitize_draft(raw: &Value) -> Option<Draft> {
    let value = raw.as_object()?;
    let updated_at = safe_integer(value.get("updatedAt")).filter(|at| *at >= 0)?;
    if value.get("deleted") == Some(&Value::Bool(true)) {
        return Some(Draft {
            text: String::new(),
            agent: Agent::default(),
            attachments: Vec::new(),
            updated_at,
            deleted: true,
        });
    }
    let text = value.get("text")?.as_str()?;
    if length(text) > MAX_DRAFT_LENGTH {
        return None;
    }
    Some(Draft {
        text: text.to_owned(),
        agent: sanitize_agent(value.get("agent")),
        attachments: sanitize_attachments(value.get("attachments")),
        updated_at,
        deleted: false,
    })
}

/// Valid drafts, newest first, at most `MAX_KEYS`.
fn sanitize_drafts(raw: Option<&Value>) -> Vec<(String, Draft)> {
    let Some(object) = raw.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, Draft)> = object
        .iter()
        .filter(|(key, _)| valid_key(key))
        .filter_map(|(key, candidate)| Some((key.clone(), sanitize_draft(candidate)?)))
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.1.updated_at));
    entries.truncate(MAX_KEYS);
    entries
}

/// Equal in everything that is synced; the `auto` flag is not compared.
fn same_draft(a: &Draft, b: &Draft) -> bool {
    a.text == b.text
        && a.updated_at == b.updated_at
        && a.deleted == b.deleted
        && a.attachments == b.attachments
        && a.agent.model == b.agent.model
        && a.agent.effort == b.agent.effort
        && a.agent.plan == b.agent.plan
        && a.agent.fast == b.agent.fast
}
