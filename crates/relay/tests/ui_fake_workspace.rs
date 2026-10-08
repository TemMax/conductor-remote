//! The fake workspace controls: the close and archive alerts, the row menu, the Continue button
//! and the New workspace dialog, driven through the desktop and the nodes only. Nothing here
//! reaches the Mac.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use conductor_remote::ui::desktop::Desktop;
use conductor_remote::ui::fake::{
    add_workspace_ui, conductor_app, main_pane, FakeDesktop, FakeEvent, FakeNode, WindowSpec,
    WorkspaceUi, WorkspaceUiSpec,
};
use conductor_remote::ui::keys::{key_code, Key, Modifiers};
use conductor_remote::ui::node::UiNode;

const PID: i32 = 4242;

fn window() -> WindowSpec {
    WindowSpec {
        repo: "relay".to_owned(),
        branch: "user/feature-x".to_owned(),
        sidebar: vec!["alpha".to_owned(), "beta".to_owned()],
        chats: vec!["One".to_owned(), "Two".to_owned(), "Three".to_owned()],
        selected: 1,
        composer_value: None,
    }
}

fn ui_spec(running: &[&str], continue_button: bool) -> WorkspaceUiSpec {
    WorkspaceUiSpec {
        running: running.iter().map(|title| (*title).to_owned()).collect(),
        continue_button,
    }
}

fn setup(running: &[&str], continue_button: bool) -> (FakeDesktop, WorkspaceUi) {
    let desktop = FakeDesktop::new(conductor_app(&window()));
    let ui = add_workspace_ui(&desktop, &ui_spec(running, continue_button));
    (desktop, ui)
}

fn kids(node: &FakeNode) -> Vec<FakeNode> {
    node.children().expect("children")
}

fn web_area(desktop: &FakeDesktop) -> FakeNode {
    desktop.app().find_role("AXWebArea").expect("a web area")
}

/// The web area's last child: the most recently mounted dialog or menu wrapper.
fn mounted(desktop: &FakeDesktop) -> FakeNode {
    kids(&web_area(desktop)).pop().expect("a child")
}

/// The only child of `node`.
fn only_child(node: &FakeNode) -> FakeNode {
    let mut children = kids(node);
    assert_eq!(children.len(), 1, "{:?} has one child", node.label());
    children.remove(0)
}

fn labels(nodes: &[FakeNode]) -> Vec<Option<String>> {
    nodes.iter().map(UiNode::label).collect()
}

fn some(labels: &[&str]) -> Vec<Option<String>> {
    labels
        .iter()
        .map(|label| Some((*label).to_owned()))
        .collect()
}

fn strip_labels(desktop: &FakeDesktop) -> Vec<Option<String>> {
    labels(&kids(&strip(desktop)))
}

fn strip(desktop: &FakeDesktop) -> FakeNode {
    let tabs = main_pane(&desktop.app())
        .find_role("AXTabGroup")
        .expect("the chat tab group");
    only_child(&tabs)
}

fn command() -> Modifiers {
    Modifiers {
        command: true,
        ..Modifiers::default()
    }
}

fn command_shift() -> Modifiers {
    Modifiers {
        command: true,
        shift: true,
        ..Modifiers::default()
    }
}

fn press_key(desktop: &FakeDesktop, key: Key, modifiers: Modifiers) {
    desktop.post_key(PID, key, modifiers).expect("key");
}

/// The button of the mounted alert with this label.
fn alert_button(desktop: &FakeDesktop, label: &str) -> FakeNode {
    let dialog = only_child(&only_child(&mounted(desktop)));
    kids(&dialog)
        .into_iter()
        .find(|node| node.label().as_deref() == Some(label))
        .unwrap_or_else(|| panic!("no button {label}"))
}

fn link(desktop: &FakeDesktop, name: &str) -> FakeNode {
    desktop
        .app()
        .find_label(name)
        .filter(|node| node.role().as_deref() == Some("AXLink"))
        .unwrap_or_else(|| panic!("no link {name}"))
}

