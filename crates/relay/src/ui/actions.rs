//! The UI actions over a [`Desktop`](super::desktop::Desktop): focusing a workspace and a chat, the composer, stop and new chat,
//! the model menu and the agent settings, the workspace controls (closing a chat, the status,
//! archiving, Continue, and the New workspace dialog), and the Run task.
//!
//! Nothing is typed, written, pressed or posted before `focus` has seen a main pane that agrees
//! with the target; the sidebar link `focus` itself presses is one exception, the Create button
//! of the New workspace dialog, which belongs to no workspace yet, the other. In the live
//! window the main pane (`AXGroup`/`AXLandmarkMain`) names the open workspace in two of its
//! direct children: an `AXPopUpButton` whose label is the repository name twice ("relay relay"),
//! and an `AXStaticText` whose value is the displayed branch (usually without its owner prefix).
//! The branch is in no pop-up's label.

use std::collections::VecDeque;
use std::time::Duration;

use super::ax::AxError;
use super::desktop::Desktop;
use super::driver::{
    AgentFailure, AgentOutcome, RunOutcome, Tab, Target, UiDriver, UiError, ViewReport,
};
use super::keys::{Key, Modifiers};
use super::node::UiNode;
use crate::agent::{AgentPatch, Effort};

/// How deep below the window the main pane and the sidebar are looked for.
const WINDOW_DEPTH: usize = 12;
/// How deep below the pane (or the sidebar, or the composer group) anything else is looked for.
const PANE_DEPTH: usize = 8;
/// How deep below the composer group an open menu is looked for.
const MENU_DEPTH: usize = 10;
/// How many times a wait looks before it gives up.
const LOOKS: usize = 10;
/// The label prefix of a chat tab's radio.
const CHAT_PREFIX: &str = "Close chat ";
/// The label prefix of the model pop-up and of its menu.
const AGENT_PREFIX: &str = "Change agent";
/// The label prefix of the model menu's Effort entry and of the Effort menu.
const EFFORT_PREFIX: &str = "Effort ";
/// The label of the Add pop-up and of its menu.
const ADD_LABEL: &str = "Add";
/// How a model item that opens a new chat ends its label.
const NEW_CHAT_SUFFIX: &str = "Opens in new chat";
/// How deep below the window an alert or the New workspace dialog is looked for.
const DIALOG_DEPTH: usize = 16;
/// How deep below the window a sidebar row's menu is looked for.
const ROW_MENU_DEPTH: usize = 20;
/// How deep below the window the Set status menu is looked for.
const STATUS_MENU_DEPTH: usize = 24;
/// The label of the row menu's status entry and of the menu it opens.
const STATUS_LABEL: &str = "Set status";
/// The label prefix of the Run strip's button when nothing runs.
const RUN_PREFIX: &str = "Run ";
/// The label prefix of the Run strip's button while a task runs.
const STOP_PREFIX: &str = "Stop ";
/// The label of the Run strip's task pop-up and of its menu.
const SELECT_TASK: &str = "Select task";
/// How deep below the Run strip its open menu is looked for.
const RUN_MENU_DEPTH: usize = 6;
/// How many times the Run strip is looked at for a started or a stopped task.
const RUN_LOOKS: usize = 15;

/// The UI commands over a desktop.
pub struct Driver<D: Desktop> {
    desktop: D,
}

impl<D: Desktop> Driver<D> {
    pub fn new(desktop: D) -> Driver<D> {
        Driver { desktop }
    }

    pub fn desktop(&self) -> &D {
        &self.desktop
    }
}

impl<D: Desktop> UiDriver for Driver<D> {
    fn trusted(&self) -> bool {
        self.desktop.trusted()
    }

    fn send_prompt(&mut self, target: &Target, text: &str, queue: bool) -> Result<u32, UiError> {
        send_prompt(&self.desktop, target, text, queue)
    }

    fn stop_turn(&mut self, target: &Target) -> Result<(), UiError> {
        stop_turn(&self.desktop, target)
    }

    fn new_chat(&mut self, target: &Target) -> Result<(), UiError> {
        new_chat(&self.desktop, target)
    }

    fn locate(&mut self) -> Result<ViewReport, UiError> {
        locate(&self.desktop)
    }

    fn open_link(&mut self, url: &str) -> Result<(), UiError> {
        open_link(&self.desktop, url)
    }

    fn list_models(&mut self, target: &Target) -> Result<Vec<String>, UiError> {
        list_models(&self.desktop, target)
    }

    fn set_agent(
        &mut self,
        target: &Target,
        patch: &AgentPatch,
    ) -> Result<AgentOutcome, AgentFailure> {
        set_agent(&self.desktop, target, patch)
    }

    fn close_chat(&mut self, target: &Target, confirm: bool) -> Result<(), UiError> {
        close_chat(&self.desktop, target, confirm)
    }

    fn set_status(&mut self, target: &Target, row: &str, label: &str) -> Result<(), UiError> {
        set_status(&self.desktop, target, row, label)
    }

    fn archive(&mut self, target: &Target, confirm: bool) -> Result<(), UiError> {
        archive(&self.desktop, target, confirm)
    }

    fn press_continue(&mut self, target: &Target) -> Result<(), UiError> {
        press_continue(&self.desktop, target)
    }

    fn confirm_create(&mut self) -> Result<(), UiError> {
        confirm_create(&self.desktop)
    }

