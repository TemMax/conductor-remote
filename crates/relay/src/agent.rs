//! The agent settings the phone stages for a chat: model, effort, Plan and Fast.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A reasoning effort, as the phone names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultracode,
}

impl Effort {
    pub const ALL: [Effort; 7] = [
        Effort::None,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
        Effort::Ultracode,
    ];

    /// `none`, `low`, `medium`, `high`, `xhigh`, `max`, `ultracode`.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::None => "none",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
            Effort::Ultracode => "ultracode",
        }
    }

    /// The inverse of `as_str`; anything else is `None`.
    pub fn parse(text: &str) -> Option<Effort> {
        Effort::ALL
            .into_iter()
            .find(|effort| effort.as_str() == text)
    }

    /// The labels Conductor's Effort menu may give this level, in the order to try them.
    pub fn menu_labels(self) -> &'static [&'static str] {
        match self {
            Effort::None => &["Off"],
            Effort::Low => &["Low"],
            Effort::Medium => &["Medium"],
            Effort::High => &["High"],
            Effort::Xhigh => &["Extra high"],
            Effort::Max => &["Max"],
            Effort::Ultracode => &["Ultracode", "Ultra"],
        }
    }
}

/// What to change; an absent field is left as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<bool>,
}

impl AgentPatch {
    /// No field is set.
    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.effort.is_none() && self.plan.is_none() && self.fast.is_none()
    }

    /// Reads `model`, `effort`, `plan`, `fast` from a JSON object, checked in that order; other
    /// keys are ignored.
    pub fn from_object(object: &Map<String, Value>) -> Result<AgentPatch, String> {
        let model = match object.get("model") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => {
                let text = text.trim();
                (!text.is_empty()).then(|| text.to_owned())
            }
            Some(_) => return Err("model: must be a string".to_owned()),
        };
        let effort = match object.get("effort") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => match Effort::parse(text) {
                Some(effort) => Some(effort),
                None => return Err(effort_invalid()),
            },
            Some(_) => return Err(effort_invalid()),
        };
        let plan = boolean(object, "plan")?;
        let fast = boolean(object, "fast")?;
        Ok(AgentPatch {
            model,
            effort,
            plan,
            fast,
        })
    }

    /// Compact JSON, absent fields left out (`{}` when empty).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// The inverse of `to_json`; `None` for anything that does not parse.
    pub fn from_json(text: &str) -> Option<AgentPatch> {
        serde_json::from_str(text).ok()
    }
}

fn effort_invalid() -> String {
    "effort: must be one of none, low, medium, high, xhigh, max, ultracode".to_owned()
}

fn boolean(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{key}: must be a boolean")),
    }
}
