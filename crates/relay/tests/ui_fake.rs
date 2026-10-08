//! The UI contract over the fakes: the fake Conductor window, the fake desktop's event log, the
//! command target and the error classification. Nothing here reaches the Mac.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use conductor_remote::ui::ax::AxError;
use conductor_remote::ui::desktop::Desktop;
use conductor_remote::ui::driver::{Tab, Target, UiError};
use conductor_remote::ui::fake::{
    conductor_app, main_pane, show_workspace, FakeDesktop, FakeEvent, FakeNode, WindowSpec,
};
use conductor_remote::ui::keys::{Key, Modifiers};
use conductor_remote::ui::node::UiNode;
use conductor_remote::ui::screen::SessionState;

const COMPOSER: &str = "Ask to make changes, @mention files, run /commands";

fn spec() -> WindowSpec {
    WindowSpec {
        repo: "relay".to_owned(),
        branch: "user/feature-x".to_owned(),
        sidebar: vec!["alpha".to_owned(), "beta".to_owned()],
        chats: vec!["One".to_owned(), "Two".to_owned(), "Three".to_owned()],
        selected: 1,
        composer_value: Some("draft".to_owned()),
    }
}

/// The children of a node, which must be readable.
fn kids(node: &FakeNode) -> Vec<FakeNode> {
    node.children().expect("children")
}

fn roles(nodes: &[FakeNode]) -> Vec<Option<String>> {
    nodes.iter().map(UiNode::role).collect()
}

fn role(role: &str) -> Option<String> {
    Some(role.to_owned())
}

fn chat_radios(app: &FakeNode) -> Vec<FakeNode> {
    let pane = main_pane(app);
    let strip = &kids(&pane)[2];
    kids(&kids(strip)[0])
}

fn command() -> Modifiers {
    Modifiers {
        command: true,
        ..Modifiers::default()
    }
}

#[test]
fn conductor_app_has_the_probe_shape() {
    let app = conductor_app(&spec());
    assert_eq!(app.role(), role("AXApplication"));

    let window = &kids(&app)[0];
    assert_eq!(window.role(), role("AXWindow"));
    assert_eq!(window.label(), Some("Conductor".to_owned()));
    let group = &kids(window)[0];
    assert_eq!(group.role(), role("AXGroup"));
    let inner = &kids(group)[0];
    assert_eq!(inner.role(), role("AXGroup"));
    let scroll = &kids(inner)[0];
    assert_eq!(scroll.role(), role("AXScrollArea"));
    let web = &kids(scroll)[0];
    assert_eq!(web.role(), role("AXWebArea"));

    let landmarks = kids(web);
    assert_eq!(landmarks.len(), 2);
    let sidebar = &landmarks[0];
    assert_eq!(sidebar.role(), role("AXGroup"));
    assert_eq!(sidebar.subrole(), role("AXLandmarkComplementary"));
    let links = kids(sidebar);
    assert_eq!(roles(&links), vec![role("AXLink"), role("AXLink")]);
    assert_eq!(links[0].label(), Some("alpha".to_owned()));
    assert_eq!(links[1].label(), Some("beta".to_owned()));
    assert_eq!(
        app.find_label("beta").map(|link| link.role()),
        Some(role("AXLink"))
    );

    let main = &landmarks[1];
    assert_eq!(main.role(), role("AXGroup"));
    assert_eq!(main.subrole(), role("AXLandmarkMain"));
    assert_eq!(main_pane(&app).subrole(), role("AXLandmarkMain"));
    let parts = kids(main);
    assert_eq!(
        roles(&parts),
        vec![
            role("AXPopUpButton"),
            role("AXStaticText"),
            role("AXTabGroup"),
            role("AXButton"),
            role("AXGroup"),
            role("AXTabGroup"),
        ]
    );
    assert_eq!(parts[0].label(), Some("relay relay".to_owned()));
    assert_eq!(parts[1].label(), None);
    assert_eq!(parts[1].value(), Ok(Some("feature-x".to_owned())));
    assert_eq!(parts[3].label(), None);

    let radios = chat_radios(&app);
    assert_eq!(radios.len(), 3);
    assert_eq!(
        radios.iter().map(UiNode::label).collect::<Vec<_>>(),
        vec![
            Some("Close chat One".to_owned()),
            Some("Close chat Two".to_owned()),
            Some("Close chat Three".to_owned()),
        ]
    );
    assert!(radios
        .iter()
        .all(|radio| radio.role() == role("AXRadioButton")));
    assert_eq!(
        radios.iter().map(UiNode::selected).collect::<Vec<_>>(),
        vec![Some(false), Some(true), Some(false)]
    );

    let composer = &parts[4];
    assert_eq!(composer.subrole(), role("AXLandmarkForm"));
    assert_eq!(composer.label(), Some("composer".to_owned()));
    let form = kids(composer);
    assert_eq!(roles(&form), vec![role("AXTextArea"), role("AXButton")]);
    assert_eq!(form[0].label(), Some(COMPOSER.to_owned()));
    assert_eq!(form[0].value(), Ok(Some("draft".to_owned())));
    assert_eq!(form[1].label(), None);
    assert_eq!(
        app.find_role("AXTextArea").and_then(|area| area.label()),
        Some(COMPOSER.to_owned())
    );

    let right = kids(&parts[5]);
    assert_eq!(roles(&right), vec![role("AXRadioButton")]);
    assert_eq!(right[0].label(), Some("Setup".to_owned()));
    assert_eq!(
        app.find_label("Setup").and_then(|setup| setup.role()),
        role("AXRadioButton")
    );
}