/// Asserts the mounted alert sits at `AXWebArea` → `AXGroup` → `AXGroup` → dialog and returns
/// the dialog.
fn assert_alert(desktop: &FakeDesktop, label: &str, buttons: &[&str]) -> FakeNode {
    let outer = mounted(desktop);
    assert_eq!(outer.role().as_deref(), Some("AXGroup"));
    let inner = only_child(&outer);
    assert_eq!(inner.role().as_deref(), Some("AXGroup"));
    let dialog = only_child(&inner);
    assert_eq!(dialog.role().as_deref(), Some("AXGroup"));
    assert_eq!(
        dialog.subrole().as_deref(),
        Some("AXApplicationAlertDialog")
    );
    assert_eq!(dialog.label().as_deref(), Some(label));
    let children = kids(&dialog);
    assert_eq!(children[0].role().as_deref(), Some("AXStaticText"));
    for (child, button) in children[1..].iter().zip(buttons) {
        assert_eq!(child.role().as_deref(), Some("AXButton"));
        assert_eq!(child.label().as_deref(), Some(*button));
    }
    assert_eq!(children.len(), 1 + buttons.len());
    dialog
}

#[test]
fn keys_w_and_a_have_their_virtual_key_codes() {
    assert_eq!(key_code(Key::W), 0x0D);
    assert_eq!(key_code(Key::A), 0x00);
}

#[test]
fn show_menu_records_its_event_and_runs_its_reactions() {
    let node = FakeNode::new("AXLink").with_label("alpha");
    let app = FakeNode::new("AXApplication").with_child(node.clone());
    let desktop = FakeDesktop::new(app);
    let order = Rc::new(RefCell::new(Vec::new()));
    for tag in ["first", "second"] {
        let order = Rc::clone(&order);
        node.on_show_menu(move |shown| {
            order.borrow_mut().push((tag, shown.label_text()));
        });
    }
    node.show_menu().expect("show menu");
    assert_eq!(
        desktop.events(),
        vec![FakeEvent::ShowMenu(Some("alpha".to_owned()))]
    );
    assert_eq!(
        *order.borrow(),
        vec![
            ("first", Some("alpha".to_owned())),
            ("second", Some("alpha".to_owned()))
        ]
    );
}

#[test]
fn cmd_w_closes_an_idle_selected_chat() {
    let (desktop, ui) = setup(&[], false);
    let closed = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&closed);
    ui.on_close(move |title| seen.borrow_mut().push(title.to_owned()));
    press_key(&desktop, Key::W, command());
    assert_eq!(ui.closed(), vec!["Two".to_owned()]);
    assert_eq!(*closed.borrow(), vec!["Two".to_owned()]);
    assert!(!ui.dialog_open());
    assert_eq!(
        strip_labels(&desktop),
        some(&["Close chat One", "Close chat Three"])
    );
    let radios = kids(&strip(&desktop));
    assert!(radios[0].is_selected());
    assert!(!radios[1].is_selected());
    // The next close takes the chat that is now first and selected.
    press_key(&desktop, Key::W, command());
    assert_eq!(ui.closed(), vec!["Two".to_owned(), "One".to_owned()]);
    assert_eq!(strip_labels(&desktop), some(&["Close chat Three"]));
    assert!(kids(&strip(&desktop))[0].is_selected());
}

#[test]
fn cmd_w_with_other_modifiers_does_nothing() {
    let (desktop, ui) = setup(&["Two"], false);
    for modifiers in [
        Modifiers::default(),
        command_shift(),
        Modifiers {
            command: true,
            option: true,
            ..Modifiers::default()
        },
    ] {
        press_key(&desktop, Key::W, modifiers);
    }
    assert!(ui.closed().is_empty());
    assert!(!ui.dialog_open());
    assert_eq!(kids(&strip(&desktop)).len(), 3);
}

#[test]
fn cmd_w_on_a_running_chat_asks_first() {
    let (desktop, ui) = setup(&["Two"], false);
    let before = kids(&web_area(&desktop)).len();
    press_key(&desktop, Key::W, command());
    assert!(ui.dialog_open());
    assert!(ui.closed().is_empty());
    assert_eq!(kids(&web_area(&desktop)).len(), before + 1);
    assert_alert(
        &desktop,
        "Close running chat?",
        &["Cancel", "Close anyway ⌘ Enter"],
    );
    assert_eq!(kids(&strip(&desktop)).len(), 3);
}

