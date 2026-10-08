//! The turn watcher, the viewing stamps and the notification texts. Pure: no I/O, no clock of
//! its own (callers pass the instant), nothing that sends.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{chat_route, PushMessage};
use crate::reads::states::SessionStateRow;

/// A notification body is one glance on a lock screen; the chat has the rest.
const BODY_CHARS: usize = 180;
/// How much of a parked prompt the "sent" notification quotes.
const PARKED_PREVIEW_CHARS: usize = 140;
/// Stamps kept before the stale ones are pruned on the next `note`.
const VIEWING_PRUNE_ABOVE: usize = 16;
/// The part of a cut that may be given back to end on a word boundary.
const WORD_LOOKBACK: f64 = 0.2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnKind {
    Done,
    Error,
}

/// A chat whose transition has been confirmed and is worth a notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub state: SessionStateRow,
    pub kind: TurnKind,
}

/// Statuses that keep a completed turn open until the person responds.
fn turn_ended(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("idle" | "needs_plan_response" | "needs_user_input")
    )
}

/// Everything the chat records about a person having asked for something, as one comparable
/// string. Empty when it records neither timestamp, which reads as "no evidence".
fn asked_by(state: &SessionStateRow) -> String {
    if state.turn_started_at.is_none() && state.last_user_message_at.is_none() {
        return String::new();
    }
    format!(
        "{}|{}",
        state.turn_started_at.as_deref().unwrap_or(""),
        state.last_user_message_at.as_deref().unwrap_or("")
    )
}

/// Port of the reference's turn watcher: a baseline first step, arm on a transition, confirm on
/// the next step, loop suppression for `done` by "<turnStartedAt>|<lastUserMessageAt>".
#[derive(Debug, Default)]
pub struct TurnWatcher {
    /// Last seen status per chat. `None` means "re-baseline on the next step".
    previous: Option<HashMap<String, Option<String>>>,
    /// Transitions seen once and awaiting a second step's confirmation.
    armed: HashMap<String, TurnKind>,
    /// Who-asked as of the last turn said `done` about, per chat: what makes a self-scheduled
    /// turn quiet.
    notified_turn: HashMap<String, String>,
}

impl TurnWatcher {
    pub fn new() -> TurnWatcher {
        TurnWatcher::default()
    }

    /// Nobody is subscribed: forget the snapshot so the next step is a baseline again.
    pub fn reset(&mut self) {
        self.previous = None;
        self.armed.clear();
        self.notified_turn.clear();
    }

    /// One poll of every live chat, in; the notifications it earned, out.
    pub fn step(&mut self, states: &[SessionStateRow]) -> Vec<Due> {
        let current: HashMap<String, Option<String>> = states
            .iter()
            .map(|s| (s.session_id.clone(), s.status.clone()))
            .collect();
        let Some(baseline) = self.previous.take() else {
            self.previous = Some(current);
            return Vec::new();
        };
        let mut due = Vec::new();
        for state in states {
            let now = state.status.as_deref();
            let before = baseline.get(&state.session_id);
            if let Some(pending) = self.armed.remove(&state.session_id) {
                // Confirmed only if the new status held for a second step; a flap just drops
                // the arm.
                let held = match pending {
                    TurnKind::Done => turn_ended(now),
                    TurnKind::Error => now == Some("error"),
                };
                if held && !(pending == TurnKind::Done && self.self_scheduled(state)) {
                    let asked = asked_by(state);
                    if pending == TurnKind::Done && !asked.is_empty() {
                        self.notified_turn.insert(state.session_id.clone(), asked);
                    }
                    due.push(Due {
                        state: state.clone(),
                        kind: pending,
                    });
                }
                continue;
            }
            // A chat never seen (a new one, or the first step after re-baselining) adds its
            // status to the snapshot but is never itself news.
            let Some(before) = before else { continue };
            if before.as_deref() == Some("working") && turn_ended(now) {
                self.armed.insert(state.session_id.clone(), TurnKind::Done);
            } else if before.as_deref() != Some("error") && now == Some("error") {
                self.armed.insert(state.session_id.clone(), TurnKind::Error);
            }
        }
        // A chat armed on the last step can vanish before this one (its workspace was
        // archived mid-turn): drop the arm, and the remembered turn of any vanished chat.
        self.armed.retain(|id, _| current.contains_key(id));
        self.notified_turn.retain(|id, _| current.contains_key(id));
        self.previous = Some(current);
        due
    }

    /// Did this chat end a turn nobody asked for? True only for a repeat of the turn head
    /// last announced.
    fn self_scheduled(&self, state: &SessionStateRow) -> bool {
        let asked = asked_by(state);
        if asked.is_empty() {
            return false;
        }
        self.notified_turn
            .get(&state.session_id)
            .is_some_and(|last| *last == asked)
    }
}

/// Which chat each device shows; a stamp is fresh for `fresh` (10 s in production).
#[derive(Debug)]
pub struct Viewing {
    fresh: Duration,
    seen: HashMap<String, (String, Instant)>,
}

impl Viewing {
    pub fn new(fresh: Duration) -> Viewing {
        Viewing {
            fresh,
            seen: HashMap::new(),
        }
    }