    fn run_task(
        &mut self,
        target: &Target,
        task: Option<&str>,
        start: bool,
    ) -> Result<RunOutcome, UiError> {
        run_task(&self.desktop, target, task, start)
    }
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

fn command() -> Modifiers {
    Modifiers {
        command: true,
        ..Modifiers::default()
    }
}

fn post<D: Desktop>(d: &D, pid: i32, key: Key, modifiers: Modifiers) -> Result<(), UiError> {
    d.post_key(pid, key, modifiers).map_err(UiError::Key)
}

// ---- tree helpers ----

/// Breadth-first from `root` (included) down to `max_depth` levels below it: every node `pred`
/// accepts, in order. A node whose children fail to read contributes none.
fn bfs<N: UiNode>(root: &N, max_depth: usize, pred: impl Fn(&N) -> bool) -> Vec<N> {
    walk(root, max_depth, &pred, usize::MAX)
}

/// The first node `bfs` would return, without walking further.
fn first<N: UiNode>(root: &N, max_depth: usize, pred: impl Fn(&N) -> bool) -> Option<N> {
    walk(root, max_depth, &pred, 1).into_iter().next()
}

fn walk<N: UiNode>(root: &N, max_depth: usize, pred: &dyn Fn(&N) -> bool, limit: usize) -> Vec<N> {
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(root.clone(), 0usize)]);
    while let Some((node, depth)) = queue.pop_front() {
        if pred(&node) {
            found.push(node.clone());
            if found.len() >= limit {
                break;
            }
        }
        if depth < max_depth {
            if let Ok(children) = node.children() {
                queue.extend(children.into_iter().map(|child| (child, depth + 1)));
            }
        }
    }
    found
}

fn has_role<N: UiNode>(node: &N, role: &str) -> bool {
    node.role().as_deref() == Some(role)
}

fn has_roles<N: UiNode>(node: &N, role: &str, subrole: &str) -> bool {
    has_role(node, role) && node.subrole().as_deref() == Some(subrole)
}

fn label_is<N: UiNode>(node: &N, label: &str) -> bool {
    node.label().as_deref() == Some(label)
}

fn label_starts_with<N: UiNode>(node: &N, prefix: &str) -> bool {
    node.label().is_some_and(|label| label.starts_with(prefix))
}

/// Whether `word` is one of the whitespace-separated words of `text`.
fn has_word(text: &str, word: &str) -> bool {
    text.split_whitespace().any(|candidate| candidate == word)
}

