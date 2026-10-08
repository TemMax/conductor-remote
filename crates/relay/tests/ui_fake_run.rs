//! The fake Run strip: the shape the real Run actions will be tested against. Nothing here reaches
//! the Mac.

use std::cell::RefCell;
use std::rc::Rc;

use conductor_remote::ui::fake::{
    add_run_strip, conductor_app, main_pane, FakeNode, RunStrip, RunStripSpec, WindowSpec,
};
use conductor_remote::ui::node::UiNode;

fn window() -> WindowSpec {
    WindowSpec {
        repo: "relay".to_owned(),
        branch: "user/feature-x".to_owned(),
        sidebar: vec!["alpha".to_owned()],
        chats: vec!["One".to_owned()],
        selected: 0,
        composer_value: None,
    }
}

fn spec(tasks: &[&str], selected: usize, running: Option<usize>) -> RunStripSpec {
    RunStripSpec {
        tasks: tasks.iter().map(|task| (*task).to_owned()).collect(),
        selected,
        running,
    }
}

fn setup(spec: &RunStripSpec) -> (FakeNode, RunStrip) {
    let app = conductor_app(&window());
    let strip = add_run_strip(&app, spec);
    (app, strip)
}

fn kids(node: &FakeNode) -> Vec<FakeNode> {
    node.children().expect("children")
}

/// The Run strip: the main pane's last child, the tab group `add_run_strip` added.
fn tabs(app: &FakeNode) -> FakeNode {
    kids(&main_pane(app))
        .pop()
        .expect("the main pane has children")
}

fn roles_and_labels(node: &FakeNode) -> Vec<(Option<String>, Option<String>)> {
    kids(node)
        .iter()
        .map(|child| (child.role(), child.label()))
        .collect()
}

fn entry(role: &str, label: &str) -> (Option<String>, Option<String>) {
    (Some(role.to_owned()), Some(label.to_owned()))
}

fn child_labelled(node: &FakeNode, label: &str) -> FakeNode {
    kids(node)
        .into_iter()
        .find(|child| child.label().as_deref() == Some(label))
        .unwrap_or_else(|| panic!("no child labelled {label}"))
}

/// The open menu at tab group → `AXGroup`/`AXApplicationGroup` → `AXGroup` → `AXMenu`.
fn open_menu(app: &FakeNode) -> Option<FakeNode> {
    let wrapper = kids(&tabs(app)).into_iter().find(|child| {
        child.role().as_deref() == Some("AXGroup")
            && child.subrole().as_deref() == Some("AXApplicationGroup")
    })?;
    let inner = kids(&wrapper).into_iter().next()?;
    assert_eq!(inner.role().as_deref(), Some("AXGroup"));
    assert_eq!(inner.subrole(), None);
    let menu = kids(&inner).into_iter().next()?;
    assert_eq!(menu.role().as_deref(), Some("AXMenu"));
    Some(menu)
}

fn menu_labels(menu: &FakeNode) -> Vec<(Option<String>, Option<String>)> {
    roles_and_labels(menu)
}

#[test]
fn the_strip_is_a_new_tab_group_in_the_main_pane() {
    let app = conductor_app(&window());
    let before = kids(&main_pane(&app)).len();
    add_run_strip(&app, &spec(&["dev", "test"], 0, None));
    let children = kids(&main_pane(&app));
    assert_eq!(children.len(), before + 1);
    assert_eq!(children[before].role().as_deref(), Some("AXTabGroup"));
    assert_eq!(
        kids(&children[before])[0].role().as_deref(),
        Some("AXRadioButton")
    );
    assert_eq!(kids(&children[before])[0].label().as_deref(), Some("Run"));
}

#[test]
fn idle_strip_shows_run_selected_and_select_task() {
    let (app, strip) = setup(&spec(&["dev", "test"], 1, None));
    assert_eq!(
        roles_and_labels(&tabs(&app)),
        vec![
            entry("AXRadioButton", "Run"),
            entry("AXButton", "Run test"),
            entry("AXPopUpButton", "Select task"),
        ]
    );
    assert_eq!(strip.running(), None);
    assert_eq!((strip.starts(), strip.stops()), (0, 0));
}

#[test]
fn one_task_shows_no_select_task() {
    let (app, _strip) = setup(&spec(&["dev"], 0, None));
    assert_eq!(
        roles_and_labels(&tabs(&app)),
        vec![entry("AXRadioButton", "Run"), entry("AXButton", "Run dev")]
    );
}

#[test]
fn a_running_task_shows_stop_and_no_pop_up() {
    let (app, strip) = setup(&spec(&["dev", "test"], 0, Some(1)));
    assert_eq!(
        roles_and_labels(&tabs(&app)),
        vec![
            entry("AXRadioButton", "Run"),
            entry("AXButton", "Stop test")
        ]
    );
    assert_eq!(strip.running().as_deref(), Some("test"));
}

#[test]
fn pressing_run_starts_the_selected_task() {
    let (app, strip) = setup(&spec(&["dev", "test"], 1, None));
    child_labelled(&tabs(&app), "Run test").press().unwrap();
    assert_eq!(strip.running().as_deref(), Some("test"));
    assert_eq!((strip.starts(), strip.stops()), (1, 0));
}