#[test]
fn cancel_leaves_the_running_chat_open() {
    let (desktop, ui) = setup(&["Two"], false);
    let before = kids(&web_area(&desktop)).len();
    press_key(&desktop, Key::W, command());
    alert_button(&desktop, "Cancel").press().expect("press");
    assert!(!ui.dialog_open());
    assert!(ui.closed().is_empty());
    assert_eq!(kids(&web_area(&desktop)).len(), before);
    assert_eq!(kids(&strip(&desktop)).len(), 3);
}

#[test]
fn close_anyway_closes_the_running_chat() {
    let (desktop, ui) = setup(&["Two"], false);
    let before = kids(&web_area(&desktop)).len();
    let closed = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&closed);
    ui.on_close(move |title| seen.borrow_mut().push(title.to_owned()));
    press_key(&desktop, Key::W, command());
    alert_button(&desktop, "Close anyway ⌘ Enter")
        .press()
        .expect("press");
    assert!(!ui.dialog_open());
    assert_eq!(kids(&web_area(&desktop)).len(), before);
    assert_eq!(ui.closed(), vec!["Two".to_owned()]);
    assert_eq!(*closed.borrow(), vec!["Two".to_owned()]);
    assert_eq!(
        strip_labels(&desktop),
        some(&["Close chat One", "Close chat Three"])
    );
    assert!(kids(&strip(&desktop))[0].is_selected());
}

#[test]
fn a_chat_closed_anyway_is_no_longer_running() {
    let (desktop, ui) = setup(&["Two"], false);
    press_key(&desktop, Key::W, command());
    alert_button(&desktop, "Close anyway ⌘ Enter")
        .press()
        .expect("press");
    // A chat with the same title comes back: it no longer counts as running.
    strip(&desktop).add_child(FakeNode::new("AXRadioButton").with_label("Close chat Two"));
    press_key(&desktop, Key::A, command_shift());
    assert!(!ui.dialog_open());
    assert!(ui.archived());
}

#[test]
fn cmd_shift_a_archives_when_nothing_runs() {
    let (desktop, ui) = setup(&[], false);
    let count = Rc::new(Cell::new(0));
    let seen = Rc::clone(&count);
    ui.on_archive(move || seen.set(seen.get() + 1));
    press_key(&desktop, Key::A, command_shift());
    assert!(ui.archived());
    assert!(!ui.dialog_open());
    assert_eq!(count.get(), 1);
}

#[test]
fn cmd_shift_a_ignores_a_running_chat_that_is_not_in_the_strip() {
    let (desktop, ui) = setup(&["Gone"], false);
    press_key(&desktop, Key::A, command_shift());
    assert!(ui.archived());
    assert!(!ui.dialog_open());
}

#[test]
fn cmd_shift_a_with_other_modifiers_does_nothing() {
    let (desktop, ui) = setup(&[], false);
    for modifiers in [
        Modifiers::default(),
        command(),
        Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    ] {
        press_key(&desktop, Key::A, modifiers);
    }
    assert!(!ui.archived());
    assert!(!ui.dialog_open());
}

#[test]
fn cmd_shift_a_with_a_running_chat_asks_first() {
    let (desktop, ui) = setup(&["Three"], false);
    press_key(&desktop, Key::A, command_shift());
    assert!(ui.dialog_open());
    assert!(!ui.archived());
    assert_alert(
        &desktop,
        "Archive workspace?",
        &["Cancel", "Stop agents and archive ⌘ Enter"],
    );
}

#[test]
fn cancel_does_not_archive() {
    let (desktop, ui) = setup(&["Three"], false);
    press_key(&desktop, Key::A, command_shift());
    alert_button(&desktop, "Cancel").press().expect("press");
    assert!(!ui.dialog_open());
    assert!(!ui.archived());
}

#[test]
fn stop_agents_and_archive_archives() {
    let (desktop, ui) = setup(&["Three"], false);
    let count = Rc::new(Cell::new(0));
    let seen = Rc::clone(&count);
    ui.on_archive(move || seen.set(seen.get() + 1));
    press_key(&desktop, Key::A, command_shift());
    alert_button(&desktop, "Stop agents and archive ⌘ Enter")
        .press()
        .expect("press");
    assert!(!ui.dialog_open());
    assert!(ui.archived());
    assert_eq!(count.get(), 1);
    assert_eq!(kids(&strip(&desktop)).len(), 3);
}