#[test]
fn show_workspace_sets_the_pop_up_and_the_branch_tail() {
    let desktop = FakeDesktop::new(conductor_app(&spec()));
    let app = desktop.app();
    show_workspace(&app, "other", "team/fix-2");
    let parts = kids(&main_pane(&app));
    assert_eq!(parts[0].role(), role("AXPopUpButton"));
    assert_eq!(parts[0].label(), Some("other other".to_owned()));
    assert_eq!(parts[1].role(), role("AXStaticText"));
    assert_eq!(parts[1].label(), None);
    assert_eq!(parts[1].value(), Ok(Some("fix-2".to_owned())));

    // A branch without a `/` is its own tail.
    show_workspace(&app, "relay", "main");
    assert_eq!(parts[0].label(), Some("relay relay".to_owned()));
    assert_eq!(parts[1].value(), Ok(Some("main".to_owned())));
    assert!(desktop.events().is_empty());
}

#[test]
fn conductor_app_without_chats_or_a_draft() {
    let app = conductor_app(&WindowSpec {
        chats: Vec::new(),
        selected: 5,
        composer_value: None,
        ..spec()
    });
    assert!(chat_radios(&app).is_empty());
    let area = app.find_role("AXTextArea").expect("text area");
    assert_eq!(area.value(), Ok(None));
}

#[test]
fn nodes_never_given_a_selected_flag_read_none() {
    let app = conductor_app(&spec());
    assert_eq!(app.selected(), None);
    assert_eq!(app.find_role("AXTextArea").unwrap().selected(), None);
    assert!(!app.is_selected());
}

#[test]
fn pressing_a_radio_selects_only_it() {
    let desktop = FakeDesktop::new(conductor_app(&spec()));
    let radios = chat_radios(&desktop.app());
    radios[2].press().unwrap();
    assert_eq!(
        radios.iter().map(FakeNode::is_selected).collect::<Vec<_>>(),
        vec![false, false, true]
    );
    radios[0].press().unwrap();
    assert_eq!(
        radios.iter().map(FakeNode::is_selected).collect::<Vec<_>>(),
        vec![true, false, false]
    );
    assert!(desktop.app().find_label("Setup").unwrap().is_selected());
    assert_eq!(
        desktop.events(),
        vec![
            FakeEvent::Press(Some("Close chat Three".to_owned())),
            FakeEvent::Press(Some("Close chat One".to_owned())),
        ]
    );
}