#[test]
fn pressing_stop_stops_the_task_and_selects_it() {
    let (app, strip) = setup(&spec(&["dev", "test"], 0, Some(1)));
    child_labelled(&tabs(&app), "Stop test").press().unwrap();
    assert_eq!(strip.running(), None);
    assert_eq!((strip.starts(), strip.stops()), (0, 1));
    assert_eq!(
        roles_and_labels(&tabs(&app)),
        vec![
            entry("AXRadioButton", "Run"),
            entry("AXButton", "Run test"),
            entry("AXPopUpButton", "Select task"),
        ]
    );
}

#[test]
fn pressing_select_task_opens_the_menu_at_its_tree_path() {
    let (app, _strip) = setup(&spec(&["dev", "test"], 0, None));
    assert!(open_menu(&app).is_none());
    child_labelled(&tabs(&app), "Select task").press().unwrap();

    let wrapper = &kids(&tabs(&app))[3];
    assert_eq!(wrapper.role().as_deref(), Some("AXGroup"));
    assert_eq!(wrapper.subrole().as_deref(), Some("AXApplicationGroup"));
    let menu = open_menu(&app).expect("the menu is open");
    assert_eq!(menu.label().as_deref(), Some("Select task"));
    assert_eq!(
        menu_labels(&menu),
        vec![
            entry("AXMenuItem", "dev"),
            entry("AXMenuItem", "test"),
            entry("AXMenuItem", "Configure"),
        ]
    );
}

#[test]
fn pressing_a_task_item_closes_the_menu_and_starts_that_task() {
    let (app, strip) = setup(&spec(&["dev", "test"], 0, None));
    child_labelled(&tabs(&app), "Select task").press().unwrap();
    child_labelled(&open_menu(&app).unwrap(), "test")
        .press()
        .unwrap();
    assert!(open_menu(&app).is_none());
    assert_eq!(strip.running().as_deref(), Some("test"));
    assert_eq!((strip.starts(), strip.stops()), (1, 0));
    assert_eq!(
        roles_and_labels(&tabs(&app)),
        vec![
            entry("AXRadioButton", "Run"),
            entry("AXButton", "Stop test")
        ]
    );
}

#[test]
fn pressing_select_task_again_closes_the_menu() {
    let (app, strip) = setup(&spec(&["dev", "test"], 0, None));
    child_labelled(&tabs(&app), "Select task").press().unwrap();
    child_labelled(&tabs(&app), "Select task").press().unwrap();
    assert!(open_menu(&app).is_none());
    assert_eq!(kids(&tabs(&app)).len(), 3);
    assert_eq!(strip.running(), None);
}

#[test]
fn every_start_and_stop_rebuilds_the_buttons_counts_and_runs_the_reactions() {
    let (app, strip) = setup(&spec(&["dev", "test"], 0, None));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&seen);
    strip.on_start(move |name| log.borrow_mut().push(format!("start {name}")));
    let log = Rc::clone(&seen);
    strip.on_stop(move |name| log.borrow_mut().push(format!("stop {name}")));

    let old_run = child_labelled(&tabs(&app), "Run dev");
    old_run.press().unwrap();
    assert!(
        kids(&tabs(&app))
            .iter()
            .all(|child| child.label().as_deref() != Some("Run dev")),
        "the Run button was replaced"
    );
    child_labelled(&tabs(&app), "Stop dev").press().unwrap();
    child_labelled(&tabs(&app), "Run dev").press().unwrap();
    child_labelled(&tabs(&app), "Stop dev").press().unwrap();

    assert_eq!((strip.starts(), strip.stops()), (2, 2));
    assert_eq!(
        *seen.borrow(),
        vec!["start dev", "stop dev", "start dev", "stop dev"]
    );
}

#[test]
fn ignore_presses_makes_run_and_stop_change_nothing() {
    let (app, strip) = setup(&spec(&["dev", "test"], 0, None));
    strip.ignore_presses();
    let reactions = Rc::new(RefCell::new(0));
    let count = Rc::clone(&reactions);
    strip.on_start(move |_| *count.borrow_mut() += 1);

    child_labelled(&tabs(&app), "Run dev").press().unwrap();
    assert_eq!(strip.running(), None);
    assert_eq!((strip.starts(), strip.stops()), (0, 0));
    assert_eq!(*reactions.borrow(), 0);
    assert_eq!(
        roles_and_labels(&tabs(&app)),
        vec![
            entry("AXRadioButton", "Run"),
            entry("AXButton", "Run dev"),
            entry("AXPopUpButton", "Select task"),
        ]
    );

    let (app, strip) = setup(&spec(&["dev", "test"], 0, Some(0)));
    strip.ignore_presses();
    child_labelled(&tabs(&app), "Stop dev").press().unwrap();
    assert_eq!(strip.running().as_deref(), Some("dev"));
    assert_eq!((strip.starts(), strip.stops()), (0, 0));
    assert_eq!(
        roles_and_labels(&tabs(&app)),
        vec![entry("AXRadioButton", "Run"), entry("AXButton", "Stop dev")]
    );
}