    /// Replaces the device's stamp; prunes stale stamps when more than 16 are kept.
    pub fn note(&mut self, device_id: &str, session_id: &str, now: Instant) {
        if self.seen.len() > VIEWING_PRUNE_ABOVE {
            let fresh = self.fresh;
            self.seen
                .retain(|_, (_, at)| now.saturating_duration_since(*at) < fresh);
        }
        self.seen
            .insert(device_id.to_owned(), (session_id.to_owned(), now));
    }

    /// Is this device looking at this chat? Per device on purpose: the phone in a pocket still
    /// buzzes for a chat the tablet happens to show.
    pub fn is_reading(&self, device_id: &str, session_id: &str, now: Instant) -> bool {
        self.seen.get(device_id).is_some_and(|(chat, at)| {
            chat == session_id && now.saturating_duration_since(*at) < self.fresh
        })
    }

    pub fn forget(&mut self, device_id: &str) {
        self.seen.remove(device_id);
    }
}

/// White space as a JavaScript regular expression's `\s` reads it: Unicode white space, without
/// U+0085 and with U+FEFF.
fn js_space(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

/// At most `max` characters (chars, not bytes), the ellipsis included; backs off to a word
/// boundary within the last 20 % when the cut lands inside a word.
pub fn clip_exact(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    if max == 1 {
        return "…".to_owned();
    }
    let hard = &chars[..max - 1];
    // Back off only when the cut lands inside a word; a cut on a space has nothing to repair.
    let splits_word = !js_space(chars[max - 1]);
    let space = hard.iter().rposition(|&c| c == ' ');
    let floor = ((max - 1) as f64 * (1.0 - WORD_LOOKBACK)).floor() as usize;
    let body = match space {
        Some(at) if splits_word && at >= floor => &hard[..at],
        _ => hard,
    };
    let body: String = body.iter().collect();
    format!("{}…", body.trim_end_matches(js_space))
}

/// Fenced code becomes `…`, whitespace runs one space, trimmed, then `clip_exact`.
pub fn one_line(text: &str, max: usize) -> String {
    let mut unfenced = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some(close) = after.find("```") else {
            break;
        };
        unfenced.push_str(&rest[..open]);
        unfenced.push('…');
        rest = &after[close + 3..];
    }
    unfenced.push_str(rest);

    let mut collapsed = String::with_capacity(unfenced.len());
    let mut in_space = false;
    for c in unfenced.chars() {
        if js_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    clip_exact(collapsed.trim_matches(js_space), max)
}

/// A turn notification: title "<workspace>[ · <chat>][ — <repo>]", body from `said`
/// (`one_line(said, 180)`, else "Finished its turn." / "Stopped with an error. <said>" /
/// "The agent stopped with an error."), tag = session id, url = `chat_route`, kind "done"/"error".
pub fn turn_message(due: &Due, said: Option<&str>, now_ms: i64) -> PushMessage {
    let state = &due.state;
    let said = said.filter(|s| !s.is_empty());
    let where_ = match state.session_title.as_deref().filter(|t| !t.is_empty()) {
        Some(chat) => format!("{} · {chat}", state.workspace_title),
        None => state.workspace_title.clone(),
    };
    let title = match state.repo_name.as_deref().filter(|r| !r.is_empty()) {
        Some(repo) => format!("{where_} — {repo}"),
        None => where_,
    };
    let (body, kind) = match (due.kind, said) {
        (TurnKind::Error, Some(said)) => (
            format!("Stopped with an error. {}", one_line(said, BODY_CHARS)),
            "error",
        ),
        (TurnKind::Error, None) => ("The agent stopped with an error.".to_owned(), "error"),
        (TurnKind::Done, Some(said)) => (one_line(said, BODY_CHARS), "done"),
        (TurnKind::Done, None) => ("Finished its turn.".to_owned(), "done"),
    };
    PushMessage {
        title,
        body,
        // Per chat, so a chatty agent replaces its own notification instead of stacking.
        tag: state.session_id.clone(),
        url: chat_route(&state.workspace_id, &state.session_id),
        kind: kind.to_owned(),
        ts: now_ms,
    }
}

/// After a parked prompt: body "Sent after unlock: <clip_exact(text, 140)>" or
/// "Parked prompt failed: <error>", tag "parked-<session id>", kind "done"/"error".
pub fn parked_message(
    title: &str,
    workspace_id: &str,
    session_id: &str,
    text: &str,
    error: Option<&str>,
    now_ms: i64,
) -> PushMessage {
    let error = error.filter(|e| !e.is_empty());
    let (body, kind) = match error {
        Some(error) => (format!("Parked prompt failed: {error}"), "error"),
        None => (
            format!(
                "Sent after unlock: {}",
                clip_exact(text, PARKED_PREVIEW_CHARS)
            ),
            "done",
        ),
    };
    PushMessage {
        title: title.to_owned(),
        body,
        // Per chat, so a second parked prompt replaces the first's notification.
        tag: format!("parked-{session_id}"),
        url: chat_route(workspace_id, session_id),
        kind: kind.to_owned(),
        ts: now_ms,
    }
}