/// `\r\n` and `\r` as `\n`.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Whether AXValue holds `wanted` (already normalized). Conductor's WebKit composer
/// exposes paragraph breaks as two newlines, even when the editor contains one.
/// Match that representation as well without changing the text written or its receipt.
fn holds(value: Option<String>, wanted: &str) -> bool {
    value.is_some_and(|value| {
        let value = normalize(&value);
        value.contains(wanted)
            || (wanted.contains('\n') && value.contains(&wanted.replace('\n', "\n\n")))
    })
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

// ---- steps ----

fn locked<D: Desktop>(d: &D) -> bool {
    d.session().is_some_and(|session| session.locked)
}

/// Conductor's window as one read found it, with the main pane the choice already walked to.
struct View<N> {
    window: N,
    /// The window's main pane; `None` when it has none.
    pane: Option<N>,
}

/// Trust, lock and pid checks, then the first window.
fn prepare<D: Desktop>(d: &D) -> Result<(i32, View<D::Node>), UiError> {
    let pid = checks(d)?;
    Ok((pid, read_window(d, pid)?))
}

/// Trust, lock and pid checks, in that order: Conductor's pid.
fn checks<D: Desktop>(d: &D) -> Result<i32, UiError> {
    if !d.trusted() {
        return Err(UiError::NotTrusted);
    }
    if locked(d) {
        return Err(UiError::Locked);
    }
    d.conductor_pid().ok_or(UiError::NotRunning)
}

/// Conductor's main window: of the app's `AXWindow` children, the first standard window that
/// holds a main pane, else the first standard window, else the first window. A dialog may be
/// listed before the main window. Its read failures are the command's. The pane found while
/// choosing is kept: the standard-window fallbacks know of none, since every standard window
/// was just walked in vain; only the last-resort first window is walked once more.
fn read_window<D: Desktop>(d: &D, pid: i32) -> Result<View<D::Node>, UiError> {
    let app = d.application(pid);
    let windows: Vec<D::Node> = app
        .children()
        .map_err(UiError::from)?
        .into_iter()
        .filter(|child| has_role(child, "AXWindow"))
        .collect();
    let standard = |window: &&D::Node| has_roles(*window, "AXWindow", "AXStandardWindow");
    let with_pane = windows
        .iter()
        .filter(standard)
        .find_map(|window| main_pane(window).map(|pane| (window, Some(pane))));
    let chosen = with_pane
        .or_else(|| windows.iter().find(standard).map(|window| (window, None)))
        .or_else(|| windows.first().map(|window| (window, main_pane(window))));
    match chosen {
        Some((window, pane)) => Ok(View {
            window: window.clone(),
            pane,
        }),
        None if locked(d) => Err(UiError::Locked),
        None => Err(UiError::NoWindow),
    }
}

fn main_pane<N: UiNode>(window: &N) -> Option<N> {
    first(window, WINDOW_DEPTH, |node| {
        has_roles(node, "AXGroup", "AXLandmarkMain")
    })
}

fn current_pane<D: Desktop>(d: &D, pid: i32) -> Result<Option<D::Node>, UiError> {
    Ok(read_window(d, pid)?.pane)
}

/// The label of the pane's first `AXPopUpButton`, breadth-first: the repository name twice.
fn pane_header<N: UiNode>(pane: &N) -> Option<String> {
    first(pane, PANE_DEPTH, |node| has_role(node, "AXPopUpButton"))
        .and_then(|header| header.label())
}

/// Whether a direct `AXStaticText` shows the branch, its tail, or the branch without its
/// owner prefix (Conductor's display for nested branch names). Failed reads count as none.
fn shows_branch<N: UiNode>(pane: &N, target: &Target) -> bool {
    pane.children().unwrap_or_default().iter().any(|child| {
        has_role(child, "AXStaticText")
            && child.value().ok().flatten().is_some_and(|value| {
                let value = value.trim();
                value == target.branch_tail()
                    || value == target.branch
                    || target
                        .branch
                        .split_once('/')
                        .is_some_and(|(_, remainder)| value == remainder)
            })
    })
}

/// The pane shows the target's branch (or its tail) as the exact value of a direct
/// `AXStaticText` child and, when the repo is known, its pop-up header names the repo as a whole
/// word.
fn pane_agrees<N: UiNode>(pane: &N, target: &Target) -> bool {
    if !shows_branch(pane, target) {
        return false;
    }
    match non_empty(&target.repo) {
        Some(repo) => pane_header(pane).is_some_and(|header| has_word(&header, repo)),
        None => true,
    }
}

fn agreeing_pane<D: Desktop>(d: &D, pid: i32, target: &Target) -> Result<Option<D::Node>, UiError> {
    Ok(current_pane(d, pid)?.filter(|pane| pane_agrees(pane, target)))
}

/// Opens the deep link and waits for the pane to agree; failing that, presses the workspace's
/// sidebar link and waits again.
fn focus<D: Desktop>(d: &D, pid: i32, target: &Target) -> Result<D::Node, UiError> {
    d.open_url(&target.deep_link());
    for _ in 0..10 {
        if let Some(pane) = agreeing_pane(d, pid, target)? {
            return Ok(pane);
        }
        d.pause(ms(150));
    }

    if let Some(name) = non_empty(&target.workspace_name) {
        let window = read_window(d, pid)?.window;
        let links = first(&window, WINDOW_DEPTH, |node| {
            has_roles(node, "AXGroup", "AXLandmarkComplementary")
        })
        .map(|sidebar| {
            bfs(&sidebar, PANE_DEPTH, |node| {
                has_role(node, "AXLink") && label_is(node, name)
            })
        })
        .unwrap_or_default();
        if let [link] = links.as_slice() {
            link.press()?;
            d.pause(ms(900));
            for _ in 0..5 {
                if let Some(pane) = agreeing_pane(d, pid, target)? {
                    return Ok(pane);
                }
                d.pause(ms(200));
            }
        }
    }

    let repo = non_empty(&target.repo).unwrap_or("the workspace");
    Err(UiError::WorkspaceNotFocused(format!(
        "{repo} on {}",
        target.branch_tail()
    )))
}

/// The chat radios of the first tab group that has any, in order.
fn chat_strip<N: UiNode>(pane: &N) -> Option<Vec<N>> {
    bfs(pane, PANE_DEPTH, |node| has_role(node, "AXTabGroup"))
        .into_iter()
        .map(|group| chat_radios(&group))
        .find(|radios| !radios.is_empty())
}

/// The group's direct radios and the radios of its direct children, labelled "Close chat …".
fn chat_radios<N: UiNode>(group: &N) -> Vec<N> {
    let mut radios = Vec::new();
    for child in group.children().unwrap_or_default() {
        if has_role(&child, "AXRadioButton") {
            radios.push(child.clone());
        }
        radios.extend(
            child
                .children()
                .unwrap_or_default()
                .into_iter()
                .filter(|grandchild| has_role(grandchild, "AXRadioButton")),
        );
    }
    radios.retain(|radio| label_starts_with(radio, CHAT_PREFIX));
    radios
}

fn chat_title<N: UiNode>(radio: &N) -> Option<String> {
    radio
        .label()
        .and_then(|label| label.strip_prefix(CHAT_PREFIX).map(str::to_owned))
}

fn current_strip<D: Desktop>(d: &D, pid: i32) -> Result<Option<Vec<D::Node>>, UiError> {
    Ok(current_pane(d, pid)?.and_then(|pane| chat_strip(&pane)))
}

/// The 0-based position of the radio `tab` names.
fn pick<N: UiNode>(radios: &[N], tab: &Tab) -> Result<usize, UiError> {
    let titles: Vec<Option<String>> = radios.iter().map(chat_title).collect();
    let at = tab
        .index
        .checked_sub(1)
        .filter(|&position| position < radios.len());
    if let Some(position) = at {
        if tab.title.is_none() || titles[position] == tab.title {
            return Ok(position);
        }
    }
    let Some(title) = &tab.title else {
        return Err(UiError::TabNotFound(tab.index));
    };
    let matching: Vec<usize> = titles
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.as_deref() == Some(title.as_str()))
        .map(|(position, _)| position)
        .collect();
    match matching.as_slice() {
        [one] => Ok(*one),
        [] => Err(UiError::TabNotFound(tab.index)),
        _ => Err(UiError::SeveralTabs(title.clone())),
    }
}

fn select_tab<D: Desktop>(d: &D, pid: i32, target: &Target) -> Result<(), UiError> {
    let Some(tab) = &target.tab else {
        return Ok(());
    };
    let Some(radios) = current_strip(d, pid)? else {
        return if tab.count <= 1 {
            Ok(())
        } else {
            Err(UiError::NoChatStrip)
        };
    };
    let position = pick(&radios, tab)?;
    let radio = &radios[position];
    if radio.selected() == Some(true) {
        return Ok(());
    }
    radio.press()?;
    d.pause(ms(500));
    let selected = current_strip(d, pid)?
        .and_then(|radios| radios.get(position).and_then(UiNode::selected))
        == Some(true);
    if selected {
        Ok(())
    } else {
        Err(UiError::TabNotSelected)
    }
}

fn ensure_front<D: Desktop>(d: &D, pid: i32) -> Result<(), UiError> {
    if d.frontmost_pid() == Some(pid) {
        return Ok(());
    }
    d.activate(pid);
    for attempt in 0..10 {
        if attempt > 0 {
            d.pause(ms(100));
        }
        if d.frontmost_pid() == Some(pid) {
            return Ok(());
        }
    }
    Err(UiError::NotFrontmost)
}