#[test]
fn every_call_records_its_event_in_order() {
    let desktop = FakeDesktop::new(conductor_app(&spec()));
    let app = desktop.application(1);
    let area = app.find_label(COMPOSER).unwrap();
    let pane = main_pane(&app);
    let added = FakeNode::new("AXButton").with_label("Later");
    pane.add_child(added.clone());
    let nested = FakeNode::new("AXGroup").with_child(FakeNode::new("AXButton").with_label("Deep"));
    pane.add_child(nested);

    area.set_focused(true).unwrap();
    area.set_value("hello").unwrap();
    assert!(desktop.open_url("conductor://workspace?id=a"));
    assert!(desktop.activate(7));
    desktop
        .post_key(4242, Key::Return, Modifiers::default())
        .unwrap();
    desktop.pause(Duration::from_millis(250));
    added.press().unwrap();
    app.find_label("Deep").unwrap().press().unwrap();
    area.set_focused(false).unwrap();
    desktop.post_key(4242, Key::K, command()).unwrap();

    assert_eq!(
        desktop.events(),
        vec![
            FakeEvent::SetFocused {
                label: Some(COMPOSER.to_owned()),
                focused: true,
            },
            FakeEvent::SetValue {
                label: Some(COMPOSER.to_owned()),
                value: "hello".to_owned(),
            },
            FakeEvent::OpenUrl("conductor://workspace?id=a".to_owned()),
            FakeEvent::Activate(7),
            FakeEvent::Key {
                pid: 4242,
                key: Key::Return,
                modifiers: Modifiers::default(),
            },
            FakeEvent::Pause(Duration::from_millis(250)),
            FakeEvent::Press(Some("Later".to_owned())),
            FakeEvent::Press(Some("Deep".to_owned())),
            FakeEvent::SetFocused {
                label: Some(COMPOSER.to_owned()),
                focused: false,
            },
            FakeEvent::Key {
                pid: 4242,
                key: Key::K,
                modifiers: command(),
            },
        ]
    );
    assert_eq!(area.value_text(), Some("hello".to_owned()));
    assert!(!area.is_focused());
    assert_eq!(desktop.frontmost_pid(), Some(7));
    assert_eq!(
        added.parent().and_then(|parent| parent.subrole()),
        role("AXLandmarkMain")
    );
}

#[test]
fn a_hand_built_tree_records_into_the_desktop() {
    let leaf = FakeNode::new("AXButton").with_label("Leaf");
    let tree = FakeNode::new("AXApplication")
        .with_child(FakeNode::new("AXGroup").with_child(leaf.clone()));
    let desktop = FakeDesktop::new(tree);
    leaf.press().unwrap();
    leaf.set_focused(true).unwrap();
    assert!(leaf.is_focused());
    assert_eq!(
        desktop.events(),
        vec![
            FakeEvent::Press(Some("Leaf".to_owned())),
            FakeEvent::SetFocused {
                label: Some("Leaf".to_owned()),
                focused: true,
            },
        ]
    );
    assert_eq!(
        leaf.parent()
            .and_then(|parent| parent.parent())
            .and_then(|app| app.role()),
        role("AXApplication")
    );
    assert!(desktop.app().parent().is_none());
}

#[test]
fn the_desktop_starts_trusted_unlocked_and_with_conductor_in_front() {
    let desktop = FakeDesktop::new(FakeNode::new("AXApplication"));
    assert!(desktop.trusted());
    assert_eq!(
        desktop.session(),
        Some(SessionState {
            locked: false,
            on_console: true,
        })
    );
    assert_eq!(desktop.conductor_pid(), Some(4242));
    assert_eq!(desktop.frontmost_pid(), Some(4242));

    desktop.set_trusted(false);
    desktop.set_session(None);
    desktop.set_conductor_pid(None);
    desktop.set_frontmost(Some(9));
    assert!(!desktop.trusted());
    assert_eq!(desktop.session(), None);
    assert_eq!(desktop.conductor_pid(), None);
    assert_eq!(desktop.frontmost_pid(), Some(9));
    assert!(desktop.events().is_empty());
}

#[test]
fn ignored_value_writes_record_and_change_nothing() {
    let desktop = FakeDesktop::new(conductor_app(&spec()));
    let area = desktop.app().find_role("AXTextArea").unwrap();
    area.ignore_value_writes();
    area.set_value("typed").unwrap();
    assert_eq!(area.value(), Ok(Some("draft".to_owned())));
    assert_eq!(
        desktop.events(),
        vec![FakeEvent::SetValue {
            label: Some(COMPOSER.to_owned()),
            value: "typed".to_owned(),
        }]
    );
    area.set_value_text(None);
    assert_eq!(area.value(), Ok(None));
    assert_eq!(desktop.events().len(), 1);
}

