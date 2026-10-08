//! The fake agent menus and the `help` / `flag` reads of `FakeNode`: the shape the real actions
//! will be tested against. Nothing here reaches the Mac.

use std::cell::RefCell;
use std::rc::Rc;

use conductor_remote::ui::ax::AxError;
use conductor_remote::ui::fake::{
    add_agent_menus, conductor_app, main_pane, AgentMenuSpec, AgentMenus, FakeNode, WindowSpec,
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

fn model(name: &str, effort: &str, fast: bool) -> (String, String, bool) {
    (name.to_owned(), effort.to_owned(), fast)
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Opus (high), Sonnet (medium, fast) and Haiku (no effort); Sonnet is current and Opus and
/// Haiku open a new chat.
fn spec() -> AgentMenuSpec {
    AgentMenuSpec {
        models: vec![
            model("Opus", "high", false),
            model("Sonnet", "medium", true),
            model("Haiku", "", false),
        ],
        current: 1,
        new_chat_models: names(&["Opus", "Haiku"]),
        efforts: names(&["low", "medium", "high"]),
        fast_item: true,
        plan: Some(false),
    }
}

fn setup(spec: &AgentMenuSpec) -> (FakeNode, AgentMenus) {
    let app = conductor_app(&window());
    let menus = add_agent_menus(&app, spec);
    (app, menus)
}

fn kids(node: &FakeNode) -> Vec<FakeNode> {
    node.children().expect("children")
}

fn labels(nodes: &[FakeNode]) -> Vec<Option<String>> {
    nodes.iter().map(UiNode::label).collect()
}

fn text(label: &str) -> Option<String> {
    Some(label.to_owned())
}

fn composer(app: &FakeNode) -> FakeNode {
    kids(&main_pane(app))[4].clone()
}

/// The composer's second new child: the group the open menus hang from.
fn slot(app: &FakeNode) -> FakeNode {
    kids(&composer(app))[3].clone()
}

fn pop_up(app: &FakeNode, index: usize) -> FakeNode {
    kids(&kids(&composer(app))[2])[index].clone()
}

fn agent_pop_up(app: &FakeNode) -> FakeNode {
    pop_up(app, 0)
}

fn add_pop_up(app: &FakeNode) -> FakeNode {
    pop_up(app, 1)
}

/// The `AXMenu` at the bottom of a menu wrapper: `AXGroup`/`AXApplicationGroup` → `AXGroup` →
/// `AXMenu`, checking each step.
fn menu_in(wrapper: &FakeNode) -> FakeNode {
    assert_eq!(wrapper.role(), text("AXGroup"));
    assert_eq!(wrapper.subrole(), text("AXApplicationGroup"));
    let inner = kids(wrapper);
    assert_eq!(inner.len(), 1);
    assert_eq!(inner[0].role(), text("AXGroup"));
    assert_eq!(inner[0].subrole(), None);
    let menus = kids(&inner[0]);
    assert_eq!(menus.len(), 1);
    assert_eq!(menus[0].role(), text("AXMenu"));
    menus[0].clone()
}

/// The menu open under the composer's menu slot, if any: composer → `AXGroup` → wrapper chain.
fn open_menu(app: &FakeNode) -> Option<FakeNode> {
    let wrappers = kids(&slot(app));
    assert!(wrappers.len() <= 1, "at most one menu is open here");
    wrappers.first().map(menu_in)
}

/// The Effort menu open under a model menu: its last child, after the items.
fn nested_menu(model_menu: &FakeNode) -> Option<FakeNode> {
    let last = kids(model_menu).pop()?;
    (last.role() != text("AXMenuItem")).then(|| menu_in(&last))
}

fn item(menu: &FakeNode, label: &str) -> FakeNode {
    kids(menu)
        .into_iter()
        .find(|node| node.label().as_deref() == Some(label))
        .unwrap_or_else(|| panic!("no item {label:?}"))
}

/// Presses the pop-up, the model menu must come out open.
fn open_model_menu(app: &FakeNode) -> FakeNode {
    agent_pop_up(app).press().expect("press");
    open_menu(app).expect("the model menu is open")
}

fn open_add_menu(app: &FakeNode) -> FakeNode {
    add_pop_up(app).press().expect("press");
    open_menu(app).expect("the Add menu is open")
}

/// Whether two handles are the same element.
fn same(a: &FakeNode, b: &FakeNode) -> bool {
    a.set_value_text(Some("probe"));
    let same = b.value_text().as_deref() == Some("probe");
    a.set_value_text(None);
    same
}

fn item_labels(menu: &FakeNode) -> Vec<String> {
    kids(menu)
        .iter()
        .map(|node| node.label().expect("a label"))
        .collect()
}

#[test]
fn help_and_flag_of_a_bare_node() {
    let bare = FakeNode::new("AXMenuItem");
    assert_eq!(bare.help(), None);
    assert_eq!(bare.flag(), None);
    assert_eq!(bare.flag_value(), None);

    let node = FakeNode::new("AXMenuItem")
        .with_help("Opus")
        .with_flag(true);
    assert_eq!(node.help(), text("Opus"));
    assert_eq!(node.flag(), Some(true));
    assert_eq!(node.flag_value(), Some(true));

    node.set_flag(false);
    assert_eq!(node.flag(), Some(false));
    assert_eq!(node.flag_value(), Some(false));
    // The flag is not the string value.
    assert_eq!(node.value(), Ok(None));
}

#[test]
fn remove_child_removes_only_that_element() {
    let first = FakeNode::new("AXButton").with_label("same");
    let twin = FakeNode::new("AXButton").with_label("same");
    let last = FakeNode::new("AXButton").with_label("last");
    let parent = FakeNode::new("AXGroup")
        .with_child(first.clone())
        .with_child(twin.clone())
        .with_child(last);

    parent.remove_child(&twin);
    assert_eq!(labels(&kids(&parent)), vec![text("same"), text("last")]);
    assert!(same(&kids(&parent)[0], &first));
    assert!(twin.parent().is_none());
    assert!(first.parent().is_some());

    // Not a child — a stranger and an element that was already removed: nothing happens.
    parent.remove_child(&FakeNode::new("AXButton").with_label("same"));
    parent.remove_child(&twin);
    assert_eq!(labels(&kids(&parent)), vec![text("same"), text("last")]);
}

#[test]
fn child_nodes_ignores_fail_children() {
    let parent = FakeNode::new("AXGroup").with_child(FakeNode::new("AXButton").with_label("a"));
    parent.fail_children(AxError::Failure);
    assert!(parent.children().is_err());
    assert_eq!(labels(&parent.child_nodes()), vec![text("a")]);
}

#[test]
fn the_composer_gets_the_pop_ups_and_an_empty_menu_slot() {
    let (app, menus) = setup(&spec());
    let children = kids(&composer(&app));
    // The text area and the button of conductor_app stay first.
    assert_eq!(children.len(), 4);
    assert_eq!(children[0].role(), text("AXTextArea"));
    assert_eq!(children[1].role(), text("AXButton"));

    assert_eq!(children[2].role(), text("AXGroup"));
    let pop_ups = kids(&children[2]);
    assert_eq!(pop_ups.len(), 2);
    assert_eq!(pop_ups[0].role(), text("AXPopUpButton"));
    assert_eq!(
        pop_ups[0].label(),
        text("Change agent (Sonnet · medium · Fast)")
    );
    assert_eq!(pop_ups[1].role(), text("AXPopUpButton"));
    assert_eq!(pop_ups[1].label(), text("Add"));

    assert_eq!(children[3].role(), text("AXGroup"));
    assert!(kids(&children[3]).is_empty());
    assert!(!menus.menu_open());
    assert_eq!(menus.plan(), Some(false));
}

#[test]
fn the_pop_up_label_is_built_from_the_current_model() {
    let (_, menus) = setup(&spec());
    assert_eq!(menus.shown(), "Change agent (Sonnet · medium · Fast)");

    let mut plain = spec();
    plain.current = 0;
    assert_eq!(setup(&plain).1.shown(), "Change agent (Opus · high)");

    let mut no_effort = spec();
    no_effort.current = 2;
    assert_eq!(setup(&no_effort).1.shown(), "Change agent (Haiku)");

    let mut no_effort_fast = spec();
    no_effort_fast.models[2].2 = true;
    no_effort_fast.current = 2;
    assert_eq!(
        setup(&no_effort_fast).1.shown(),
        "Change agent (Haiku · Fast)"
    );
}

#[test]
fn pressing_change_agent_opens_and_closes_the_model_menu() {
    let (app, menus) = setup(&spec());
    let pop_up = agent_pop_up(&app);
    let slot = slot(&app);

    pop_up.press().expect("press");
    assert!(menus.menu_open());
    // composer → AXGroup (the slot) → AXGroup/AXApplicationGroup → AXGroup → AXMenu.
    assert_eq!(slot.role(), text("AXGroup"));
    assert_eq!(kids(&slot).len(), 1);
    let menu = open_menu(&app).expect("open menu");
    assert_eq!(menu.label(), text("Change agent (Sonnet · medium · Fast)"));
    assert_eq!(menu.role(), text("AXMenu"));

    pop_up.press().expect("press");
    assert!(!menus.menu_open());
    assert!(kids(&slot).is_empty());

    // The slot is kept: the menu opens again in the same place.
    pop_up.press().expect("press");
    assert!(menus.menu_open());
    assert!(same(&slot, &kids(&composer(&app))[3]));
    assert_eq!(kids(&slot).len(), 1);
}

#[test]
fn the_model_menu_lists_the_models_then_the_entries() {
    let (app, _) = setup(&spec());
    let menu = open_model_menu(&app);
    assert_eq!(
        item_labels(&menu),
        vec![
            "Opus high Opens in new chat",
            "Sonnet medium Fast mode on",
            "Haiku Opens in new chat",
            "Add models",
            "Effort medium",
            "Fast",
        ]
    );
    let items = kids(&menu);
    assert!(items.iter().all(|node| node.role() == text("AXMenuItem")));
    let helps: Vec<_> = items.iter().map(UiNode::help).collect();
    assert_eq!(
        helps,
        vec![
            text("Opus"),
            text("Sonnet"),
            text("Haiku"),
            None,
            None,
            None
        ]
    );
    let flags: Vec<_> = items.iter().map(UiNode::flag).collect();
    assert_eq!(flags, vec![None, None, None, None, None, Some(true)]);
}

#[test]
fn the_model_menu_label_is_the_pop_up_label_at_opening_time() {
    let (app, _) = setup(&spec());
    let mut other = spec();
    other.current = 0;
    let (other_app, _) = setup(&other);
    assert_eq!(
        open_model_menu(&app).label(),
        text("Change agent (Sonnet · medium · Fast)")
    );
    assert_eq!(
        open_model_menu(&other_app).label(),
        text("Change agent (Opus · high)")
    );
}

#[test]
fn the_effort_and_fast_entries_are_optional() {
    let mut bare = spec();
    bare.efforts = Vec::new();
    bare.fast_item = false;
    let (app, _) = setup(&bare);
    assert_eq!(
        item_labels(&open_model_menu(&app)),
        vec![
            "Opus high Opens in new chat",
            "Sonnet medium Fast mode on",
            "Haiku Opens in new chat",
            "Add models",
        ]
    );

    let mut effort_only = spec();
    effort_only.fast_item = false;
    let (app, _) = setup(&effort_only);
    assert_eq!(item_labels(&open_model_menu(&app))[4..], ["Effort medium"]);

    let mut fast_only = spec();
    fast_only.efforts = Vec::new();
    let (app, _) = setup(&fast_only);
    assert_eq!(item_labels(&open_model_menu(&app))[4..], ["Fast"]);
}

#[test]
fn only_a_model_that_is_not_current_opens_in_a_new_chat() {
    let mut current_opus = spec();
    current_opus.current = 0;
    let (app, _) = setup(&current_opus);
    let labels = item_labels(&open_model_menu(&app));
    assert_eq!(labels[0], "Opus high");
    assert_eq!(labels[1], "Sonnet medium Fast mode on");
    assert_eq!(labels[2], "Haiku Opens in new chat");
}

#[test]
fn pressing_a_model_closes_the_menu_and_switches_to_it() {
    let mut start = spec();
    start.current = 0;
    let (app, menus) = setup(&start);
    let reactions = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&reactions);
    menus.on_new_chat(move |name| seen.borrow_mut().push(name.to_owned()));

    let menu = open_model_menu(&app);
    item(&menu, "Sonnet medium Fast mode on")
        .press()
        .expect("press");
    assert!(!menus.menu_open());
    assert!(open_menu(&app).is_none());
    // The model's own effort and fast apply; Sonnet does not open a new chat.
    assert_eq!(menus.shown(), "Change agent (Sonnet · medium · Fast)");
    assert_eq!(agent_pop_up(&app).label(), text(&menus.shown()));
    assert!(reactions.borrow().is_empty());

    let menu = open_model_menu(&app);
    assert_eq!(menu.label(), text("Change agent (Sonnet · medium · Fast)"));
    assert_eq!(item(&menu, "Effort medium").label(), text("Effort medium"));
    assert_eq!(item(&menu, "Fast").flag(), Some(true));
}

#[test]
fn pressing_a_new_chat_model_runs_every_reaction_with_its_name() {
    let (app, menus) = setup(&spec());
    let log = Rc::new(RefCell::new(Vec::new()));
    let first = Rc::clone(&log);
    menus.on_new_chat(move |name| first.borrow_mut().push(format!("first {name}")));
    let second = Rc::clone(&log);
    menus.on_new_chat(move |name| second.borrow_mut().push(format!("second {name}")));

    item(&open_model_menu(&app), "Opus high Opens in new chat")
        .press()
        .expect("press");
    assert_eq!(menus.shown(), "Change agent (Opus · high)");
    assert!(!menus.menu_open());
    assert_eq!(*log.borrow(), vec!["first Opus", "second Opus"]);

    // Haiku has no effort label: the pop-up shows its name alone.
    item(&open_model_menu(&app), "Haiku Opens in new chat")
        .press()
        .expect("press");
    assert_eq!(menus.shown(), "Change agent (Haiku)");
    assert_eq!(log.borrow().len(), 4);
    assert_eq!(log.borrow()[3], "second Haiku");
}

#[test]
fn ignored_model_presses_close_the_menu_and_change_nothing() {
    let (app, menus) = setup(&spec());
    let log = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&log);
    menus.on_new_chat(move |name| seen.borrow_mut().push(name.to_owned()));
    menus.ignore_model_presses();

    let menu = open_model_menu(&app);
    item(&menu, "Opus high Opens in new chat")
        .press()
        .expect("press");
    assert!(!menus.menu_open());
    assert!(open_menu(&app).is_none());
    assert_eq!(menus.shown(), "Change agent (Sonnet · medium · Fast)");
    assert!(log.borrow().is_empty());
}