/// The group labelled "composer".
fn composer_group<N: UiNode>(pane: &N) -> Option<N> {
    first(pane, PANE_DEPTH, |node| {
        has_role(node, "AXGroup") && label_is(node, "composer")
    })
}

/// The text area of the group labelled "composer".
fn composer<N: UiNode>(pane: &N) -> Option<N> {
    let group = composer_group(pane)?;
    first(&group, PANE_DEPTH, |node| has_role(node, "AXTextArea"))
}

// ---- agent controls ----

/// Looks up to `LOOKS` times, pausing `pause` between two looks: what the first look that found
/// something found. Nothing is paused before the first look or after the last.
fn look<D: Desktop, T>(
    d: &D,
    pause: Duration,
    once: impl FnMut() -> Result<Option<T>, UiError>,
) -> Result<Option<T>, UiError> {
    look_n(d, LOOKS, pause, once)
}

/// `look`, with `count` looks at most.
fn look_n<D: Desktop, T>(
    d: &D,
    count: usize,
    pause: Duration,
    mut once: impl FnMut() -> Result<Option<T>, UiError>,
) -> Result<Option<T>, UiError> {
    for attempt in 0..count {
        if attempt > 0 {
            d.pause(pause);
        }
        if let Some(found) = once()? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// What the model pop-up's label says of the chat.
struct Shown {
    model: String,
    effort: Option<String>,
    fast: bool,
}

impl Shown {
    /// `Change agent (<model>[ · <effort>][ · Fast])`; `None` for any other label.
    fn parse(label: &str) -> Option<Shown> {
        let inner = label.strip_prefix("Change agent (")?.strip_suffix(')')?;
        let mut parts: Vec<&str> = inner.split(" · ").collect();
        let fast = parts.len() > 1 && parts.last() == Some(&"Fast");
        if fast {
            parts.pop();
        }
        Some(Shown {
            model: parts[0].to_owned(),
            effort: parts.get(1).map(|effort| (*effort).to_owned()),
            fast,
        })
    }
}

/// The composer group of the pane as it is now. Elements may be replaced when Conductor
/// re-renders, so every read of the controls starts here.
fn fresh_composer<D: Desktop>(d: &D, pid: i32) -> Result<D::Node, UiError> {
    current_pane(d, pid)?
        .and_then(|pane| composer_group(&pane))
        .ok_or(UiError::NoComposer)
}

/// The `AXPopUpButton` whose label starts with "Change agent".
fn model_popup<D: Desktop>(d: &D, pid: i32) -> Result<D::Node, UiError> {
    let group = fresh_composer(d, pid)?;
    first(&group, PANE_DEPTH, |node| {
        has_role(node, "AXPopUpButton") && label_starts_with(node, AGENT_PREFIX)
    })
    .ok_or(UiError::NoModelPicker)
}

fn shown<D: Desktop>(d: &D, pid: i32) -> Result<Shown, UiError> {
    model_popup(d, pid)?
        .label()
        .and_then(|label| Shown::parse(&label))
        .ok_or(UiError::NoModelPicker)
}

/// The first `AXMenu` under the composer group that `pred` accepts.
fn open_menu<D: Desktop>(
    d: &D,
    pid: i32,
    pred: impl Fn(&D::Node) -> bool,
) -> Result<Option<D::Node>, UiError> {
    let group = fresh_composer(d, pid)?;
    Ok(first(&group, MENU_DEPTH, |node| {
        has_role(node, "AXMenu") && pred(node)
    }))
}

fn model_menu<D: Desktop>(d: &D, pid: i32) -> Result<Option<D::Node>, UiError> {
    open_menu(d, pid, |menu| label_starts_with(menu, AGENT_PREFIX))
}

fn effort_menu<D: Desktop>(d: &D, pid: i32) -> Result<Option<D::Node>, UiError> {
    open_menu(d, pid, |menu| label_starts_with(menu, EFFORT_PREFIX))
}

fn add_menu<D: Desktop>(d: &D, pid: i32) -> Result<Option<D::Node>, UiError> {
    open_menu(d, pid, |menu| label_is(menu, ADD_LABEL))
}

/// The menu's direct `AXMenuItem` children, in order.
fn menu_items<N: UiNode>(menu: &N) -> Vec<N> {
    let mut items = menu.children().unwrap_or_default();
    items.retain(|item| has_role(item, "AXMenuItem"));
    items
}

/// The model menu: the one already open, else the one a press on the pop-up opens.
fn open_model_menu<D: Desktop>(d: &D, pid: i32) -> Result<D::Node, UiError> {
    if let Some(menu) = model_menu(d, pid)? {
        return Ok(menu);
    }
    model_popup(d, pid)?.press()?;
    look(d, ms(150), || model_menu(d, pid))?.ok_or_else(|| UiError::MenuNotOpened("model".into()))
}

/// Presses the pop-up until the model menu is gone, twice at most.
fn close_model_menu<D: Desktop>(d: &D, pid: i32) -> Result<(), UiError> {
    if model_menu(d, pid)?.is_none() {
        return Ok(());
    }
    for _ in 0..2 {
        model_popup(d, pid)?.press()?;
        let gone = look(d, ms(150), || {
            Ok(model_menu(d, pid)?.is_none().then_some(()))
        })?;
        if gone.is_some() {
            return Ok(());
        }
    }
    Err(UiError::MenuStuck)
}

/// The item `name` picks among `items` (each with its help): the one whose help is `name`
/// ignoring ASCII case, else the only one whose help starts with `name` ignoring case.
fn pick_model<N: UiNode>(mut items: Vec<(N, String)>, name: &str) -> Result<(N, String), UiError> {
    if let Some(position) = items
        .iter()
        .position(|(_, help)| help.eq_ignore_ascii_case(name))
    {
        return Ok(items.swap_remove(position));
    }
    let prefix = name.to_lowercase();
    let mut starting = items
        .into_iter()
        .filter(|(_, help)| help.to_lowercase().starts_with(&prefix));
    match (starting.next(), starting.next()) {
        (Some(only), None) => Ok(only),
        (None, _) => Err(UiError::NoModel(name.to_owned())),
        (Some(_), Some(_)) => Err(UiError::SeveralModels(name.to_owned())),
    }
}

/// Step 1: picks the model in the model menu and waits for the pop-up to name it. `new_chat` is
/// set before the item that opens a new chat is pressed, so a press that fails still reports it.
fn set_model<D: Desktop>(d: &D, pid: i32, name: &str, new_chat: &mut bool) -> Result<(), UiError> {
    let current = shown(d, pid)?.model;
    if current.eq_ignore_ascii_case(name) {
        return Ok(());
    }
    let menu = open_model_menu(d, pid)?;
    let items = menu_items(&menu)
        .into_iter()
        .filter_map(|item| item.help().map(|help| (item, help)))
        .collect();
    let (item, help) = pick_model(items, name)?;
    if help == current {
        return Ok(());
    }
    let opens_chat = item
        .label()
        .is_some_and(|label| label.ends_with(NEW_CHAT_SUFFIX));
    *new_chat = opens_chat;
    item.press()?;
    look(d, ms(200), || {
        Ok((shown(d, pid)?.model == help).then_some(()))
    })?
    .ok_or(UiError::ModelNotApplied(help))
}

/// Step 2: picks the effort in the model menu's Effort menu and waits for the pop-up to show it.
fn set_effort<D: Desktop>(d: &D, pid: i32, effort: Effort) -> Result<(), UiError> {
    let labels = effort.menu_labels();
    let already = shown(d, pid)?
        .effort
        .is_some_and(|current| labels.contains(&current.as_str()));
    if already {
        return Ok(());
    }
    let missing = || UiError::NoEffort(labels[0].into());
    let menu = open_model_menu(d, pid)?;
    let entry = menu_items(&menu)
        .into_iter()
        .find(|item| label_starts_with(item, EFFORT_PREFIX))
        .ok_or_else(missing)?;
    entry.press()?;
    let levels = look(d, ms(150), || effort_menu(d, pid))?
        .ok_or_else(|| UiError::MenuNotOpened("effort".into()))?;
    let items = menu_items(&levels);
    let (item, label) = labels
        .iter()
        .find_map(|label| {
            items
                .iter()
                .find(|item| label_is(*item, label))
                .map(|item| (item, *label))
        })
        .ok_or_else(missing)?;
    item.press()?;
    look(d, ms(200), || {
        Ok((shown(d, pid)?.effort.as_deref() == Some(label)).then_some(()))
    })?
    .ok_or_else(|| UiError::EffortNotApplied(label.into()))
}

/// Step 3: presses the model menu's Fast item unless it is already as wanted, and waits for the
/// pop-up to agree.
fn set_fast<D: Desktop>(d: &D, pid: i32, want: bool) -> Result<(), UiError> {
    if shown(d, pid)?.fast == want {
        return Ok(());
    }
    let menu = open_model_menu(d, pid)?;
    let item = menu_items(&menu)
        .into_iter()
        .find(|item| label_is(item, "Fast"))
        .ok_or(UiError::NoFast)?;
    if item.flag() != Some(want) {
        item.press()?;
    }
    look(d, ms(200), || {
        Ok((shown(d, pid)?.fast == want).then_some(()))
    })?
    .ok_or(UiError::FastNotApplied)
}

/// Steps 1 to 3, the ones that work in the model menu, which they may leave open.
fn set_in_model_menu<D: Desktop>(
    d: &D,
    pid: i32,
    patch: &AgentPatch,
    new_chat: &mut bool,
) -> Result<(), UiError> {
    if let Some(name) = &patch.model {
        set_model(d, pid, name, new_chat)?;
    }
    if let Some(effort) = patch.effort {
        set_effort(d, pid, effort)?;
    }
    if let Some(want) = patch.fast {
        set_fast(d, pid, want)?;
    }
    Ok(())
}

/// The `AXPopUpButton` labelled "Add".
fn add_popup<D: Desktop>(d: &D, pid: i32) -> Result<D::Node, UiError> {
    let group = fresh_composer(d, pid)?;
    first(&group, PANE_DEPTH, |node| {
        has_role(node, "AXPopUpButton") && label_is(node, ADD_LABEL)
    })
    .ok_or(UiError::NoPlan)
}

/// Presses `Add` and reads Plan mode from its menu, which stays open: whether it is on, and the
/// item that changes it. A menu with no Plan item is closed with one more press.
fn read_plan<D: Desktop>(d: &D, pid: i32) -> Result<(bool, D::Node), UiError> {
    add_popup(d, pid)?.press()?;
    let menu = look(d, ms(150), || add_menu(d, pid))?
        .ok_or_else(|| UiError::MenuNotOpened(ADD_LABEL.into()))?;
    let items = menu_items(&menu);
    let item = |prefix: &str| items.iter().find(|item| label_starts_with(*item, prefix));
    let plan = item("Exit plan mode")
        .map(|item| (true, item.clone()))
        .or_else(|| item("Plan mode").map(|item| (false, item.clone())));
    match plan {
        Some(plan) => Ok(plan),
        None => {
            add_popup(d, pid)?.press()?;
            Err(UiError::NoPlan)
        }
    }
}

/// Presses `Add` and waits for its menu to go.
fn close_add_menu<D: Desktop>(d: &D, pid: i32) -> Result<(), UiError> {
    add_popup(d, pid)?.press()?;
    look(d, ms(150), || Ok(add_menu(d, pid)?.is_none().then_some(())))?.ok_or(UiError::MenuStuck)
}

/// Step 4: presses the Add menu's Plan item unless Plan mode is already as wanted, then reads
/// the menu once more.
fn set_plan<D: Desktop>(d: &D, pid: i32, want: bool) -> Result<(), UiError> {
    let (on, item) = read_plan(d, pid)?;
    if on == want {
        return close_add_menu(d, pid);
    }
    item.press()?;
    d.pause(ms(300));
    let (on, _) = read_plan(d, pid)?;
    close_add_menu(d, pid)?;
    if on == want {
        Ok(())
    } else {
        Err(UiError::PlanNotApplied)
    }
}

// ---- workspace controls ----

/// The alert titled `title` in the window as it is now: the `AXGroup`/`AXApplicationAlertDialog`
/// with that label.
fn alert<D: Desktop>(d: &D, pid: i32, title: &str) -> Result<Option<D::Node>, UiError> {
    let window = read_window(d, pid)?.window;
    Ok(first(&window, DIALOG_DEPTH, |node| {
        has_roles(node, "AXGroup", "AXApplicationAlertDialog") && label_is(node, title)
    }))
}

/// Answers the alert titled `title`, when Conductor shows it: `confirm` presses the button whose
/// label starts with `confirm_prefix`; otherwise `Cancel` is pressed and the answer is
/// `NeedsConfirmation`. Either way the alert must be gone afterwards. No alert is no question.
fn answer_alert<D: Desktop>(
    d: &D,
    pid: i32,
    title: &str,
    confirm_prefix: &str,
    confirm: bool,
) -> Result<(), UiError> {
    let Some(dialog) = look_n(d, 5, ms(200), || alert(d, pid, title))? else {
        return Ok(());
    };
    let button = first(&dialog, PANE_DEPTH, |node| {
        has_role(node, "AXButton")
            && if confirm {
                label_starts_with(node, confirm_prefix)
            } else {
                label_is(node, "Cancel")
            }
    })
    .ok_or(UiError::DialogStuck)?;
    button.press()?;
    look(d, ms(150), || {
        Ok(alert(d, pid, title)?.is_none().then_some(()))
    })?
    .ok_or(UiError::DialogStuck)?;
    if confirm {
        Ok(())
    } else {
        Err(UiError::NeedsConfirmation)
    }
}

/// The `Set status` item of the open row menu: of the first `AXMenu` under the window that has
/// one among its direct items.
fn status_entry<D: Desktop>(d: &D, pid: i32) -> Result<Option<D::Node>, UiError> {
    let window = read_window(d, pid)?.window;
    Ok(
        bfs(&window, ROW_MENU_DEPTH, |node| has_role(node, "AXMenu"))
            .iter()
            .find_map(|menu| {
                menu_items(menu)
                    .into_iter()
                    .find(|item| label_is(item, STATUS_LABEL))
            }),
    )
}

/// The open `AXMenu` labelled `Set status`.
fn status_menu<D: Desktop>(d: &D, pid: i32) -> Result<Option<D::Node>, UiError> {
    let window = read_window(d, pid)?.window;
    Ok(first(&window, STATUS_MENU_DEPTH, |node| {
        has_role(node, "AXMenu") && label_is(node, STATUS_LABEL)
    }))
}

// ---- run controls ----

/// The strip's direct `AXButton` whose label starts with `prefix`, and the task it names: the
/// rest of its label.
fn run_button<N: UiNode>(strip: &N, prefix: &str) -> Option<(N, String)> {
    strip
        .children()
        .unwrap_or_default()
        .into_iter()
        .filter(|child| has_role(child, "AXButton"))
        .find_map(|button| {
            let name = button.label()?.strip_prefix(prefix)?.to_owned();
            Some((button, name))
        })
}

/// The Run strip: of the pane's direct `AXTabGroup` children, the first that has a Run or a Stop
/// button.
fn run_strip<N: UiNode>(pane: &N) -> Option<N> {
    pane.children()
        .unwrap_or_default()
        .into_iter()
        .find(|child| {
            has_role(child, "AXTabGroup")
                && (run_button(child, RUN_PREFIX).is_some()
                    || run_button(child, STOP_PREFIX).is_some())
        })
}

/// The Run strip of the pane as it is now. Its buttons are replaced when a task starts or stops,
/// so every read of the strip starts here.
fn current_run_strip<D: Desktop>(d: &D, pid: i32) -> Result<Option<D::Node>, UiError> {
    Ok(current_pane(d, pid)?.and_then(|pane| run_strip(&pane)))
}

/// The task the strip names now on a button whose label starts with `prefix`.
fn run_shows<D: Desktop>(d: &D, pid: i32, prefix: &str) -> Result<Option<String>, UiError> {
    Ok(current_run_strip(d, pid)?
        .and_then(|strip| run_button(&strip, prefix))
        .map(|(_, name)| name))
}

/// The strip's direct `AXPopUpButton` labelled "Select task".
fn task_popup<N: UiNode>(strip: &N) -> Option<N> {
    strip
        .children()
        .unwrap_or_default()
        .into_iter()
        .find(|child| has_role(child, "AXPopUpButton") && label_is(child, SELECT_TASK))
}

/// The open `AXMenu` labelled "Select task" under the strip as it is now.
fn task_menu<D: Desktop>(d: &D, pid: i32) -> Result<Option<D::Node>, UiError> {
    Ok(current_run_strip(d, pid)?.and_then(|strip| {
        first(&strip, RUN_MENU_DEPTH, |node| {
            has_role(node, "AXMenu") && label_is(node, SELECT_TASK)
        })
    }))
}

/// Presses `stop`, the strip's Stop button, and waits for the strip to show a Run button.
fn stop_run<D: Desktop>(d: &D, pid: i32, stop: &D::Node) -> Result<(), UiError> {
    stop.press()?;
    look_n(d, RUN_LOOKS, ms(200), || run_shows(d, pid, RUN_PREFIX))?
        .map(|_| ())
        .ok_or(UiError::RunNotChanged)
}

/// Presses what starts a task in the strip as it is now. With a name: the Run button when it
/// names that task, else the task's item in the Select task menu, which starts it at once; a menu
/// without the item is closed with one more press on the pop-up. With no name: the Run button.
fn press_run<D: Desktop>(d: &D, pid: i32, task: Option<&str>) -> Result<(), UiError> {
    let strip = current_run_strip(d, pid)?.ok_or(UiError::NoRunStrip)?;
    let run = run_button(&strip, RUN_PREFIX);
    let Some(name) = task else {
        let (button, _) = run.ok_or(UiError::NoRunStrip)?;
        button.press()?;
        return Ok(());
    };
    if let Some((button, _)) = run.filter(|(_, shown)| shown == name) {
        button.press()?;
        return Ok(());
    }
    let missing = || UiError::NoRunTask(name.to_owned());
    task_popup(&strip).ok_or_else(missing)?.press()?;
    let menu = look(d, ms(150), || task_menu(d, pid))?
        .ok_or_else(|| UiError::MenuNotOpened(SELECT_TASK.into()))?;
    let Some(item) = menu_items(&menu)
        .into_iter()
        .find(|item| label_is(item, name))
    else {
        if let Some(popup) = current_run_strip(d, pid)?.and_then(|strip| task_popup(&strip)) {
            popup.press()?;
        }
        return Err(missing());
    };
    item.press()?;
    Ok(())
}

// ---- commands ----

fn send_prompt<D: Desktop>(
    d: &D,
    target: &Target,
    text: &str,
    queue: bool,
) -> Result<u32, UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    select_tab(d, pid, target)?;
    let area = current_pane(d, pid)?
        .and_then(|pane| composer(&pane))
        .ok_or(UiError::NoComposer)?;
    ensure_front(d, pid)?;

    // Focus only helps the write; a refusal is not a failure.
    let _ = area.set_focused(true);
    if area.set_value(text).is_err() {
        let _ = area.set_value("");
        return Err(UiError::ComposerRejected);
    }
    d.pause(ms(250));
    let wanted = normalize(text);
    if !holds(area.value()?, &wanted) {
        let _ = area.set_value("");
        return Err(UiError::ComposerRejected);
    }

    let modifiers = Modifiers {
        command: queue,
        ..Modifiers::default()
    };
    for press in 1..=2 {
        post(d, pid, Key::Return, modifiers)?;
        for _ in 0..4 {
            d.pause(ms(200));
            // A composer that went away, is empty or no longer holds the prompt has sent it.
            let still_there = area
                .value()
                .map(|value| holds(value, &wanted))
                .unwrap_or(false);
            if !still_there {
                return Ok(press);
            }
        }
    }
    Err(UiError::StillInComposer)
}

fn stop_turn<D: Desktop>(d: &D, target: &Target) -> Result<(), UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    select_tab(d, pid, target)?;
    ensure_front(d, pid)?;
    post(d, pid, Key::L, command())?;
    d.pause(ms(200));
    post(
        d,
        pid,
        Key::Delete,
        Modifiers {
            shift: true,
            ..command()
        },
    )?;
    d.pause(ms(300));
    Ok(())
}