#[test]
fn show_menu_on_a_sidebar_link_opens_the_row_menu() {
    let (desktop, ui) = setup(&[], false);
    let before = kids(&web_area(&desktop)).len();
    link(&desktop, "beta").show_menu().expect("show menu");
    assert!(ui.menu_open());
    assert_eq!(kids(&web_area(&desktop)).len(), before + 1);
    // AXWebArea → AXGroup → AXMenu.
    let wrapper = mounted(&desktop);
    assert_eq!(wrapper.role().as_deref(), Some("AXGroup"));
    let menu = only_child(&wrapper);
    assert_eq!(menu.role().as_deref(), Some("AXMenu"));
    let items = kids(&menu);
    assert_eq!(
        labels(&items),
        some(&[
            "Mark as unread R",
            "Pin P",
            "Set status",
            "Move to section",
            "Rename",
            "Copy link ⌘⇧C",
            "Archive ⌘⇧A"
        ])
    );
    assert!(items
        .iter()
        .all(|item| item.role().as_deref() == Some("AXMenuItem")));
    assert_eq!(
        desktop.events().last(),
        Some(&FakeEvent::ShowMenu(Some("beta".to_owned())))
    );
}

#[test]
fn set_status_opens_a_nested_menu() {
    let (desktop, _ui) = setup(&[], false);
    link(&desktop, "alpha").show_menu().expect("show menu");
    let menu = only_child(&mounted(&desktop));
    let set_status = kids(&menu)
        .into_iter()
        .find(|item| item.label().as_deref() == Some("Set status"))
        .expect("Set status");
    set_status.press().expect("press");
    // Row menu → AXGroup → AXMenu.
    let nested_wrapper = kids(&menu).pop().expect("a nested wrapper");
    assert_eq!(nested_wrapper.role().as_deref(), Some("AXGroup"));
    let nested = only_child(&nested_wrapper);
    assert_eq!(nested.role().as_deref(), Some("AXMenu"));
    assert_eq!(nested.label().as_deref(), Some("Set status"));
    assert_eq!(
        labels(&kids(&nested)),
        some(&["Backlog", "In progress", "In review", "Done", "Canceled"])
    );
    // Pressing Set status again does not open a second submenu.
    set_status.press().expect("press");
    assert_eq!(kids(&menu).len(), 8);
}

#[test]
fn picking_a_status_records_it_and_closes_both_menus() {
    let (desktop, ui) = setup(&[], false);
    let before = kids(&web_area(&desktop)).len();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    ui.on_status(move |row, item| sink.borrow_mut().push((row.to_owned(), item.to_owned())));
    link(&desktop, "beta").show_menu().expect("show menu");
    let menu = only_child(&mounted(&desktop));
    kids(&menu)
        .into_iter()
        .find(|item| item.label().as_deref() == Some("Set status"))
        .expect("Set status")
        .press()
        .expect("press");
    let nested = only_child(&kids(&menu).pop().expect("wrapper"));
    kids(&nested)
        .into_iter()
        .find(|item| item.label().as_deref() == Some("In review"))
        .expect("In review")
        .press()
        .expect("press");
    assert_eq!(
        ui.status(),
        Some(("beta".to_owned(), "In review".to_owned()))
    );
    assert_eq!(
        *seen.borrow(),
        vec![("beta".to_owned(), "In review".to_owned())]
    );
    assert!(!ui.menu_open());
    assert_eq!(kids(&web_area(&desktop)).len(), before);
}

#[test]
fn escape_closes_the_row_menu_and_its_nested_menu_without_a_status() {
    let (desktop, ui) = setup(&[], false);
    let before = kids(&web_area(&desktop)).len();
    link(&desktop, "alpha").show_menu().expect("show menu");
    let menu = only_child(&mounted(&desktop));
    kids(&menu)
        .into_iter()
        .find(|item| item.label().as_deref() == Some("Set status"))
        .expect("Set status")
        .press()
        .expect("press");
    // A modified Escape is not Escape.
    press_key(&desktop, Key::Escape, command());
    assert!(ui.menu_open());
    press_key(&desktop, Key::Escape, Modifiers::default());
    assert!(!ui.menu_open());
    assert_eq!(ui.status(), None);
    assert_eq!(kids(&web_area(&desktop)).len(), before);
}

