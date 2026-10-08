//! Provider question packets, independent of their Markdown/tool summaries.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionProvider {
    Claude,
    Codex,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequest {
    pub id: String,
    pub provider: QuestionProvider,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub header: Option<String>,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// Option indices refer to this request's original order, never to display labels.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionAnswer {
    pub selected: Vec<usize>,
    pub other: Option<String>,
}

impl QuestionRequest {
    pub fn validate_answers(&self, answers: &[QuestionAnswer]) -> bool {
        self.questions.len() == answers.len()
            && self.questions.iter().zip(answers).all(|(q, a)| {
                let other = a.other.as_deref().unwrap_or("");
                (a.selected.len() + usize::from(!other.trim().is_empty()) > 0)
                    && (q.multi_select
                        || a.selected.len() + usize::from(!other.trim().is_empty()) == 1)
                    && other.len() <= 16_384
                    && a.selected
                        .iter()
                        .enumerate()
                        .all(|(at, i)| *i < q.options.len() && !a.selected[..at].contains(i))
            })
    }
}

fn string(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 16_384)
        .map(str::to_owned)
}

fn request(id: &Value, input: &Value, provider: QuestionProvider) -> Option<QuestionRequest> {
    let id = string(id)?;
    let values = input.get("questions")?.as_array()?;
    if values.is_empty() || values.len() > 16 {
        return None;
    }
    let questions = values
        .iter()
        .map(|v| {
            let question = string(v.get("question")?)?;
            let options = v.get("options")?.as_array()?;
            if options.len() > 50 {
                return None;
            }
            let options = options
                .iter()
                .map(|v| {
                    if let Some(label) = v.as_str() {
                        Some(QuestionOption {
                            label: string(&Value::String(label.into()))?,
                            description: String::new(),
                        })
                    } else {
                        Some(QuestionOption {
                            label: string(v.get("label")?)?,
                            description: v
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        })
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Question {
                question,
                options,
                header: v.get("header").and_then(string),
                multi_select: v
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if questions.iter().enumerate().any(|(i, q)| {
        questions[..i]
            .iter()
            .any(|prev| prev.question == q.question)
            || q.options
                .iter()
                .enumerate()
                .any(|(j, option)| q.options[..j].contains(option))
    }) {
        return None;
    }
    Some(QuestionRequest {
        id,
        provider,
        questions,
    })
}

pub(crate) fn codex_request(frame: &Value) -> Option<QuestionRequest> {
    let packet = frame.get("codex_async_questions")?;
    request(packet.get("id")?, packet, QuestionProvider::Codex)
}

pub(crate) fn claude_request(block: &Value) -> Option<QuestionRequest> {
    if !matches!(
        block.get("name")?.as_str()?,
        "AskUserQuestion" | "mcp__conductor__AskUserQuestion"
    ) {
        return None;
    }
    request(
        block.get("id")?,
        block.get("input")?,
        QuestionProvider::Claude,
    )
}
