//! A fake element tree and a fake desktop, for tests: they record what the UI logic does to them
//! and never reach the Mac.
//!
//! Neither type is `Send`: build them on the thread that uses them.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::Duration;

use super::ax::AxError;
use super::desktop::Desktop;
use super::keys::{Key, Modifiers};
use super::node::UiNode;
use super::screen::SessionState;

/// What a fake records, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeEvent {
    OpenUrl(String),
    Activate(i32),
    Key {
        pid: i32,
        key: Key,
        modifiers: Modifiers,
    },
    /// The pressed node's label.
    Press(Option<String>),
    /// The node's label, when `AXShowMenu` is performed on it.
    ShowMenu(Option<String>),
    SetValue {
        label: Option<String>,
        value: String,
    },
    SetFocused {
        label: Option<String>,
        focused: bool,
    },
    Pause(Duration),
}

/// An event log shared by a desktop and every node of its tree.
type Log = Rc<RefCell<Vec<FakeEvent>>>;

type PressReaction = Rc<dyn Fn(&FakeNode)>;
type UrlReaction = Box<dyn FnMut(&str)>;
type KeyReaction = Box<dyn FnMut(Key, Modifiers)>;

/// The state of one fake element.
struct NodeState {
    role: Option<String>,
    subrole: Option<String>,
    label: Option<String>,
    value: Option<String>,
    selected: Option<bool>,
    help: Option<String>,
    flag: Option<bool>,
    focused: bool,
    children: Vec<FakeNode>,
    parent: Weak<RefCell<NodeState>>,
    /// The log this node records into: its own until it is attached to a tree, then the tree's.
    log: Log,
    on_press: Vec<PressReaction>,
    on_show_menu: Vec<PressReaction>,
    ignore_value_writes: bool,
    fail_children: Option<AxError>,
    fail_value: Option<AxError>,
}

/// One fake element. Clones share the same element.
#[derive(Clone)]
pub struct FakeNode {
    state: Rc<RefCell<NodeState>>,
}

impl FakeNode {
    pub fn new(role: &str) -> FakeNode {
        FakeNode {
            state: Rc::new(RefCell::new(NodeState {
                role: Some(role.to_owned()),
                subrole: None,
                label: None,
                value: None,
                selected: None,
                help: None,
                flag: None,
                focused: false,
                children: Vec::new(),
                parent: Weak::new(),
                log: Log::default(),
                on_press: Vec::new(),
                on_show_menu: Vec::new(),
                ignore_value_writes: false,
                fail_children: None,
                fail_value: None,
            })),
        }
    }

    pub fn with_subrole(self, subrole: &str) -> FakeNode {
        self.state.borrow_mut().subrole = Some(subrole.to_owned());
        self
    }

    pub fn with_label(self, label: &str) -> FakeNode {
        self.set_label(label);
        self
    }

    pub fn with_value(self, value: &str) -> FakeNode {
        self.set_value_text(Some(value));
        self
    }

    pub fn with_selected(self, selected: bool) -> FakeNode {
        self.set_selected(selected);
        self
    }

    pub fn with_help(self, help: &str) -> FakeNode {
        self.state.borrow_mut().help = Some(help.to_owned());
        self
    }

    pub fn with_flag(self, flag: bool) -> FakeNode {
        self.set_flag(flag);
        self
    }

    /// Appends `child`, sets its parent to this node, and points its whole subtree at this
    /// node's log.
    pub fn with_child(self, child: FakeNode) -> FakeNode {
        self.add_child(child);
        self
    }

    /// The same as [`FakeNode::with_child`], on a built tree.
    pub fn add_child(&self, child: FakeNode) {
        let log = self.log();
        child.state.borrow_mut().parent = Rc::downgrade(&self.state);
        child.share_log(&log);
        self.state.borrow_mut().children.push(child);
    }

    pub fn set_label(&self, label: &str) {
        self.state.borrow_mut().label = Some(label.to_owned());
    }

    /// Changes the value without an event.
    pub fn set_value_text(&self, value: Option<&str>) {
        self.state.borrow_mut().value = value.map(str::to_owned);
    }

    pub fn set_selected(&self, selected: bool) {
        self.state.borrow_mut().selected = Some(selected);
    }

    /// Changes the boolean `AXValue` without an event.
    pub fn set_flag(&self, flag: bool) {
        self.state.borrow_mut().flag = Some(flag);
    }

    pub fn flag_value(&self) -> Option<bool> {
        self.state.borrow().flag
    }

    /// Removes `child`, the child that is the same element (`Rc::ptr_eq`), and detaches it from
    /// this node. Nothing happens when it is not a child.
    pub fn remove_child(&self, child: &FakeNode) {
        let position = self
            .state
            .borrow()
            .children
            .iter()
            .position(|candidate| Rc::ptr_eq(&candidate.state, &child.state));
        if let Some(position) = position {
            let removed = self.state.borrow_mut().children.remove(position);
            removed.state.borrow_mut().parent = Weak::new();
        }
    }

    /// The children, in order, ignoring `fail_children`.
    pub fn child_nodes(&self) -> Vec<FakeNode> {
        self.state.borrow().children.clone()
    }

    /// Runs on every `press`, after the event is recorded. Each reaction added runs, in the order
    /// they were added.
    pub fn on_press(&self, reaction: impl Fn(&FakeNode) + 'static) {
        self.state.borrow_mut().on_press.push(Rc::new(reaction));
    }

    /// Runs on every `show_menu`, after the event is recorded. Each reaction added runs, in the
    /// order they were added.
    pub fn on_show_menu(&self, reaction: impl Fn(&FakeNode) + 'static) {
        self.state.borrow_mut().on_show_menu.push(Rc::new(reaction));
    }

    /// `set_value` records its event and then changes nothing (the app ignored the write).
    pub fn ignore_value_writes(&self) {
        self.state.borrow_mut().ignore_value_writes = true;
    }

    /// Every later `children()` call fails with `error`.
    pub fn fail_children(&self, error: AxError) {
        self.state.borrow_mut().fail_children = Some(error);
    }

    /// Every later `value()` call fails with `error` (an element that went away).
    pub fn fail_value(&self, error: AxError) {
        self.state.borrow_mut().fail_value = Some(error);
    }

    pub fn parent(&self) -> Option<FakeNode> {
        self.state
            .borrow()
            .parent
            .upgrade()
            .map(|state| FakeNode { state })
    }

    /// Breadth-first, this node included: the first node whose label equals `label`.
    pub fn find_label(&self, label: &str) -> Option<FakeNode> {
        self.find(|state| state.label.as_deref() == Some(label))
    }

    /// Breadth-first, this node included: the first node with this role.
    pub fn find_role(&self, role: &str) -> Option<FakeNode> {
        self.find(|state| state.role.as_deref() == Some(role))
    }

    pub fn label_text(&self) -> Option<String> {
        self.state.borrow().label.clone()
    }