fn new_chat<D: Desktop>(d: &D, target: &Target) -> Result<(), UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    ensure_front(d, pid)?;
    post(d, pid, Key::L, command())?;
    d.pause(ms(200));
    post(d, pid, Key::T, command())?;
    d.pause(ms(300));
    Ok(())
}

fn locate<D: Desktop>(d: &D) -> Result<ViewReport, UiError> {
    let (_, view) = prepare(d)?;
    let Some(pane) = view.pane else {
        return Ok(ViewReport::default());
    };
    let strip = chat_strip(&pane).unwrap_or_default();
    Ok(ViewReport {
        pane_header: pane_header(&pane),
        chat_tabs: strip.len(),
        selected_tab: strip
            .iter()
            .position(|radio| radio.selected() == Some(true))
            .map(|position| position + 1),
        composer: composer(&pane).is_some(),
    })
}

/// The checks of `prepare` without the window read, then the link; a link the system would not
/// take is `Ax(Failure)`.
fn open_link<D: Desktop>(d: &D, url: &str) -> Result<(), UiError> {
    checks(d)?;
    if d.open_url(url) {
        Ok(())
    } else {
        Err(UiError::Ax(AxError::Failure))
    }
}

/// The model names of the target chat's model menu, in menu order: the help of each item that
/// has one. The menu is closed again whatever the read gave.
fn list_models<D: Desktop>(d: &D, target: &Target) -> Result<Vec<String>, UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    select_tab(d, pid, target)?;
    ensure_front(d, pid)?;
    let read = open_model_menu(d, pid).map(|menu| {
        menu_items(&menu)
            .iter()
            .filter_map(UiNode::help)
            .collect::<Vec<String>>()
    });
    let closed = close_model_menu(d, pid);
    let names = read?;
    closed?;
    if names.is_empty() {
        return Err(UiError::NoModelPicker);
    }
    Ok(names)
}