#[test]
fn pressing_effort_opens_the_nested_menu_under_the_model_menu() {
    let (app, menus) = setup(&spec());
    let model_menu = open_model_menu(&app);
    assert!(nested_menu(&model_menu).is_none());

    item(&model_menu, "Effort medium").press().expect("press");
    // model menu → AXGroup/AXApplicationGroup → AXGroup → AXMenu.
    let nested = nested_menu(&model_menu).expect("nested menu");
    assert_eq!(nested.label(), text("Effort medium"));
    assert_eq!(item_labels(&nested), vec!["low", "medium", "high"]);
    assert!(kids(&nested)
        .iter()
        .all(|node| node.role() == text("AXMenuItem")));
    // The model menu is still the only menu under the slot.
    assert!(same(&open_menu(&app).expect("menu"), &model_menu));
    assert!(menus.menu_open());
}

#[test]
fn pressing_a_nested_item_sets_the_effort_and_keeps_the_model_menu_open() {
    let (app, menus) = setup(&spec());
    let model_menu = open_model_menu(&app);
    item(&model_menu, "Effort medium").press().expect("press");
    let nested = nested_menu(&model_menu).expect("nested menu");

    item(&nested, "low").press().expect("press");
    assert!(nested_menu(&model_menu).is_none());
    assert!(menus.menu_open());
    assert_eq!(
        item_labels(&model_menu)[3..],
        ["Add models", "Effort low", "Fast"]
    );
    assert_eq!(menus.shown(), "Change agent (Sonnet · low · Fast)");
    assert_eq!(agent_pop_up(&app).label(), text(&menus.shown()));

    // The new effort is what the submenu is labelled with next time.
    item(&model_menu, "Effort low").press().expect("press");
    assert_eq!(
        nested_menu(&model_menu).expect("nested").label(),
        text("Effort low")
    );
}