    pub fn value_text(&self) -> Option<String> {
        self.state.borrow().value.clone()
    }

    pub fn is_selected(&self) -> bool {
        self.state.borrow().selected.unwrap_or(false)
    }

    pub fn is_focused(&self) -> bool {
        self.state.borrow().focused
    }

    /// Breadth-first over the tree, ignoring `fail_children`.
    fn find(&self, matches: impl Fn(&NodeState) -> bool) -> Option<FakeNode> {
        let mut queue = VecDeque::from([self.clone()]);
        while let Some(node) = queue.pop_front() {
            if matches(&node.state.borrow()) {
                return Some(node);
            }
            queue.extend(node.state.borrow().children.iter().cloned());
        }
        None
    }

    fn log(&self) -> Log {
        Rc::clone(&self.state.borrow().log)
    }

    /// Points this node and every node below it at `log`.
    fn share_log(&self, log: &Log) {
        self.state.borrow_mut().log = Rc::clone(log);
        let children = self.state.borrow().children.clone();
        for child in &children {
            child.share_log(log);
        }
    }

    fn record(&self, event: FakeEvent) {
        self.log().borrow_mut().push(event);
    }
}

impl UiNode for FakeNode {
    fn role(&self) -> Option<String> {
        self.state.borrow().role.clone()
    }

    fn subrole(&self) -> Option<String> {
        self.state.borrow().subrole.clone()
    }

    fn label(&self) -> Option<String> {
        self.label_text()
    }

    fn value(&self) -> Result<Option<String>, AxError> {
        let state = self.state.borrow();
        match state.fail_value {
            Some(error) => Err(error),
            None => Ok(state.value.clone()),
        }
    }

    fn selected(&self) -> Option<bool> {
        self.state.borrow().selected
    }

    fn help(&self) -> Option<String> {
        self.state.borrow().help.clone()
    }

    fn flag(&self) -> Option<bool> {
        self.state.borrow().flag
    }

    fn children(&self) -> Result<Vec<FakeNode>, AxError> {
        let state = self.state.borrow();
        match state.fail_children {
            Some(error) => Err(error),
            None => Ok(state.children.clone()),
        }
    }

    fn press(&self) -> Result<(), AxError> {
        self.record(FakeEvent::Press(self.label_text()));
        // Cloned out first: a reaction may change this node.
        let reactions = self.state.borrow().on_press.clone();
        for reaction in reactions {
            reaction(self);
        }
        Ok(())
    }

    fn show_menu(&self) -> Result<(), AxError> {
        self.record(FakeEvent::ShowMenu(self.label_text()));
        // Cloned out first: a reaction may change this node.
        let reactions = self.state.borrow().on_show_menu.clone();
        for reaction in reactions {
            reaction(self);
        }
        Ok(())
    }

    fn set_value(&self, text: &str) -> Result<(), AxError> {
        self.record(FakeEvent::SetValue {
            label: self.label_text(),
            value: text.to_owned(),
        });
        let mut state = self.state.borrow_mut();
        if !state.ignore_value_writes {
            state.value = Some(text.to_owned());
        }
        Ok(())
    }

    fn set_focused(&self, focused: bool) -> Result<(), AxError> {
        self.record(FakeEvent::SetFocused {
            label: self.label_text(),
            focused,
        });
        self.state.borrow_mut().focused = focused;
        Ok(())
    }
}

/// A fake Mac with one app tree and one event log.
pub struct FakeDesktop {
    app: FakeNode,
    log: Log,
    trusted: Cell<bool>,
    session: Cell<Option<SessionState>>,
    conductor_pid: Cell<Option<i32>>,
    frontmost: Cell<Option<i32>>,
    refuse_activation: Cell<bool>,
    key_failure: RefCell<Option<String>>,
    on_open_url: RefCell<Vec<UrlReaction>>,
    on_key: RefCell<Vec<KeyReaction>>,
}

/// The pid a `FakeDesktop` gives Conductor until told otherwise.
const CONDUCTOR_PID: i32 = 4242;

impl FakeDesktop {
    /// Trusted, unlocked and on console, Conductor running as pid 4242 and frontmost. Every node
    /// of `app`, and every node added later, records into this desktop's log.
    pub fn new(app: FakeNode) -> FakeDesktop {
        let log = Log::default();
        app.share_log(&log);
        FakeDesktop {
            app,
            log,
            trusted: Cell::new(true),
            session: Cell::new(Some(SessionState {
                locked: false,
                on_console: true,
            })),
            conductor_pid: Cell::new(Some(CONDUCTOR_PID)),
            frontmost: Cell::new(Some(CONDUCTOR_PID)),
            refuse_activation: Cell::new(false),
            key_failure: RefCell::new(None),
            on_open_url: RefCell::new(Vec::new()),
            on_key: RefCell::new(Vec::new()),
        }
    }

    pub fn app(&self) -> FakeNode {
        self.app.clone()
    }

    pub fn set_trusted(&self, trusted: bool) {
        self.trusted.set(trusted);
    }

    pub fn set_session(&self, session: Option<SessionState>) {
        self.session.set(session);
    }

    pub fn set_conductor_pid(&self, pid: Option<i32>) {
        self.conductor_pid.set(pid);
    }

    pub fn set_frontmost(&self, pid: Option<i32>) {
        self.frontmost.set(pid);
    }

    /// `activate` returns false and changes nothing.
    pub fn refuse_activation(&self) {
        self.refuse_activation.set(true);
    }

    /// Runs on every `open_url`, after the event is recorded. Each reaction added runs, in the
    /// order they were added.
    pub fn on_open_url(&self, reaction: impl FnMut(&str) + 'static) {
        self.on_open_url.borrow_mut().push(Box::new(reaction));
    }

    /// Runs on every `post_key`, after the event is recorded. Each reaction added runs, in the
    /// order they were added.
    pub fn on_key(&self, reaction: impl FnMut(Key, Modifiers) + 'static) {
        self.on_key.borrow_mut().push(Box::new(reaction));
    }

    /// Every later `post_key` fails with this message.
    pub fn fail_keys(&self, message: &str) {
        *self.key_failure.borrow_mut() = Some(message.to_owned());
    }

    pub fn events(&self) -> Vec<FakeEvent> {
        self.log.borrow().clone()
    }

    fn record(&self, event: FakeEvent) {
        self.log.borrow_mut().push(event);
    }
}

/// Runs every reaction in `slot` with `run`. The reactions are taken out while they run, so one
/// may add another (it runs from the next call on).
fn run_reactions<R>(slot: &RefCell<Vec<R>>, mut run: impl FnMut(&mut R)) {
    let mut reactions = std::mem::take(&mut *slot.borrow_mut());
    for reaction in &mut reactions {
        run(reaction);
    }
    let mut slot = slot.borrow_mut();
    let added = std::mem::replace(&mut *slot, reactions);
    slot.extend(added);
}

impl Desktop for FakeDesktop {
    type Node = FakeNode;