/// Applies `patch` through the composer's menus: model, effort and Fast in the model menu, which
/// is then closed whatever they returned, then Plan in the Add menu.
fn set_agent<D: Desktop>(
    d: &D,
    target: &Target,
    patch: &AgentPatch,
) -> Result<AgentOutcome, AgentFailure> {
    if patch.is_empty() {
        return Ok(AgentOutcome::default());
    }
    if target.branch.is_empty() {
        return Err(UiError::NoBranch.into());
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    select_tab(d, pid, target)?;
    ensure_front(d, pid)?;

    let mut new_chat = false;
    let applied = set_in_model_menu(d, pid, patch, &mut new_chat);
    let closed = close_model_menu(d, pid);
    applied
        .and(closed)
        .and_then(|()| match patch.plan {
            Some(want) => set_plan(d, pid, want),
            None => Ok(()),
        })
        .map(|()| AgentOutcome { new_chat })
        .map_err(|error| AgentFailure { error, new_chat })
}

/// Closes the target chat with Cmd+W and answers the alert Conductor shows for a running agent.
fn close_chat<D: Desktop>(d: &D, target: &Target, confirm: bool) -> Result<(), UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    select_tab(d, pid, target)?;
    ensure_front(d, pid)?;
    post(d, pid, Key::L, command())?;
    d.pause(ms(200));
    post(d, pid, Key::W, command())?;
    answer_alert(d, pid, "Close running chat?", "Close anyway", confirm)
}