#[test]
fn failing_children_and_values_fail_every_later_read() {
    let app = conductor_app(&spec());
    let pane = main_pane(&app);
    pane.fail_children(AxError::CannotComplete);
    assert_eq!(pane.children().err(), Some(AxError::CannotComplete));
    assert_eq!(pane.children().err(), Some(AxError::CannotComplete));

    let area = app.find_role("AXTextArea").unwrap();
    area.fail_value(AxError::InvalidUiElement);
    assert_eq!(area.value(), Err(AxError::InvalidUiElement));
    assert_eq!(area.value(), Err(AxError::InvalidUiElement));
    // The other reads still answer.
    assert_eq!(area.label(), Some(COMPOSER.to_owned()));
}

#[test]
fn refused_activation_returns_false_and_changes_nothing() {
    let desktop = FakeDesktop::new(FakeNode::new("AXApplication"));
    desktop.set_frontmost(Some(1));
    desktop.refuse_activation();
    assert!(!desktop.activate(4242));
    assert_eq!(desktop.frontmost_pid(), Some(1));
    assert_eq!(desktop.events(), vec![FakeEvent::Activate(4242)]);
}

#[test]
fn failing_keys_record_and_fail_without_the_reaction() {
    let desktop = FakeDesktop::new(FakeNode::new("AXApplication"));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    desktop.on_key(move |key, modifiers| sink.borrow_mut().push((key, modifiers)));

    desktop
        .post_key(4242, Key::Escape, Modifiers::default())
        .unwrap();
    desktop.fail_keys("no event");
    assert_eq!(
        desktop.post_key(4242, Key::Return, command()),
        Err("no event".to_owned())
    );
    assert_eq!(
        desktop.post_key(4242, Key::Space, Modifiers::default()),
        Err("no event".to_owned())
    );
    assert_eq!(*seen.borrow(), vec![(Key::Escape, Modifiers::default())]);
    assert_eq!(desktop.events().len(), 3);
}

#[test]
fn reactions_run_after_their_event_is_recorded() {
    let desktop = FakeDesktop::new(conductor_app(&spec()));

    let area = desktop.app().find_role("AXTextArea").unwrap();
    let on_key = area.clone();
    desktop.on_key(move |key, _| {
        if key == Key::Return {
            on_key.set_value_text(None);
        }
    });
    let urls = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&urls);
    desktop.on_open_url(move |url| sink.borrow_mut().push(url.to_owned()));

    let pressed = Rc::new(RefCell::new(0));
    let count = Rc::clone(&pressed);
    let button = main_pane(&desktop.app()).find_role("AXButton").unwrap();
    button.on_press(move |node| {
        assert_eq!(node.label_text(), None);
        *count.borrow_mut() += 1;
    });

    desktop
        .post_key(4242, Key::Return, Modifiers::default())
        .unwrap();
    assert_eq!(area.value_text(), None);
    assert!(desktop.open_url("conductor://x"));
    assert_eq!(*urls.borrow(), vec!["conductor://x".to_owned()]);
    button.press().unwrap();
    button.press().unwrap();
    assert_eq!(*pressed.borrow(), 2);
    assert_eq!(
        desktop.events(),
        vec![
            FakeEvent::Key {
                pid: 4242,
                key: Key::Return,
                modifiers: Modifiers::default(),
            },
            FakeEvent::OpenUrl("conductor://x".to_owned()),
            FakeEvent::Press(None),
            FakeEvent::Press(None),
        ]
    );
}

#[test]
fn a_node_dropped_from_the_tree_keeps_its_last_state() {
    let area = {
        let desktop = FakeDesktop::new(conductor_app(&spec()));
        let area = desktop.app().find_role("AXTextArea").unwrap();
        area.set_value("kept").unwrap();
        area.set_label("Renamed");
        area
    };
    assert_eq!(area.value_text(), Some("kept".to_owned()));
    assert_eq!(area.label_text(), Some("Renamed".to_owned()));
    assert!(area.parent().is_none());
}

fn target(session_id: Option<&str>) -> Target {
    Target {
        workspace_id: "ws 1".to_owned(),
        session_id: session_id.map(str::to_owned),
        repo: Some("relay".to_owned()),
        branch: "user/feature-x".to_owned(),
        workspace_name: Some("alpha".to_owned()),
        tab: Some(Tab {
            index: 2,
            count: 3,
            title: Some("Two".to_owned()),
        }),
    }
}