    fn trusted(&self) -> bool {
        self.trusted.get()
    }

    fn session(&self) -> Option<SessionState> {
        self.session.get()
    }

    fn conductor_pid(&self) -> Option<i32> {
        self.conductor_pid.get()
    }

    /// `app()`, whatever the pid.
    fn application(&self, _pid: i32) -> FakeNode {
        self.app()
    }

    /// Records the URL, runs the reactions and returns true.
    fn open_url(&self, url: &str) -> bool {
        self.record(FakeEvent::OpenUrl(url.to_owned()));
        run_reactions(&self.on_open_url, |reaction| reaction(url));
        true
    }

    fn frontmost_pid(&self) -> Option<i32> {
        self.frontmost.get()
    }

    /// Records `Activate(pid)`; unless refused, makes `pid` frontmost and returns true.
    fn activate(&self, pid: i32) -> bool {
        self.record(FakeEvent::Activate(pid));
        if self.refuse_activation.get() {
            return false;
        }
        self.frontmost.set(Some(pid));
        true
    }

    /// Records the key, then fails when told to, else runs the reactions.
    fn post_key(&self, pid: i32, key: Key, modifiers: Modifiers) -> Result<(), String> {
        self.record(FakeEvent::Key {
            pid,
            key,
            modifiers,
        });
        if let Some(message) = self.key_failure.borrow().clone() {
            return Err(message);
        }
        run_reactions(&self.on_key, |reaction| reaction(key, modifiers));
        Ok(())
    }

    /// Records `Pause(duration)` and returns at once.
    fn pause(&self, duration: Duration) {
        self.record(FakeEvent::Pause(duration));
    }
}

/// The shape of a Conductor window, for `conductor_app`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowSpec {
    pub repo: String,
    pub branch: String,
    /// Workspace names in the sidebar.
    pub sidebar: Vec<String>,
    /// Chat titles, in tab order.
    pub chats: Vec<String>,
    /// 0-based index of the selected chat; ignored when `chats` is empty.
    pub selected: usize,
    pub composer_value: Option<String>,
}

/// The label of the composer's text area.
const COMPOSER_LABEL: &str = "Ask to make changes, @mention files, run /commands";

/// An application node shaped like the live probe: the app → `AXWindow` "Conductor" →
/// `AXGroup` → `AXGroup` → `AXScrollArea` → `AXWebArea`, which holds
/// (1) `AXGroup`/`AXLandmarkComplementary` with one `AXLink` labelled with each sidebar name, and
/// (2) `AXGroup`/`AXLandmarkMain` with, in order: `AXPopUpButton` labelled "<repo> <repo>"
///     (the repository name twice, as the live window shows it), an unlabelled `AXStaticText`
///     whose value is the tail of `branch` (the part after its last `/`), `AXTabGroup` →
///     `AXGroup` → one `AXRadioButton` per chat labelled "Close chat <title>" (selected as
///     given; pressing one selects it and deselects its siblings), an unlabelled `AXButton`,
///     then `AXGroup`/`AXLandmarkForm` labelled "composer" → `AXTextArea` labelled
///     "Ask to make changes, @mention files, run /commands" with `composer_value`, and an
///     unlabelled `AXButton`; and (3) a second `AXTabGroup` → `AXRadioButton` "Setup".
///
/// The unlabelled `AXButton` after the chat strip is a child of the main pane; the second one
/// sits in the composer group after the text area. The right-panel `AXTabGroup` of (3) is the
/// main pane's last child, and its "Setup" radio is selected.
pub fn conductor_app(spec: &WindowSpec) -> FakeNode {
    let sidebar = spec.sidebar.iter().fold(
        FakeNode::new("AXGroup").with_subrole("AXLandmarkComplementary"),
        |sidebar, name| sidebar.with_child(FakeNode::new("AXLink").with_label(name)),
    );

    let strip = FakeNode::new("AXGroup");
    for (index, title) in spec.chats.iter().enumerate() {
        let radio = FakeNode::new("AXRadioButton")
            .with_label(&format!("Close chat {title}"))
            .with_selected(index == spec.selected);
        radio.on_press(select_among_siblings);
        strip.add_child(radio);
    }

    let mut text_area = FakeNode::new("AXTextArea").with_label(COMPOSER_LABEL);
    if let Some(value) = &spec.composer_value {
        text_area = text_area.with_value(value);
    }
    let composer = FakeNode::new("AXGroup")
        .with_subrole("AXLandmarkForm")
        .with_label("composer")
        .with_child(text_area)
        .with_child(FakeNode::new("AXButton"));

    let setup = FakeNode::new("AXRadioButton")
        .with_label("Setup")
        .with_selected(true);
    setup.on_press(select_among_siblings);

    let main = FakeNode::new("AXGroup")
        .with_subrole("AXLandmarkMain")
        .with_child(FakeNode::new("AXPopUpButton").with_label(&repo_label(&spec.repo)))
        .with_child(FakeNode::new("AXStaticText").with_value(branch_tail(&spec.branch)))
        .with_child(FakeNode::new("AXTabGroup").with_child(strip))
        .with_child(FakeNode::new("AXButton"))
        .with_child(composer)
        .with_child(FakeNode::new("AXTabGroup").with_child(setup));

    let web_area = FakeNode::new("AXWebArea")
        .with_child(sidebar)
        .with_child(main);
    let window = FakeNode::new("AXWindow")
        .with_label("Conductor")
        .with_child(FakeNode::new("AXGroup").with_child(
            FakeNode::new("AXGroup").with_child(FakeNode::new("AXScrollArea").with_child(web_area)),
        ));
    FakeNode::new("AXApplication")
        .with_label("Conductor")
        .with_child(window)
}

/// The pop-up label the live window gives a repository: its name twice.
fn repo_label(repo: &str) -> String {
    format!("{repo} {repo}")
}

/// The part of `branch` after its last `/`.
fn branch_tail(branch: &str) -> &str {
    branch.rsplit('/').next().unwrap_or("")
}

/// Selects `node` and deselects every other radio under its parent.
fn select_among_siblings(node: &FakeNode) {
    let Some(parent) = node.parent() else {
        node.set_selected(true);
        return;
    };
    let siblings = parent.state.borrow().children.clone();
    for sibling in &siblings {
        if Rc::ptr_eq(&sibling.state, &node.state) {
            sibling.set_selected(true);
        } else if sibling.state.borrow().selected.is_some() {
            sibling.set_selected(false);
        }
    }
}

/// The main pane (`AXLandmarkMain`) of a `conductor_app` tree, for tests that change it.
pub fn main_pane(app: &FakeNode) -> FakeNode {
    app.find(|state| state.subrole.as_deref() == Some("AXLandmarkMain"))
        .expect("a conductor_app tree has an AXLandmarkMain pane")
}