#[test]
fn escape_without_a_menu_changes_nothing() {
    let (desktop, ui) = setup(&[], false);
    press_key(&desktop, Key::Escape, Modifiers::default());
    assert!(!ui.menu_open());
    assert!(!ui.dialog_open());
}

#[test]
fn the_continue_button_is_a_direct_child_of_the_main_pane() {
    let (desktop, ui) = setup(&[], true);
    let pane = main_pane(&desktop.app());
    let button = kids(&pane)
        .into_iter()
        .find(|node| node.label().as_deref() == Some("Continue"))
        .expect("a Continue child");
    assert_eq!(button.role().as_deref(), Some("AXButton"));
    let count = Rc::new(Cell::new(0));
    let seen = Rc::clone(&count);
    ui.on_continue(move || seen.set(seen.get() + 1));
    assert!(!ui.continued());
    button.press().expect("press");
    assert!(ui.continued());
    assert_eq!(count.get(), 1);
    assert!(kids(&pane)
        .iter()
        .all(|node| node.label().as_deref() != Some("Continue")));
    assert_eq!(
        desktop.events(),
        vec![FakeEvent::Press(Some("Continue".to_owned()))]
    );
}

#[test]
fn there_is_no_continue_button_unless_asked_for() {
    let (desktop, ui) = setup(&[], false);
    assert!(kids(&main_pane(&desktop.app()))
        .iter()
        .all(|node| node.label().as_deref() != Some("Continue")));
    assert!(!ui.continued());
}

#[test]
fn a_new_workspace_link_opens_the_dialog() {
    let (desktop, ui) = setup(&[], false);
    let before = kids(&web_area(&desktop)).len();
    assert!(desktop.open_url("conductor://new"));
    assert!(ui.dialog_open());
    assert_eq!(kids(&web_area(&desktop)).len(), before + 1);
    // AXWebArea → AXGroup → dialog.
    let wrapper = mounted(&desktop);
    assert_eq!(wrapper.role().as_deref(), Some("AXGroup"));
    let dialog = only_child(&wrapper);
    assert_eq!(dialog.role().as_deref(), Some("AXGroup"));
    assert_eq!(dialog.subrole().as_deref(), Some("AXApplicationDialog"));
    assert_eq!(dialog.label().as_deref(), Some("New workspace"));
    let form = only_child(&dialog);
    assert_eq!(form.role().as_deref(), Some("AXGroup"));
    assert_eq!(form.subrole().as_deref(), Some("AXLandmarkForm"));
    assert_eq!(form.label().as_deref(), Some("composer"));
    let parts = kids(&form);
    assert_eq!(parts[0].role().as_deref(), Some("AXTextArea"));
    assert_eq!(
        parts[0].label().as_deref(),
        Some("What do you want to work on?")
    );
    assert_eq!(parts[1].role().as_deref(), Some("AXButton"));
    assert_eq!(parts[1].label().as_deref(), Some("Create"));
    assert_eq!(parts.len(), 2);
}

#[test]
fn other_links_do_not_open_the_dialog() {
    let (desktop, ui) = setup(&[], false);
    for url in [
        "conductor://workspace/abc",
        "conductor://workspace",
        "https://example.com/conductor://new",
    ] {
        assert!(desktop.open_url(url));
    }
    assert!(!ui.dialog_open());
    assert_eq!(kids(&web_area(&desktop)).len(), 2);
}

#[test]
fn create_counts_and_closes_the_dialog() {
    let (desktop, ui) = setup(&[], false);
    let before = kids(&web_area(&desktop)).len();
    let count = Rc::new(Cell::new(0));
    let seen = Rc::clone(&count);
    ui.on_create(move || seen.set(seen.get() + 1));
    desktop.open_url("conductor://new?x=1");
    let form = only_child(&only_child(&mounted(&desktop)));
    let create = kids(&form).pop().expect("Create");
    assert_eq!(create.label().as_deref(), Some("Create"));
    create.press().expect("press");
    assert_eq!(ui.created(), 1);
    assert_eq!(count.get(), 1);
    assert!(!ui.dialog_open());
    assert_eq!(kids(&web_area(&desktop)).len(), before);
    desktop.open_url("conductor://new");
    assert!(ui.dialog_open());
    assert_eq!(ui.created(), 1);
}