#[test]
fn closing_the_model_menu_closes_the_nested_menu() {
    let (app, menus) = setup(&spec());
    let model_menu = open_model_menu(&app);
    item(&model_menu, "Effort medium").press().expect("press");
    assert!(nested_menu(&model_menu).is_some());

    agent_pop_up(&app).press().expect("press");
    assert!(!menus.menu_open());
    assert!(open_menu(&app).is_none());

    let reopened = open_model_menu(&app);
    assert!(nested_menu(&reopened).is_none());
}

#[test]
fn ignored_effort_presses_close_the_submenu_and_change_nothing() {
    let (app, menus) = setup(&spec());
    menus.ignore_effort_presses();
    let model_menu = open_model_menu(&app);
    item(&model_menu, "Effort medium").press().expect("press");
    let nested = nested_menu(&model_menu).expect("nested menu");

    item(&nested, "high").press().expect("press");
    assert!(nested_menu(&model_menu).is_none());
    assert!(menus.menu_open());
    assert_eq!(
        item(&model_menu, "Effort medium").label(),
        text("Effort medium")
    );
    assert_eq!(menus.shown(), "Change agent (Sonnet · medium · Fast)");
}

#[test]
fn pressing_fast_flips_the_model_the_item_and_the_pop_up() {
    let (app, menus) = setup(&spec());
    let model_menu = open_model_menu(&app);
    let fast = item(&model_menu, "Fast");
    assert_eq!(fast.flag(), Some(true));

    fast.press().expect("press");
    assert_eq!(fast.flag(), Some(false));
    assert_eq!(menus.shown(), "Change agent (Sonnet · medium)");
    assert!(menus.menu_open());

    fast.press().expect("press");
    assert_eq!(fast.flag(), Some(true));
    assert_eq!(menus.shown(), "Change agent (Sonnet · medium · Fast)");
    assert!(menus.menu_open());

    // It is the current model's fast: Sonnet keeps it when the menu is reopened.
    fast.press().expect("press");
    agent_pop_up(&app).press().expect("press");
    let reopened = open_model_menu(&app);
    assert_eq!(item(&reopened, "Fast").flag(), Some(false));
    assert_eq!(
        item(&reopened, "Sonnet medium").label(),
        text("Sonnet medium")
    );
}