/// Makes a `conductor_app` tree show the workspace of `repo` on `branch`, as Conductor does: the
/// main pane's first `AXPopUpButton` is labelled "<repo> <repo>" and its first direct
/// `AXStaticText` child holds the branch's tail.
pub fn show_workspace(app: &FakeNode, repo: &str, branch: &str) {
    let pane = main_pane(app);
    pane.find_role("AXPopUpButton")
        .expect("a conductor_app pane has a pop-up")
        .set_label(&repo_label(repo));
    let children = pane.state.borrow().children.clone();
    children
        .iter()
        .find(|child| child.state.borrow().role.as_deref() == Some("AXStaticText"))
        .expect("a conductor_app pane has a static text")
        .set_value_text(Some(branch_tail(branch)));
}

/// The agent controls to add to a `conductor_app` composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMenuSpec {
    /// The configured models in menu order: (name, effort label, fast).
    pub models: Vec<(String, String, bool)>,
    /// The index in `models` of the chat's model.
    pub current: usize,
    /// The names of the models that open a new chat when picked.
    pub new_chat_models: Vec<String>,
    /// The items of the Effort submenu; empty: the model menu has no Effort entry.
    pub efforts: Vec<String>,
    /// Whether the model menu has a Fast item.
    pub fast_item: bool,
    /// Plan mode; `None`: the Add menu has no Plan item.
    pub plan: Option<bool>,
}

/// A menu that is in the tree: the wrapper chain's top node, and the `AXMenu` at its bottom.
struct OpenMenu {
    wrapper: FakeNode,
    menu: FakeNode,
}

type NewChatReaction = Rc<dyn Fn(&str)>;

/// The state of the fake agent controls.
struct MenusState {
    models: RefCell<Vec<(String, String, bool)>>,
    current: Cell<usize>,
    new_chat_models: Vec<String>,
    efforts: Vec<String>,
    fast_item: bool,
    plan: Cell<Option<bool>>,
    /// The `Change agent` pop-up.
    popup: FakeNode,
    /// The composer child the open menus hang from.
    slot: FakeNode,
    model_menu: RefCell<Option<OpenMenu>>,
    effort_menu: RefCell<Option<OpenMenu>>,
    effort_item: RefCell<Option<FakeNode>>,
    fast_node: RefCell<Option<FakeNode>>,
    add_menu: RefCell<Option<OpenMenu>>,
    on_new_chat: RefCell<Vec<NewChatReaction>>,
    ignore_model_presses: Cell<bool>,
    ignore_effort_presses: Cell<bool>,
    keep_menus_open: Cell<bool>,
}

/// A handle on the fake agent controls. Clones share the same controls.
#[derive(Clone)]
pub struct AgentMenus {
    state: Rc<MenusState>,
}

/// Adds the `Change agent` and `Add` pop-ups to the composer group of a `conductor_app` tree.
///
/// The composer group gets two new children: an `AXGroup` holding the pop-ups, and an `AXGroup`
/// the open menus hang from (menu → `AXGroup`/`AXApplicationGroup` → `AXGroup` → `AXMenu`).
///
/// # Panics
///
/// When `app` has no composer group, or `spec.current` is not an index of `spec.models`.
pub fn add_agent_menus(app: &FakeNode, spec: &AgentMenuSpec) -> AgentMenus {
    assert!(
        spec.current < spec.models.len(),
        "the current model must be one of the models"
    );
    let composer = app
        .find(|state| state.subrole.as_deref() == Some("AXLandmarkForm"))
        .expect("a conductor_app tree has a composer group");
    let popup = FakeNode::new("AXPopUpButton");
    let add = FakeNode::new("AXPopUpButton").with_label("Add");
    let slot = FakeNode::new("AXGroup");
    let state = Rc::new(MenusState {
        models: RefCell::new(spec.models.clone()),
        current: Cell::new(spec.current),
        new_chat_models: spec.new_chat_models.clone(),
        efforts: spec.efforts.clone(),
        fast_item: spec.fast_item,
        plan: Cell::new(spec.plan),
        popup: popup.clone(),
        slot: slot.clone(),
        model_menu: RefCell::new(None),
        effort_menu: RefCell::new(None),
        effort_item: RefCell::new(None),
        fast_node: RefCell::new(None),
        add_menu: RefCell::new(None),
        on_new_chat: RefCell::new(Vec::new()),
        ignore_model_presses: Cell::new(false),
        ignore_effort_presses: Cell::new(false),
        keep_menus_open: Cell::new(false),
    });
    state.refresh_popup();
    let pressed = Rc::clone(&state);
    popup.on_press(move |_| pressed.press_agent_popup());
    let pressed = Rc::clone(&state);
    add.on_press(move |_| pressed.press_add_popup());
    composer.add_child(FakeNode::new("AXGroup").with_child(popup).with_child(add));
    composer.add_child(slot);
    AgentMenus { state }
}

impl AgentMenus {
    /// The `Change agent` pop-up's label.
    pub fn shown(&self) -> String {
        self.state.popup.label_text().unwrap_or_default()
    }

    /// Plan mode as the Add menu would show it.
    pub fn plan(&self) -> Option<bool> {
        self.state.plan.get()
    }

    /// Whether the model menu or the Add menu is open.
    pub fn menu_open(&self) -> bool {
        self.state.model_menu.borrow().is_some() || self.state.add_menu.borrow().is_some()
    }

    /// Runs with the model's name whenever a model listed in `new_chat_models` is picked.
    pub fn on_new_chat(&self, reaction: impl Fn(&str) + 'static) {
        self.state.on_new_chat.borrow_mut().push(Rc::new(reaction));
    }

    /// From now on a press on a model item still closes the menu but changes nothing.
    pub fn ignore_model_presses(&self) {
        self.state.ignore_model_presses.set(true);
    }

    /// From now on a press on an Effort submenu item closes the submenu but changes nothing.
    pub fn ignore_effort_presses(&self) {
        self.state.ignore_effort_presses.set(true);
    }

    /// From now on a press on a pop-up no longer closes its open menu.
    pub fn keep_menus_open(&self) {
        self.state.keep_menus_open.set(true);
    }
}

/// `<head> <tail>`, or `<head>` alone when `tail` is empty.
fn spaced(head: &str, tail: &str) -> String {
    if tail.is_empty() {
        head.to_owned()
    } else {
        format!("{head} {tail}")
    }
}

/// Puts `menu` under `parent` as the live window does: `AXGroup`/`AXApplicationGroup` →
/// `AXGroup` → `AXMenu`.
fn mount_menu(parent: &FakeNode, menu: FakeNode) -> OpenMenu {
    let wrapper = FakeNode::new("AXGroup")
        .with_subrole("AXApplicationGroup")
        .with_child(FakeNode::new("AXGroup").with_child(menu.clone()));
    parent.add_child(wrapper.clone());
    OpenMenu { wrapper, menu }
}

impl MenusState {
    /// The current model as (name, effort, fast).
    fn current_model(&self) -> (String, String, bool) {
        self.models.borrow()[self.current.get()].clone()
    }