/// Archives the target workspace with Cmd+Shift+A and answers the alert Conductor shows when
/// agents are running.
fn archive<D: Desktop>(d: &D, target: &Target, confirm: bool) -> Result<(), UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    ensure_front(d, pid)?;
    post(d, pid, Key::L, command())?;
    d.pause(ms(200));
    post(
        d,
        pid,
        Key::A,
        Modifiers {
            shift: true,
            ..command()
        },
    )?;
    answer_alert(
        d,
        pid,
        "Archive workspace?",
        "Stop agents and archive",
        confirm,
    )
}

/// Presses the `Continue` button that is a direct child of the target's pane.
fn press_continue<D: Desktop>(d: &D, target: &Target) -> Result<(), UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    select_tab(d, pid, target)?;
    let button = current_pane(d, pid)?
        .and_then(|pane| {
            pane.children()
                .unwrap_or_default()
                .into_iter()
                .find(|child| has_role(child, "AXButton") && label_is(child, "Continue"))
        })
        .ok_or(UiError::NoContinue)?;
    ensure_front(d, pid)?;
    button.press()?;
    Ok(())
}

/// Picks `label` in the Set status menu of the sidebar row titled `row`: the row's menu, its
/// `Set status` entry, then the item. A menu without the item is closed with Escape.
fn set_status<D: Desktop>(d: &D, target: &Target, row: &str, label: &str) -> Result<(), UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    let window = read_window(d, pid)?.window;
    let rows = first(&window, WINDOW_DEPTH, |node| {
        has_roles(node, "AXGroup", "AXLandmarkComplementary")
    })
    .map(|sidebar| {
        bfs(&sidebar, PANE_DEPTH, |node| {
            has_role(node, "AXLink") && label_is(node, row)
        })
    })
    .unwrap_or_default();
    let [link] = rows.as_slice() else {
        return Err(UiError::NoSidebarRow(row.into()));
    };
    ensure_front(d, pid)?;
    link.show_menu()?;
    let entry = look(d, ms(150), || status_entry(d, pid))?.ok_or(UiError::NoStatusMenu)?;
    entry.press()?;
    let menu = look(d, ms(150), || status_menu(d, pid))?.ok_or(UiError::NoStatusMenu)?;
    let Some(item) = menu_items(&menu)
        .into_iter()
        .find(|item| label_is(item, label))
    else {
        // Closing the menus only tidies up; the answer is the missing status either way.
        for _ in 0..2 {
            let _ = post(d, pid, Key::Escape, Modifiers::default());
        }
        return Err(UiError::NoStatus(label.into()));
    };
    item.press()?;
    Ok(())
}