#[test]
fn deep_link_percent_encodes_both_ids() {
    assert_eq!(
        target(Some("s/2")).deep_link(),
        "conductor://workspace?id=ws%201&session=s%2F2"
    );
    assert_eq!(target(None).deep_link(), "conductor://workspace?id=ws%201");
    let mut unreserved = target(Some("A-z.0_9~é"));
    unreserved.workspace_id = "a&b=c?d#e%".to_owned();
    assert_eq!(
        unreserved.deep_link(),
        "conductor://workspace?id=a%26b%3Dc%3Fd%23e%25&session=A-z.0_9~%C3%A9"
    );
}

#[test]
fn branch_tail_is_the_part_after_the_last_slash() {
    assert_eq!(target(None).branch_tail(), "feature-x");
    let mut main = target(None);
    main.branch = "main".to_owned();
    assert_eq!(main.branch_tail(), "main");
    main.branch = String::new();
    assert_eq!(main.branch_tail(), "");
}

#[test]
fn ax_errors_map_to_ui_errors() {
    assert_eq!(
        UiError::from(AxError::CannotComplete),
        UiError::NotResponding
    );
    assert_eq!(UiError::from(AxError::ApiDisabled), UiError::NotTrusted);
    assert_eq!(
        UiError::from(AxError::InvalidUiElement),
        UiError::Ax(AxError::InvalidUiElement)
    );
    assert_eq!(
        UiError::from(AxError::Other(-1)),
        UiError::Ax(AxError::Other(-1))
    );
}

/// Every variant, with whether it sent nothing, whether a retry won't help, and whether it is
/// the lock.
fn every_variant() -> Vec<(UiError, bool, bool, bool)> {
    vec![
        (UiError::NotTrusted, true, true, false),
        (UiError::Locked, true, false, true),
        (UiError::NotRunning, true, true, false),
        (UiError::NoWindow, true, false, false),
        (UiError::NotResponding, false, false, false),
        (UiError::NotFrontmost, true, false, false),
        (UiError::NoBranch, true, true, false),
        (
            UiError::WorkspaceNotFocused("alpha".to_owned()),
            true,
            false,
            false,
        ),
        (UiError::NoChatStrip, true, false, false),
        (UiError::SeveralTabs("Two".to_owned()), true, false, false),
        (UiError::TabNotFound(2), true, false, false),
        (UiError::TabNotSelected, true, false, false),
        (UiError::NoComposer, true, false, false),
        (UiError::ComposerRejected, true, false, false),
        (UiError::StillInComposer, false, false, false),
        (UiError::Key("no event".to_owned()), false, false, false),
        (UiError::Ax(AxError::Failure), false, false, false),
    ]
}

#[test]
fn ui_errors_classify_every_variant() {
    for (error, sent_nothing, retry_wont_help, is_lock) in every_variant() {
        assert_eq!(
            error.sent_nothing(),
            sent_nothing,
            "sent_nothing of {error:?}"
        );
        assert_eq!(
            error.retry_wont_help(),
            retry_wont_help,
            "retry_wont_help of {error:?}"
        );
        assert_eq!(error.is_lock(), is_lock, "is_lock of {error:?}");
    }
}

#[test]
fn only_the_lock_text_starts_with_the_mac_is_locked() {
    assert!(UiError::Locked.to_string().starts_with("The Mac is locked"));
    for (error, _, _, is_lock) in every_variant() {
        assert_eq!(
            error.to_string().starts_with("The Mac is locked"),
            is_lock,
            "{error:?}"
        );
    }
}

#[test]
fn ui_error_texts_carry_their_values() {
    assert_eq!(
        UiError::WorkspaceNotFocused("alpha".to_owned()).to_string(),
        "couldn't open alpha in Conductor - open the workspace on your Mac and try again."
    );
    assert_eq!(UiError::TabNotFound(2).to_string(), "chat tab 2 not found");
    assert_eq!(
        UiError::SeveralTabs("Two".to_owned()).to_string(),
        "several chat tabs match Two"
    );
    assert_eq!(
        UiError::Key("no event".to_owned()).to_string(),
        "couldn't press a key: no event"
    );
    assert_eq!(
        UiError::Ax(AxError::Failure).to_string(),
        "Accessibility error: a system error occurred"
    );
}