    fn popup_label(&self) -> String {
        let (name, effort, fast) = self.current_model();
        let mut parts = vec![name];
        if !effort.is_empty() {
            parts.push(effort);
        }
        if fast {
            parts.push("Fast".to_string());
        }
        format!("Change agent ({})", parts.join(" · "))
    }

    fn refresh_popup(&self) {
        self.popup.set_label(&self.popup_label());
    }

    fn press_agent_popup(self: &Rc<Self>) {
        let open = self.model_menu.borrow().is_some();
        if !open {
            self.open_model_menu();
        } else if !self.keep_menus_open.get() {
            self.close_model_menu();
        }
    }

    fn press_add_popup(self: &Rc<Self>) {
        let open = self.add_menu.borrow().is_some();
        if !open {
            self.open_add_menu();
        } else if !self.keep_menus_open.get() {
            self.close_add_menu();
        }
    }

    fn open_model_menu(self: &Rc<Self>) {
        let menu = FakeNode::new("AXMenu").with_label(&self.popup_label());
        let current = self.current.get();
        let models = self.models.borrow().clone();
        for (index, (name, effort, fast)) in models.iter().enumerate() {
            let mut label = spaced(name, effort);
            if *fast {
                label.push_str(" Fast mode on");
            }
            if index != current && self.new_chat_models.contains(name) {
                label.push_str(" Opens in new chat");
            }
            let item = FakeNode::new("AXMenuItem")
                .with_label(&label)
                .with_help(name);
            let state = Rc::clone(self);
            item.on_press(move |_| state.pick_model(index));
            menu.add_child(item);
        }
        menu.add_child(FakeNode::new("AXMenuItem").with_label("Add models"));
        let (_, effort, fast) = &models[current];
        if !self.efforts.is_empty() {
            let item = FakeNode::new("AXMenuItem").with_label(&spaced("Effort", effort));
            let state = Rc::clone(self);
            item.on_press(move |_| state.open_effort_menu());
            menu.add_child(item.clone());
            *self.effort_item.borrow_mut() = Some(item);
        }
        if self.fast_item {
            let item = FakeNode::new("AXMenuItem")
                .with_label("Fast")
                .with_flag(*fast);
            let state = Rc::clone(self);
            item.on_press(move |_| state.toggle_fast());
            menu.add_child(item.clone());
            *self.fast_node.borrow_mut() = Some(item);
        }
        *self.model_menu.borrow_mut() = Some(mount_menu(&self.slot, menu));
    }

    /// Closes the model menu and, with it, a nested Effort menu.
    fn close_model_menu(&self) {
        let Some(open) = self.model_menu.borrow_mut().take() else {
            return;
        };
        self.slot.remove_child(&open.wrapper);
        self.effort_menu.borrow_mut().take();
        self.effort_item.borrow_mut().take();
        self.fast_node.borrow_mut().take();
    }

    fn pick_model(&self, index: usize) {
        self.close_model_menu();
        if self.ignore_model_presses.get() {
            return;
        }
        self.current.set(index);
        self.refresh_popup();
        let (name, _, _) = self.current_model();
        if self.new_chat_models.contains(&name) {
            // Cloned out first: a reaction may add another.
            let reactions = self.on_new_chat.borrow().clone();
            for reaction in reactions {
                reaction(&name);
            }
        }
    }

    fn open_effort_menu(self: &Rc<Self>) {
        if self.effort_menu.borrow().is_some() {
            return;
        }
        let Some(parent) = self
            .model_menu
            .borrow()
            .as_ref()
            .map(|open| open.menu.clone())
        else {
            return;
        };
        let (_, effort, _) = self.current_model();
        let menu = FakeNode::new("AXMenu").with_label(&spaced("Effort", &effort));
        for label in &self.efforts {
            let item = FakeNode::new("AXMenuItem").with_label(label);
            let state = Rc::clone(self);
            let chosen = label.clone();
            item.on_press(move |_| state.pick_effort(&chosen));
            menu.add_child(item);
        }
        *self.effort_menu.borrow_mut() = Some(mount_menu(&parent, menu));
    }

    fn pick_effort(&self, effort: &str) {
        if let Some(open) = self.effort_menu.borrow_mut().take() {
            if let Some(parent) = open.wrapper.parent() {
                parent.remove_child(&open.wrapper);
            }
        }
        if self.ignore_effort_presses.get() {
            return;
        }
        self.models.borrow_mut()[self.current.get()].1 = effort.to_owned();
        if let Some(item) = self.effort_item.borrow().as_ref() {
            item.set_label(&spaced("Effort", effort));
        }
        self.refresh_popup();
    }

    fn toggle_fast(&self) {
        let fast = {
            let mut models = self.models.borrow_mut();
            let model = &mut models[self.current.get()];
            model.2 = !model.2;
            model.2
        };
        if let Some(item) = self.fast_node.borrow().as_ref() {
            item.set_flag(fast);
        }
        self.refresh_popup();
    }

    fn open_add_menu(self: &Rc<Self>) {
        let menu = FakeNode::new("AXMenu").with_label("Add");
        let item = |label: &str| FakeNode::new("AXMenuItem").with_label(label);
        menu.add_child(item("Link issue ⌘ I"));
        if let Some(plan) = self.plan.get() {
            let plan_item = item(if plan {
                "Exit plan mode ⇧ Tab"
            } else {
                "Plan mode ⇧ Tab"
            });
            let state = Rc::clone(self);
            plan_item.on_press(move |_| {
                state.plan.set(state.plan.get().map(|plan| !plan));
                state.close_add_menu();
            });
            menu.add_child(plan_item);
        }
        menu.add_child(item("Link workspaces"));
        menu.add_child(item("Add attachment ⌘ U"));
        *self.add_menu.borrow_mut() = Some(mount_menu(&self.slot, menu));
    }

    fn close_add_menu(&self) {
        if let Some(open) = self.add_menu.borrow_mut().take() {
            self.slot.remove_child(&open.wrapper);
        }
    }
}

/// What the fake Conductor shows around a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceUiSpec {
    /// The titles of the chats whose agent is running (Conductor asks before closing them and
    /// before archiving).
    pub running: Vec<String>,
    /// Whether the main pane shows a Continue button.
    pub continue_button: bool,
}

/// The prefix of a chat radio's label.
const CHAT_PREFIX: &str = "Close chat ";

/// The items of the row menu, in order.
const ROW_MENU_ITEMS: [&str; 7] = [
    "Mark as unread R",
    "Pin P",
    "Set status",
    "Move to section",
    "Rename",
    "Copy link ⌘⇧C",
    "Archive ⌘⇧A",
];

/// The items of the nested Set status menu, in order.
const STATUS_ITEMS: [&str; 5] = ["Backlog", "In progress", "In review", "Done", "Canceled"];

type CloseReaction = Rc<dyn Fn(&str)>;
type StatusReaction = Rc<dyn Fn(&str, &str)>;