#[test]
fn pressing_add_opens_and_closes_the_add_menu() {
    let (app, menus) = setup(&spec());
    let pop_up = add_pop_up(&app);

    pop_up.press().expect("press");
    assert!(menus.menu_open());
    let menu = open_menu(&app).expect("the Add menu");
    assert_eq!(menu.label(), text("Add"));
    assert_eq!(
        item_labels(&menu),
        vec![
            "Link issue ⌘ I",
            "Plan mode ⇧ Tab",
            "Link workspaces",
            "Add attachment ⌘ U",
        ]
    );
    assert!(kids(&menu)
        .iter()
        .all(|node| node.role() == text("AXMenuItem")));

    pop_up.press().expect("press");
    assert!(!menus.menu_open());
    assert!(open_menu(&app).is_none());
}

#[test]
fn the_plan_item_follows_the_plan_mode() {
    let mut on = spec();
    on.plan = Some(true);
    let (app, menus) = setup(&on);
    assert_eq!(menus.plan(), Some(true));
    assert_eq!(item_labels(&open_add_menu(&app))[1], "Exit plan mode ⇧ Tab");

    let mut none = spec();
    none.plan = None;
    let (app, menus) = setup(&none);
    assert_eq!(menus.plan(), None);
    assert_eq!(
        item_labels(&open_add_menu(&app)),
        vec!["Link issue ⌘ I", "Link workspaces", "Add attachment ⌘ U"]
    );
}

