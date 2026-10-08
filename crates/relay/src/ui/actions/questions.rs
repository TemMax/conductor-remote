//! Answer only a matched native question form; never use the ordinary composer.
use super::*;
use crate::transcript::questions::{Question, QuestionAnswer, QuestionRequest};

fn words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_question<N: UiNode>(node: &N, question: &Question) -> bool {
    has_role(node, "AXStaticText")
        && node
            .value()
            .ok()
            .flatten()
            .is_some_and(|v| words(&v) == words(&question.question))
}

fn form<D: Desktop>(
    d: &D,
    pid: i32,
    target: &Target,
    request: &QuestionRequest,
    guard: &dyn Fn() -> bool,
) -> Result<D::Node, UiError> {
    if !guard() {
        return Err(UiError::QuestionStale);
    }
    native_form(d, pid, target, request)?.ok_or(UiError::QuestionStale)
}

fn native_form<D: Desktop>(
    d: &D,
    pid: i32,
    target: &Target,
    request: &QuestionRequest,
) -> Result<Option<D::Node>, UiError> {
    let pane = current_pane(d, pid)?
        .filter(|p| pane_agrees(p, target))
        .ok_or(UiError::QuestionStale)?;
    if let Some(tab) = &target.tab {
        let radios = chat_strip(&pane).unwrap_or_default();
        if !(radios.is_empty() && tab.count == 1)
            && (radios.len() != tab.count
                || radios
                    .get(tab.index - 1)
                    .is_none_or(|r| r.selected() != Some(true)))
        {
            return Err(UiError::QuestionStale);
        }
    }
    let matches = |n: &D::Node| {
        has_role(n, "AXGroup")
            && first(n, 3, |c| {
                has_role(c, "AXTextArea") && label_is(c, "Other response")
            })
            .is_some()
            && first(n, 3, |c| {
                request.questions.iter().any(|q| is_question(c, q))
            })
            .is_some()
    };
    // A failed observation is not evidence that Submit dismissed the form.
    let nodes = checked_nodes(&pane, 20)?;
    // label() also returns None when AX title/description reads fail. An
    // unidentified textarea could still be the native Other response field.
    if nodes
        .iter()
        .any(|n| has_role(n, "AXTextArea") && n.label().is_none_or(|label| label.trim().is_empty()))
    {
        return Err(UiError::QuestionStale);
    }
    let has_other = nodes
        .iter()
        .any(|n| has_role(n, "AXTextArea") && label_is(n, "Other response"));
    let forms: Vec<_> = nodes
        .into_iter()
        .filter(matches)
        .filter(|n| bfs(n, 20, matches).len() == 1)
        .collect();
    match forms.as_slice() {
        [form] => Ok(Some(form.clone())),
        [] if !has_other => Ok(None),
        _ => Err(UiError::QuestionStale),
    }
}

fn checked_nodes<N: UiNode>(root: &N, max_depth: usize) -> Result<Vec<N>, UiError> {
    let mut nodes = Vec::new();
    let mut queue = VecDeque::from([(root.clone(), 0)]);
    while let Some((node, depth)) = queue.pop_front() {
        if node.role().is_none() {
            return Err(UiError::QuestionStale);
        }
        if depth < max_depth {
            queue.extend(node.children()?.into_iter().map(|child| (child, depth + 1)));
        }
        nodes.push(node);
    }
    Ok(nodes)
}

fn page<D: Desktop>(
    d: &D,
    pid: i32,
    target: &Target,
    request: &QuestionRequest,
    index: usize,
    guard: &dyn Fn() -> bool,
) -> Result<D::Node, UiError> {
    let mut group = form(d, pid, target, request, guard)?;
    if request.questions.len() > 1 {
        let nav = group
            .children()?
            .into_iter()
            .find(|n| has_role(n, "AXButton") && label_is(n, &format!("Question {}", index + 1)))
            .ok_or(UiError::QuestionStale)?;
        nav.press()?;
        d.pause(ms(100));
        group = form(d, pid, target, request, guard)?;
    }
    let q = &request.questions[index];
    if first(&group, 3, |n| is_question(n, q)).is_none() {
        return Err(UiError::QuestionStale);
    }
    controls(&group, q)?;
    Ok(group)
}

fn controls<N: UiNode>(group: &N, q: &Question) -> Result<Vec<N>, UiError> {
    let role = if q.multi_select {
        "AXCheckBox"
    } else {
        "AXRadioButton"
    };
    let nodes: Vec<_> = group
        .children()?
        .into_iter()
        .filter(|n| has_role(n, role) && !label_is(n, "Select other response"))
        .collect();
    if nodes.len() != q.options.len() {
        return Err(UiError::QuestionStale);
    }
    for (i, (node, option)) in nodes.iter().zip(&q.options).enumerate() {
        let expected = words(&format!(
            "{} {} {}",
            i + 1,
            option.label,
            option.description
        ));
        if node.label().is_none_or(|l| words(&l) != expected) || node.flag().is_none() {
            return Err(UiError::QuestionStale);
        }
    }
    Ok(nodes)
}