/// A row menu that is in the tree.
struct OpenRowMenu {
    wrapper: FakeNode,
    menu: FakeNode,
}

/// The state of the fake workspace controls.
struct WorkspaceState {
    web_area: FakeNode,
    main: FakeNode,
    /// The `AXGroup` that holds the chat radios.
    strip: FakeNode,
    running: RefCell<Vec<String>>,
    closed: RefCell<Vec<String>>,
    archived: Cell<bool>,
    status: RefCell<Option<(String, String)>>,
    continued: Cell<bool>,
    created: Cell<usize>,
    /// The wrapper of the open alert or New workspace dialog.
    dialog: RefCell<Option<FakeNode>>,
    row_menu: RefCell<Option<OpenRowMenu>>,
    /// The wrapper of the open nested Set status menu.
    status_menu: RefCell<Option<FakeNode>>,
    on_close: RefCell<Vec<CloseReaction>>,
    on_archive: RefCell<Vec<Rc<dyn Fn()>>>,
    on_status: RefCell<Vec<StatusReaction>>,
    on_continue: RefCell<Vec<Rc<dyn Fn()>>>,
    on_create: RefCell<Vec<Rc<dyn Fn()>>>,
}

/// A handle on the fake workspace controls. Clones share the same state.
#[derive(Clone)]
pub struct WorkspaceUi {
    state: Rc<WorkspaceState>,
}

/// Wires the workspace controls into a `FakeDesktop` whose app is a `conductor_app` tree: key
/// reactions for Cmd+W and Cmd+Shift+A, row menus on every sidebar link, the Continue button,
/// and the New workspace dialog for `conductor://` links that are not `conductor://workspace…`.
///
/// Alert dialogs hang at `AXWebArea` → `AXGroup` → `AXGroup` → dialog, row menus at `AXWebArea` →
/// `AXGroup` → `AXMenu` (the Set status menu at row menu → `AXGroup` → `AXMenu`), and the New
/// workspace dialog at `AXWebArea` → `AXGroup` → dialog. A second dialog replaces the first, a
/// second row menu the first.
///
/// # Panics
///
/// When the desktop's app is not a `conductor_app` tree.
pub fn add_workspace_ui(desktop: &FakeDesktop, spec: &WorkspaceUiSpec) -> WorkspaceUi {
    let app = desktop.app();
    let web_area = app
        .find_role("AXWebArea")
        .expect("a conductor_app tree has an AXWebArea");
    let main = main_pane(&app);
    let strip = main
        .find_role("AXTabGroup")
        .and_then(|tabs| tabs.child_nodes().into_iter().next())
        .expect("a conductor_app pane has a chat strip");
    let sidebar = app
        .find(|state| state.subrole.as_deref() == Some("AXLandmarkComplementary"))
        .expect("a conductor_app tree has a sidebar");
    let state = Rc::new(WorkspaceState {
        web_area,
        main,
        strip,
        running: RefCell::new(spec.running.clone()),
        closed: RefCell::new(Vec::new()),
        archived: Cell::new(false),
        status: RefCell::new(None),
        continued: Cell::new(false),
        created: Cell::new(0),
        dialog: RefCell::new(None),
        row_menu: RefCell::new(None),
        status_menu: RefCell::new(None),
        on_close: RefCell::new(Vec::new()),
        on_archive: RefCell::new(Vec::new()),
        on_status: RefCell::new(Vec::new()),
        on_continue: RefCell::new(Vec::new()),
        on_create: RefCell::new(Vec::new()),
    });

    let keys = Rc::clone(&state);
    desktop.on_key(move |key, modifiers| keys.handle_key(key, modifiers));
    let urls = Rc::clone(&state);
    desktop.on_open_url(move |url| urls.handle_url(url));
    for link in sidebar.child_nodes() {
        let state = Rc::clone(&state);
        link.on_show_menu(move |link| state.open_row_menu(link));
    }
    if spec.continue_button {
        state.add_continue_button();
    }
    WorkspaceUi { state }
}

impl WorkspaceUi {
    /// The titles of the chats closed so far, in order.
    pub fn closed(&self) -> Vec<String> {
        self.state.closed.borrow().clone()
    }

    pub fn archived(&self) -> bool {
        self.state.archived.get()
    }

    /// The last status set: (the row's title, the item's label).
    pub fn status(&self) -> Option<(String, String)> {
        self.state.status.borrow().clone()
    }

    pub fn continued(&self) -> bool {
        self.state.continued.get()
    }

    /// How many times Create was pressed.
    pub fn created(&self) -> usize {
        self.state.created.get()
    }

    /// Whether an alert dialog or the New workspace dialog is showing.
    pub fn dialog_open(&self) -> bool {
        self.state.dialog.borrow().is_some()
    }

    /// Whether a row menu is open.
    pub fn menu_open(&self) -> bool {
        self.state.row_menu.borrow().is_some()
    }

    /// Run when a chat is closed (its title), the workspace is archived, a status is set (row
    /// title, item label), Continue is pressed, Create is pressed.
    pub fn on_close(&self, reaction: impl Fn(&str) + 'static) {
        self.state.on_close.borrow_mut().push(Rc::new(reaction));
    }

    pub fn on_archive(&self, reaction: impl Fn() + 'static) {
        self.state.on_archive.borrow_mut().push(Rc::new(reaction));
    }

    pub fn on_status(&self, reaction: impl Fn(&str, &str) + 'static) {
        self.state.on_status.borrow_mut().push(Rc::new(reaction));
    }

    pub fn on_continue(&self, reaction: impl Fn() + 'static) {
        self.state.on_continue.borrow_mut().push(Rc::new(reaction));
    }

    pub fn on_create(&self, reaction: impl Fn() + 'static) {
        self.state.on_create.borrow_mut().push(Rc::new(reaction));
    }
}

/// The title of a chat radio.
fn chat_title(radio: &FakeNode) -> Option<String> {
    let label = radio.label_text()?;
    label.strip_prefix(CHAT_PREFIX).map(str::to_owned)
}

impl WorkspaceState {
    fn handle_key(self: &Rc<Self>, key: Key, modifiers: Modifiers) {
        let command_only = Modifiers {
            command: true,
            ..Modifiers::default()
        };
        let command_shift = Modifiers {
            command: true,
            shift: true,
            ..Modifiers::default()
        };
        if key == Key::W && modifiers == command_only {
            self.close_selected_chat();
        } else if key == Key::A && modifiers == command_shift {
            self.archive_workspace();
        } else if key == Key::Escape && modifiers == Modifiers::default() {
            self.close_row_menu();
        }
    }

    fn handle_url(self: &Rc<Self>, url: &str) {
        if url.starts_with("conductor://") && !url.starts_with("conductor://workspace") {
            self.open_new_workspace_dialog();
        }
    }

    /// The chat radios, in order.
    fn chats(&self) -> Vec<FakeNode> {
        self.strip.child_nodes()
    }