#[test]
fn pressing_the_plan_item_flips_plan_and_closes_the_menu() {
    let (app, menus) = setup(&spec());
    let menu = open_add_menu(&app);
    item(&menu, "Plan mode ⇧ Tab").press().expect("press");
    assert_eq!(menus.plan(), Some(true));
    assert!(!menus.menu_open());
    assert!(open_menu(&app).is_none());

    let menu = open_add_menu(&app);
    assert_eq!(item_labels(&menu)[1], "Exit plan mode ⇧ Tab");
    item(&menu, "Exit plan mode ⇧ Tab").press().expect("press");
    assert_eq!(menus.plan(), Some(false));
    assert!(!menus.menu_open());
}

#[test]
fn the_other_add_items_change_nothing() {
    let (app, menus) = setup(&spec());
    let menu = open_add_menu(&app);
    item(&menu, "Link workspaces").press().expect("press");
    assert!(menus.menu_open());
    assert_eq!(menus.plan(), Some(false));
}

#[test]
fn kept_open_menus_stay_open_when_their_pop_up_is_pressed() {
    let (app, menus) = setup(&spec());
    menus.keep_menus_open();

    let model_menu = open_model_menu(&app);
    agent_pop_up(&app).press().expect("press");
    assert!(menus.menu_open());
    assert!(same(&open_menu(&app).expect("menu"), &model_menu));

    // A model item still closes it.
    item(&model_menu, "Sonnet medium Fast mode on")
        .press()
        .expect("press");
    assert!(!menus.menu_open());

    let add_menu = open_add_menu(&app);
    add_pop_up(&app).press().expect("press");
    assert!(menus.menu_open());
    assert!(same(&open_menu(&app).expect("menu"), &add_menu));
}

#[test]
fn clones_share_the_same_controls() {
    let (app, menus) = setup(&spec());
    let clone = menus.clone();
    clone.ignore_model_presses();
    open_model_menu(&app);
    assert!(menus.menu_open());
    let menu = open_menu(&app).expect("menu");
    item(&menu, "Opus high Opens in new chat")
        .press()
        .expect("press");
    assert_eq!(clone.shown(), menus.shown());
    assert_eq!(menus.shown(), "Change agent (Sonnet · medium · Fast)");
}