/// Presses `Create` in the New workspace dialog, waiting for the dialog a create link opens. A
/// window that cannot be read yet shows no dialog yet.
fn confirm_create<D: Desktop>(d: &D) -> Result<(), UiError> {
    let pid = checks(d)?;
    let dialog = look_n(d, 20, ms(250), || {
        Ok(read_window(d, pid).ok().and_then(|view| {
            first(&view.window, DIALOG_DEPTH, |node| {
                has_roles(node, "AXGroup", "AXApplicationDialog") && label_is(node, "New workspace")
            })
        }))
    })?
    .ok_or(UiError::NoCreateDialog)?;
    let create = first(&dialog, PANE_DEPTH, |node| {
        has_role(node, "AXButton") && label_is(node, "Create")
    })
    .ok_or(UiError::NoCreateDialog)?;
    create.press()?;
    Ok(())
}

/// Starts (`start`) or stops the target workspace's Run task with the buttons of its Run strip.
/// `task` names the task to start; `None` is the one the strip's button names. A task that is
/// already as asked is left alone, and another one that runs is stopped before the start.
fn run_task<D: Desktop>(
    d: &D,
    target: &Target,
    task: Option<&str>,
    start: bool,
) -> Result<RunOutcome, UiError> {
    if target.branch.is_empty() {
        return Err(UiError::NoBranch);
    }
    let (pid, _) = prepare(d)?;
    focus(d, pid, target)?;
    let strip = current_run_strip(d, pid)?.ok_or(UiError::NoRunStrip)?;
    let running = run_button(&strip, STOP_PREFIX);

    if !start {
        let Some((stop, name)) = running else {
            return Ok(RunOutcome::default());
        };
        ensure_front(d, pid)?;
        stop_run(d, pid, &stop)?;
        return Ok(RunOutcome {
            changed: true,
            task: Some(name),
        });
    }

    match running {
        Some((_, name)) if task.is_none_or(|task| task == name) => {
            return Ok(RunOutcome {
                changed: false,
                task: Some(name),
            });
        }
        Some((stop, _)) => {
            ensure_front(d, pid)?;
            stop_run(d, pid, &stop)?;
        }
        None => ensure_front(d, pid)?,
    }
    press_run(d, pid, task)?;
    let name = look_n(d, RUN_LOOKS, ms(200), || run_shows(d, pid, STOP_PREFIX))?
        .ok_or(UiError::RunNotChanged)?;
    Ok(RunOutcome {
        changed: true,
        task: Some(name),
    })
}