    fn selected_title(&self) -> Option<String> {
        self.chats()
            .iter()
            .find(|radio| radio.is_selected())
            .and_then(chat_title)
    }

    fn is_running(&self, title: &str) -> bool {
        self.running.borrow().iter().any(|running| running == title)
    }

    fn close_selected_chat(self: &Rc<Self>) {
        let Some(title) = self.selected_title() else {
            return;
        };
        if self.is_running(&title) {
            let closing = title.clone();
            self.show_alert(
                "Close running chat?",
                "The agent of this chat is still running.",
                "Close anyway ⌘ Enter",
                move |state| {
                    state
                        .running
                        .borrow_mut()
                        .retain(|running| *running != closing);
                    state.close_chat(&closing);
                },
            );
        } else {
            self.close_chat(&title);
        }
    }

    fn close_chat(&self, title: &str) {
        let radio = self
            .chats()
            .into_iter()
            .find(|radio| chat_title(radio).as_deref() == Some(title));
        if let Some(radio) = radio {
            self.strip.remove_child(&radio);
        }
        if let Some(first) = self.chats().first() {
            first.set_selected(true);
        }
        self.closed.borrow_mut().push(title.to_owned());
        // Cloned out first: a reaction may add another.
        let reactions = self.on_close.borrow().clone();
        for reaction in reactions {
            reaction(title);
        }
    }

    fn archive_workspace(self: &Rc<Self>) {
        let any_running = self
            .chats()
            .iter()
            .filter_map(chat_title)
            .any(|title| self.is_running(&title));
        if any_running {
            self.show_alert(
                "Archive workspace?",
                "Some chats of this workspace still have a running agent.",
                "Stop agents and archive ⌘ Enter",
                |state| state.archive(),
            );
        } else {
            self.archive();
        }
    }

    fn archive(&self) {
        self.archived.set(true);
        let reactions = self.on_archive.borrow().clone();
        for reaction in reactions {
            reaction();
        }
    }

    /// Shows an alert dialog with `Cancel` and `confirm_label`: the first removes the dialog, the
    /// second removes it and runs `confirm`.
    fn show_alert(
        self: &Rc<Self>,
        label: &str,
        message: &str,
        confirm_label: &str,
        confirm: impl Fn(&Rc<WorkspaceState>) + 'static,
    ) {
        let cancel = FakeNode::new("AXButton").with_label("Cancel");
        let state = Rc::clone(self);
        cancel.on_press(move |_| state.close_dialog());
        let confirm_button = FakeNode::new("AXButton").with_label(confirm_label);
        let state = Rc::clone(self);
        confirm_button.on_press(move |_| {
            state.close_dialog();
            confirm(&state);
        });
        let dialog = FakeNode::new("AXGroup")
            .with_subrole("AXApplicationAlertDialog")
            .with_label(label)
            .with_child(FakeNode::new("AXStaticText").with_value(message))
            .with_child(cancel)
            .with_child(confirm_button);
        let wrapper =
            FakeNode::new("AXGroup").with_child(FakeNode::new("AXGroup").with_child(dialog));
        self.mount_dialog(wrapper);
    }

    fn open_new_workspace_dialog(self: &Rc<Self>) {
        let create = FakeNode::new("AXButton").with_label("Create");
        let state = Rc::clone(self);
        create.on_press(move |_| {
            state.close_dialog();
            state.created.set(state.created.get() + 1);
            let reactions = state.on_create.borrow().clone();
            for reaction in reactions {
                reaction();
            }
        });
        let composer = FakeNode::new("AXGroup")
            .with_subrole("AXLandmarkForm")
            .with_label("composer")
            .with_child(FakeNode::new("AXTextArea").with_label("What do you want to work on?"))
            .with_child(create);
        let dialog = FakeNode::new("AXGroup")
            .with_subrole("AXApplicationDialog")
            .with_label("New workspace")
            .with_child(composer);
        self.mount_dialog(FakeNode::new("AXGroup").with_child(dialog));
    }

    /// Puts `wrapper` under the web area as the open dialog, replacing an open one.
    fn mount_dialog(&self, wrapper: FakeNode) {
        self.close_dialog();
        self.web_area.add_child(wrapper.clone());
        *self.dialog.borrow_mut() = Some(wrapper);
    }

    fn close_dialog(&self) {
        let open = self.dialog.borrow_mut().take();
        if let Some(wrapper) = open {
            self.web_area.remove_child(&wrapper);
        }
    }

    fn open_row_menu(self: &Rc<Self>, link: &FakeNode) {
        self.close_row_menu();
        let title = link.label_text().unwrap_or_default();
        let menu = FakeNode::new("AXMenu");
        for label in ROW_MENU_ITEMS {
            let item = FakeNode::new("AXMenuItem").with_label(label);
            if label == "Set status" {
                let state = Rc::clone(self);
                let title = title.clone();
                item.on_press(move |_| state.open_status_menu(&title));
            }
            menu.add_child(item);
        }
        let wrapper = FakeNode::new("AXGroup").with_child(menu.clone());
        self.web_area.add_child(wrapper.clone());
        *self.row_menu.borrow_mut() = Some(OpenRowMenu { wrapper, menu });
    }

    fn open_status_menu(self: &Rc<Self>, title: &str) {
        if self.status_menu.borrow().is_some() {
            return;
        }
        let Some(parent) = self
            .row_menu
            .borrow()
            .as_ref()
            .map(|open| open.menu.clone())
        else {
            return;
        };
        let menu = FakeNode::new("AXMenu").with_label("Set status");
        for label in STATUS_ITEMS {
            let item = FakeNode::new("AXMenuItem").with_label(label);
            let state = Rc::clone(self);
            let title = title.to_owned();
            item.on_press(move |_| state.set_status(&title, label));
            menu.add_child(item);
        }
        let wrapper = FakeNode::new("AXGroup").with_child(menu);
        parent.add_child(wrapper.clone());
        *self.status_menu.borrow_mut() = Some(wrapper);
    }

    fn set_status(&self, title: &str, item: &str) {
        self.close_row_menu();
        *self.status.borrow_mut() = Some((title.to_owned(), item.to_owned()));
        let reactions = self.on_status.borrow().clone();
        for reaction in reactions {
            reaction(title, item);
        }
    }

    /// Closes the row menu and, with it, a nested Set status menu.
    fn close_row_menu(&self) {
        let open = self.row_menu.borrow_mut().take();
        if let Some(open) = open {
            self.web_area.remove_child(&open.wrapper);
        }
        self.status_menu.borrow_mut().take();
    }

    fn add_continue_button(self: &Rc<Self>) {
        let button = FakeNode::new("AXButton").with_label("Continue");
        let state = Rc::clone(self);
        button.on_press(move |button| {
            state.continued.set(true);
            state.main.remove_child(button);
            let reactions = state.on_continue.borrow().clone();
            for reaction in reactions {
                reaction();
            }
        });
        self.main.add_child(button);
    }
}