fn other_area<N: UiNode>(group: &N) -> Result<N, UiError> {
    first(group, 3, |n| {
        has_role(n, "AXTextArea") && label_is(n, "Other response")
    })
    .ok_or(UiError::QuestionStale)
}
fn empty_other(value: Option<String>) -> bool {
    value.is_none_or(|v| v.is_empty() || v == "[text entry area: Other response]")
}
fn custom_matches(value: Option<String>, wanted: &str) -> bool {
    value.is_some_and(|v| {
        let v = normalize(&v);
        let wanted = normalize(wanted);
        v == wanted || v == wanted.replace('\n', "\n\n")
    })
}

pub(super) fn answer<D: Desktop>(
    d: &D,
    target: &Target,
    request: &QuestionRequest,
    answers: &[QuestionAnswer],
    guard: &dyn Fn() -> bool,
) -> Result<(), UiError> {
    if !request.validate_answers(answers) {
        return Err(UiError::QuestionInvalid);
    }
    if !guard() {
        return Err(UiError::QuestionStale);
    }
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    select_tab(d, pid, target)?;
    ensure_front(d, pid)?;
    // Inspect every page before changing any answers.
    for i in 0..request.questions.len() {
        page(d, pid, target, request, i, guard)?;
    }
    for (i, (q, a)) in request.questions.iter().zip(answers).enumerate() {
        let other = a
            .other
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("");
        let group = page(d, pid, target, request, i, guard)?;
        let area = other_area(&group)?;
        if other.is_empty() && !empty_other(area.value()?) {
            area.set_focused(true)?;
            area.set_value("")?;
            d.pause(ms(100));
        }
        if q.multi_select {
            for option in 0..q.options.len() {
                let group = page(d, pid, target, request, i, guard)?;
                let nodes = controls(&group, q)?;
                if nodes[option].flag() != Some(a.selected.contains(&option)) {
                    nodes[option].press()?;
                    d.pause(ms(100));
                }
            }
        } else if let Some(option) = a.selected.first() {
            let group = page(d, pid, target, request, i, guard)?;
            let nodes = controls(&group, q)?;
            if nodes[*option].flag() != Some(true) {
                nodes[*option].press()?;
                d.pause(ms(100));
            }
        }
        if !other.is_empty() {
            let group = page(d, pid, target, request, i, guard)?;
            let area = other_area(&group)?;
            area.set_focused(true)?;
            area.set_value(other)?;
            d.pause(ms(100));
            if !custom_matches(area.value()?, other) {
                return Err(UiError::QuestionInvalid);
            }
        }
        if q.multi_select {
            let group = page(d, pid, target, request, i, guard)?;
            if let Some(check) = first(&group, 3, |n| {
                has_role(n, "AXCheckBox") && label_is(n, "Select other response")
            }) {
                if check.flag() != Some(!other.is_empty()) {
                    check.press()?;
                    d.pause(ms(100));
                }
            } else if !other.is_empty() {
                return Err(UiError::QuestionStale);
            }
        }
    }
    // Read back all choices after navigation/automatic advance before the single submit.
    for (i, (q, a)) in request.questions.iter().zip(answers).enumerate() {
        let group = page(d, pid, target, request, i, guard)?;
        let other = a
            .other
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("");
        let area = other_area(&group)?;
        if (other.is_empty() && !empty_other(area.value()?))
            || (!other.is_empty() && !custom_matches(area.value()?, other))
        {
            return Err(UiError::QuestionInvalid);
        }
        if q.multi_select {
            if let Some(check) = first(&group, 3, |n| {
                has_role(n, "AXCheckBox") && label_is(n, "Select other response")
            }) {
                if check.flag() != Some(!other.is_empty()) {
                    return Err(UiError::QuestionInvalid);
                }
            } else if !other.is_empty() {
                return Err(UiError::QuestionStale);
            }
        }
        let nodes = controls(&group, q)?;
        if nodes
            .iter()
            .enumerate()
            .any(|(j, n)| n.flag() != Some(a.selected.contains(&j)))
        {
            return Err(UiError::QuestionInvalid);
        }
    }
    let group = form(d, pid, target, request, guard)?;
    let buttons: Vec<_> = group
        .children()?
        .into_iter()
        .filter(|n| has_role(n, "AXButton") && n.label().is_none_or(|l| l.is_empty()))
        .collect();
    if buttons.len() != 1 {
        return Err(UiError::QuestionStale);
    }
    // An AXPress error can arrive after the native app accepted it. Never press again.
    buttons[0]
        .press()
        .map_err(|_| UiError::QuestionSubmissionUnknown)?;
    let gone = look(d, ms(100), || {
        native_form(d, pid, target, request)
            .map(|form| form.is_none().then_some(()))
            .map_err(|_| UiError::QuestionSubmissionUnknown)
    })?;
    if gone.is_none() {
        return Err(UiError::QuestionSubmissionUnknown);
    }
    Ok(())
}