/// The Run strip to add to a `conductor_app` main pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStripSpec {
    /// The task names, in menu order; one task shows no `Select task`.
    pub tasks: Vec<String>,
    /// The index of the task the strip's button names when nothing runs.
    pub selected: usize,
    /// The index of the running task, if one runs.
    pub running: Option<usize>,
}

type TaskReaction = Rc<dyn Fn(&str)>;

/// The state of the fake Run strip.
struct RunStripState {
    /// The `AXTabGroup` that holds the strip and, while it is open, the menu.
    tabs: FakeNode,
    tasks: Vec<String>,
    selected: Cell<usize>,
    running: Cell<Option<usize>>,
    starts: Cell<usize>,
    stops: Cell<usize>,
    ignore_presses: Cell<bool>,
    /// The button and the pop-up, the children of `tabs` that a start or a stop rebuilds.
    controls: RefCell<Vec<FakeNode>>,
    menu: RefCell<Option<OpenMenu>>,
    on_start: RefCell<Vec<TaskReaction>>,
    on_stop: RefCell<Vec<TaskReaction>>,
}

/// A handle on the fake Run strip. Clones share the same strip.
#[derive(Clone)]
pub struct RunStrip {
    state: Rc<RunStripState>,
}

/// Adds the Run strip (a new `AXTabGroup`, a direct child of the main pane) to a `conductor_app`
/// tree.
///
/// The tab group's children are an `AXRadioButton` `Run` and then, when nothing runs, an
/// `AXButton` `Run <selected name>` and (with more than one task) an `AXPopUpButton`
/// `Select task`; while a task runs, an `AXButton` `Stop <name>` and no pop-up. The open task menu
/// hangs at tab group → `AXGroup`/`AXApplicationGroup` → `AXGroup` → `AXMenu` `Select task`.
///
/// # Panics
///
/// When `app` has no main pane, `spec.tasks` is empty, or `spec.selected` or `spec.running` is not
/// an index of `spec.tasks`.
pub fn add_run_strip(app: &FakeNode, spec: &RunStripSpec) -> RunStrip {
    assert!(
        spec.selected < spec.tasks.len(),
        "the selected task must be one of the tasks"
    );
    assert!(
        spec.running.is_none_or(|index| index < spec.tasks.len()),
        "the running task must be one of the tasks"
    );
    let tabs =
        FakeNode::new("AXTabGroup").with_child(FakeNode::new("AXRadioButton").with_label("Run"));
    let state = Rc::new(RunStripState {
        tabs: tabs.clone(),
        tasks: spec.tasks.clone(),
        selected: Cell::new(spec.selected),
        running: Cell::new(spec.running),
        starts: Cell::new(0),
        stops: Cell::new(0),
        ignore_presses: Cell::new(false),
        controls: RefCell::new(Vec::new()),
        menu: RefCell::new(None),
        on_start: RefCell::new(Vec::new()),
        on_stop: RefCell::new(Vec::new()),
    });
    state.rebuild();
    main_pane(app).add_child(tabs);
    RunStrip { state }
}

impl RunStrip {
    /// The name of the running task.
    pub fn running(&self) -> Option<String> {
        self.state.running_name()
    }

    /// How many times a task was started, and stopped.
    pub fn starts(&self) -> usize {
        self.state.starts.get()
    }

    pub fn stops(&self) -> usize {
        self.state.stops.get()
    }

    /// Run when a task starts (its name) and stops (its name).
    pub fn on_start(&self, reaction: impl Fn(&str) + 'static) {
        self.state.on_start.borrow_mut().push(Rc::new(reaction));
    }

    pub fn on_stop(&self, reaction: impl Fn(&str) + 'static) {
        self.state.on_stop.borrow_mut().push(Rc::new(reaction));
    }

    /// From now on a press on a Run or Stop button changes nothing.
    pub fn ignore_presses(&self) {
        self.state.ignore_presses.set(true);
    }
}

impl RunStripState {
    fn running_name(&self) -> Option<String> {
        self.running.get().map(|index| self.tasks[index].clone())
    }

    /// Replaces the button and the pop-up to match the state, and closes the menu.
    fn rebuild(self: &Rc<Self>) {
        self.close_menu();
        for control in self.controls.borrow_mut().drain(..) {
            self.tabs.remove_child(&control);
        }
        let mut controls = Vec::new();
        if let Some(name) = self.running_name() {
            let stop = FakeNode::new("AXButton").with_label(&format!("Stop {name}"));
            let state = Rc::clone(self);
            stop.on_press(move |_| {
                if !state.ignore_presses.get() {
                    state.stop();
                }
            });
            controls.push(stop);
        } else {
            let name = &self.tasks[self.selected.get()];
            let run = FakeNode::new("AXButton").with_label(&format!("Run {name}"));
            let state = Rc::clone(self);
            run.on_press(move |_| {
                if !state.ignore_presses.get() {
                    state.start(state.selected.get());
                }
            });
            controls.push(run);
            if self.tasks.len() > 1 {
                let popup = FakeNode::new("AXPopUpButton").with_label("Select task");
                let state = Rc::clone(self);
                popup.on_press(move |_| state.toggle_menu());
                controls.push(popup);
            }
        }
        for control in &controls {
            self.tabs.add_child(control.clone());
        }
        *self.controls.borrow_mut() = controls;
    }

    fn start(self: &Rc<Self>, index: usize) {
        self.running.set(Some(index));
        self.starts.set(self.starts.get() + 1);
        self.rebuild();
        self.react(&self.on_start, &self.tasks[index]);
    }

    fn stop(self: &Rc<Self>) {
        let Some(index) = self.running.take() else {
            return;
        };
        self.selected.set(index);
        self.stops.set(self.stops.get() + 1);
        self.rebuild();
        self.react(&self.on_stop, &self.tasks[index]);
    }

    fn react(&self, slot: &RefCell<Vec<TaskReaction>>, name: &str) {
        // Cloned out first: a reaction may add another.
        let reactions = slot.borrow().clone();
        for reaction in reactions {
            reaction(name);
        }
    }

    fn toggle_menu(self: &Rc<Self>) {
        if self.menu.borrow().is_some() {
            self.close_menu();
            return;
        }
        let menu = FakeNode::new("AXMenu").with_label("Select task");
        for (index, name) in self.tasks.iter().enumerate() {
            let item = FakeNode::new("AXMenuItem").with_label(name);
            let state = Rc::clone(self);
            item.on_press(move |_| {
                state.close_menu();
                state.start(index);
            });
            menu.add_child(item);
        }
        menu.add_child(FakeNode::new("AXMenuItem").with_label("Configure"));
        *self.menu.borrow_mut() = Some(mount_menu(&self.tabs, menu));
    }

    fn close_menu(&self) {
        let open = self.menu.borrow_mut().take();
        if let Some(open) = open {
            self.tabs.remove_child(&open.wrapper);
        }
    }
}
