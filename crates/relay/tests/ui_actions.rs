//! The UI actions over the fake desktop: focus, tab selection, the composer, the keys, and every
//! way a command fails. Nothing here reaches the Mac.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::ax::AxError;
use conductor_remote::ui::driver::{Tab, Target, UiDriver, UiError, ViewReport};
use conductor_remote::ui::fake::{
    conductor_app, main_pane, show_workspace, FakeDesktop, FakeEvent, FakeNode, WindowSpec,
};
use conductor_remote::ui::keys::{Key, Modifiers};
use conductor_remote::ui::node::UiNode;
use conductor_remote::ui::screen::SessionState;

const COMPOSER: &str = "Ask to make changes, @mention files, run /commands";
const PID: i32 = 4242;
const LINK: &str = "conductor://workspace?id=ws-1&session=s-2";
const TARGET_HEADER: &str = "relay relay";
const PROMPT: &str = "fix the tests";

/// A window showing another workspace of the same repo until something lands the target.
fn spec() -> WindowSpec {
    WindowSpec {
        repo: "relay".to_owned(),
        branch: "other".to_owned(),
        sidebar: vec!["alpha".to_owned(), "beta".to_owned()],
        chats: vec!["One".to_owned(), "Two".to_owned(), "Three".to_owned()],
        selected: 1,
        composer_value: Some("draft".to_owned()),
    }
}

/// The second chat of workspace "beta" on `user/feature-x`, already selected in `spec()`.
fn target() -> Target {
    Target {
        workspace_id: "ws-1".to_owned(),
        session_id: Some("s-2".to_owned()),
        repo: Some("relay".to_owned()),
        branch: "user/feature-x".to_owned(),
        workspace_name: Some("beta".to_owned()),
        tab: Some(Tab {
            index: 2,
            count: 3,
            title: Some("Two".to_owned()),
        }),
    }
}

/// Makes `app` show the target's workspace, as landing on it does.
fn land(app: &FakeNode) {
    show_workspace(app, "relay", "user/feature-x");
}

fn tab(index: usize, count: usize, title: Option<&str>) -> Option<Tab> {
    Some(Tab {
        index,
        count,
        title: title.map(str::to_owned),
    })
}

struct Setup {
    driver: Driver<FakeDesktop>,
    app: FakeNode,
    area: FakeNode,
}

impl Setup {
    fn new(spec: &WindowSpec) -> Setup {
        let app = conductor_app(spec);
        let pane = main_pane(&app);
        let area = pane.find_role("AXTextArea").expect("composer");
        Setup {
            driver: Driver::new(FakeDesktop::new(app.clone())),
            app,
            area,
        }
    }

    /// Lands the deep link and makes Return empty the composer.
    fn working(spec: &WindowSpec) -> Setup {
        let setup = Setup::new(spec);
        setup.land_on_open_url();
        setup.return_empties_composer();
        setup
    }

    fn desktop(&self) -> &FakeDesktop {
        self.driver.desktop()
    }

    fn land_on_open_url(&self) {
        let app = self.app.clone();
        self.desktop().on_open_url(move |_| land(&app));
    }

    fn return_empties_composer(&self) {
        let area = self.area.clone();
        self.desktop().on_key(move |key, _| {
            if key == Key::Return {
                area.set_value_text(Some(""));
            }
        });
    }

    fn radio(&self, title: &str) -> FakeNode {
        self.app
            .find_label(&format!("Close chat {title}"))
            .expect("radio")
    }

    fn events(&self) -> Vec<FakeEvent> {
        self.desktop().events()
    }

    /// The events without the pauses.
    fn actions(&self) -> Vec<FakeEvent> {
        self.events()
            .into_iter()
            .filter(|event| !matches!(event, FakeEvent::Pause(_)))
            .collect()
    }

    fn keys(&self) -> Vec<(Key, Modifiers)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                FakeEvent::Key { key, modifiers, .. } => Some((key, modifiers)),
                _ => None,
            })
            .collect()
    }

    fn presses(&self) -> Vec<Option<String>> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                FakeEvent::Press(label) => Some(label),
                _ => None,
            })
            .collect()
    }

    /// No key posted, no value written and no focus set.
    fn assert_untouched_composer(&self) {
        assert!(
            !self.events().iter().any(|event| matches!(
                event,
                FakeEvent::Key { .. } | FakeEvent::SetValue { .. } | FakeEvent::SetFocused { .. }
            )),
            "{:?}",
            self.events()
        );
    }
}

fn plain() -> Modifiers {
    Modifiers::default()
}

fn command() -> Modifiers {
    Modifiers {
        command: true,
        ..Modifiers::default()
    }
}

fn set_value(value: &str) -> FakeEvent {
    FakeEvent::SetValue {
        label: Some(COMPOSER.to_owned()),
        value: value.to_owned(),
    }
}

fn return_key(modifiers: Modifiers) -> FakeEvent {
    FakeEvent::Key {
        pid: PID,
        key: Key::Return,
        modifiers,
    }
}

fn not_focused() -> UiError {
    UiError::WorkspaceNotFocused("relay on feature-x".to_owned())
}

// ---- send ----

#[test]
fn send_types_into_the_selected_chat_and_presses_return() {
    let mut setup = Setup::working(&spec());
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(1));
    assert_eq!(
        setup.actions(),
        vec![
            FakeEvent::OpenUrl(LINK.to_owned()),
            FakeEvent::SetFocused {
                label: Some(COMPOSER.to_owned()),
                focused: true,
            },
            set_value(PROMPT),
            return_key(plain()),
        ]
    );
    assert!(setup
        .events()
        .contains(&FakeEvent::Pause(Duration::from_millis(250))));
    assert_eq!(setup.area.value_text(), Some(String::new()));
}

#[test]
fn send_accepts_a_nested_branch_without_its_owner_prefix() {
    let mut setup = Setup::new(&spec());
    let app = setup.app.clone();
    setup.desktop().on_open_url(move |_| {
        show_workspace(&app, "relay", "owner/topic/feature-x");
        main_pane(&app)
            .children()
            .unwrap()
            .into_iter()
            .find(|node| node.role().as_deref() == Some("AXStaticText"))
            .unwrap()
            .set_value_text(Some("topic/feature-x"));
    });
    setup.return_empties_composer();
    let mut destination = target();
    destination.branch = "owner/topic/feature-x".to_owned();
    assert_eq!(setup.driver.send_prompt(&destination, PROMPT, false), Ok(1));
    assert_eq!(setup.keys(), vec![(Key::Return, plain())]);
}

#[test]
fn send_refuses_other_nested_branches_with_a_shared_tail() {
    for shown in ["unrelated/feature-x", "topic/feature-x-old"] {
        let mut setup = Setup::new(&spec());
        let app = setup.app.clone();
        setup.desktop().on_open_url(move |_| {
            show_workspace(&app, "relay", "owner/topic/feature-x");
            main_pane(&app)
                .children()
                .unwrap()
                .into_iter()
                .find(|node| node.role().as_deref() == Some("AXStaticText"))
                .unwrap()
                .set_value_text(Some(shown));
        });
        let mut destination = target();
        destination.branch = "owner/topic/feature-x".to_owned();
        assert_eq!(
            setup.driver.send_prompt(&destination, PROMPT, false),
            Err(not_focused())
        );
        setup.assert_untouched_composer();
    }
}

#[test]
fn send_in_queue_mode_posts_command_return() {
    let mut setup = Setup::working(&spec());
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, true), Ok(1));
    assert_eq!(setup.keys(), vec![(Key::Return, command())]);
}

#[test]
fn send_selects_another_tab_first() {
    let mut setup = Setup::working(&spec());
    let mut target = target();
    target.tab = tab(3, 3, Some("Three"));
    assert_eq!(setup.driver.send_prompt(&target, PROMPT, false), Ok(1));
    let actions = setup.actions();
    assert_eq!(actions[0], FakeEvent::OpenUrl(LINK.to_owned()));
    assert_eq!(
        actions[1],
        FakeEvent::Press(Some("Close chat Three".to_owned()))
    );
    assert_eq!(actions[3], set_value(PROMPT));
    assert!(setup.radio("Three").is_selected());
    assert!(!setup.radio("Two").is_selected());
}

#[test]
fn send_finds_a_moved_tab_by_its_title() {
    let mut setup = Setup::working(&spec());
    let mut target = target();
    // The chat moved from position 1 to 3 since the list was read.
    target.tab = tab(1, 3, Some("Three"));
    assert_eq!(setup.driver.send_prompt(&target, PROMPT, false), Ok(1));
    assert_eq!(setup.presses(), vec![Some("Close chat Three".to_owned())]);
}

#[test]
fn send_falls_back_to_the_sidebar_link() {
    let mut setup = Setup::new(&spec());
    setup.return_empties_composer();
    let app = setup.app.clone();
    setup
        .app
        .find_label("beta")
        .expect("link")
        .on_press(move |_| land(&app));
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(1));
    let actions = setup.actions();
    assert_eq!(actions[0], FakeEvent::OpenUrl(LINK.to_owned()));
    assert_eq!(actions[1], FakeEvent::Press(Some("beta".to_owned())));
    assert_eq!(actions[3], set_value(PROMPT));
    let pauses_before_press = setup
        .events()
        .iter()
        .take_while(|event| !matches!(event, FakeEvent::Press(_)))
        .filter(|event| **event == FakeEvent::Pause(Duration::from_millis(150)))
        .count();
    assert_eq!(pauses_before_press, 10);
    assert!(setup
        .events()
        .contains(&FakeEvent::Pause(Duration::from_millis(900))));
}

#[test]
fn send_fails_when_neither_the_link_nor_the_sidebar_lands() {
    let mut setup = Setup::new(&spec());
    setup.return_empties_composer();
    let error = setup.driver.send_prompt(&target(), PROMPT, false);
    assert_eq!(error, Err(not_focused()));
    assert_eq!(
        error.unwrap_err().to_string(),
        "couldn't open relay on feature-x in Conductor - open the workspace on your Mac and try again."
    );
    setup.assert_untouched_composer();
    assert_eq!(setup.presses(), vec![Some("beta".to_owned())]);
    assert_eq!(setup.area.value_text(), Some("draft".to_owned()));
}

#[test]
fn send_without_a_repo_names_the_workspace() {
    let mut setup = Setup::new(&spec());
    let mut target = target();
    target.repo = None;
    target.workspace_name = None;
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(UiError::WorkspaceNotFocused(
            "the workspace on feature-x".to_owned()
        ))
    );
    setup.assert_untouched_composer();
    assert!(setup.presses().is_empty());
}

#[test]
fn send_refuses_a_pane_showing_another_branch() {
    let mut setup = Setup::new(&spec());
    setup.return_empties_composer();
    let app = setup.app.clone();
    setup
        .desktop()
        .on_open_url(move |_| show_workspace(&app, "relay", "user/feature-y"));
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(not_focused())
    );
    setup.assert_untouched_composer();
}

#[test]
fn send_refuses_a_branch_that_only_starts_with_the_targets() {
    let mut setup = Setup::new(&WindowSpec {
        branch: "fix-2".to_owned(),
        ..spec()
    });
    setup.return_empties_composer();
    let mut target = target();
    target.branch = "fix".to_owned();
    target.workspace_name = None;
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(UiError::WorkspaceNotFocused("relay on fix".to_owned()))
    );
    setup.assert_untouched_composer();
}

#[test]
fn send_refuses_a_repo_that_only_starts_with_the_targets() {
    let mut setup = Setup::new(&WindowSpec {
        repo: "relay-old".to_owned(),
        branch: "user/feature-x".to_owned(),
        ..spec()
    });
    setup.return_empties_composer();
    let mut target = target();
    target.workspace_name = None;
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(not_focused())
    );
    setup.assert_untouched_composer();
}

#[test]
fn send_accepts_the_branch_tail_in_the_header() {
    let mut setup = Setup::working(&spec());
    let app = setup.app.clone();
    setup
        .desktop()
        .on_open_url(move |_| show_workspace(&app, "relay", "feature-x"));
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(1));
}

#[test]
fn send_refuses_the_right_branch_tail_in_another_repository() {
    let mut setup = Setup::new(&spec());
    setup.return_empties_composer();
    let app = setup.app.clone();
    setup
        .desktop()
        .on_open_url(move |_| show_workspace(&app, "infra", "user/feature-x"));
    let mut target = target();
    target.workspace_name = None;
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(not_focused())
    );
    setup.assert_untouched_composer();
}

#[test]
fn send_refuses_a_branch_that_only_starts_with_the_tail() {
    let mut setup = Setup::new(&spec());
    setup.return_empties_composer();
    let app = setup.app.clone();
    setup
        .desktop()
        .on_open_url(move |_| show_workspace(&app, "relay", "user/feature-x-2"));
    let mut target = target();
    target.workspace_name = None;
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(not_focused())
    );
    setup.assert_untouched_composer();
}

#[test]
fn send_refuses_a_branch_tail_nested_below_the_pane() {
    let mut setup = Setup::new(&spec());
    setup.return_empties_composer();
    // The pane's own static text shows another branch; the tail sits one level deeper.
    main_pane(&setup.app).add_child(
        FakeNode::new("AXGroup").with_child(FakeNode::new("AXStaticText").with_value("feature-x")),
    );
    let mut target = target();
    target.workspace_name = None;
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(not_focused())
    );
    setup.assert_untouched_composer();
}

#[test]
fn send_refuses_a_pane_without_a_static_text() {
    // The repo's pop-up and the composer, and the tail only as the value of another role.
    let pane = FakeNode::new("AXGroup")
        .with_subrole("AXLandmarkMain")
        .with_child(FakeNode::new("AXPopUpButton").with_label(TARGET_HEADER))
        .with_child(FakeNode::new("AXTextField").with_value("feature-x"))
        .with_child(
            FakeNode::new("AXGroup")
                .with_label("composer")
                .with_child(FakeNode::new("AXTextArea").with_label(COMPOSER)),
        );
    let app = FakeNode::new("AXApplication").with_child(
        FakeNode::new("AXWindow")
            .with_label("Conductor")
            .with_child(pane),
    );
    let mut driver = Driver::new(FakeDesktop::new(app));
    let mut target = target();
    target.workspace_name = None;
    assert_eq!(
        driver.send_prompt(&target, PROMPT, false),
        Err(not_focused())
    );
    assert!(!driver.desktop().events().iter().any(|event| matches!(
        event,
        FakeEvent::Key { .. } | FakeEvent::SetValue { .. } | FakeEvent::SetFocused { .. }
    )));
}

#[test]
fn send_clears_a_composer_that_ignores_the_write() {
    let mut setup = Setup::working(&spec());
    setup.area.ignore_value_writes();
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::ComposerRejected)
    );
    let actions = setup.actions();
    assert_eq!(
        &actions[actions.len() - 2..],
        &[set_value(PROMPT), set_value("")]
    );
    assert!(setup.keys().is_empty());
}

#[test]
fn send_without_a_composer_is_no_composer() {
    let mut setup = Setup::working(&spec());
    setup
        .app
        .find_label("composer")
        .expect("composer group")
        .set_label("something else");
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::NoComposer)
    );
    setup.assert_untouched_composer();
}

#[test]
fn send_reports_still_in_composer_after_two_ignored_returns() {
    let mut setup = Setup::new(&spec());
    setup.land_on_open_url();
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::StillInComposer)
    );
    assert_eq!(
        setup.keys(),
        vec![(Key::Return, plain()), (Key::Return, plain())]
    );
    let waits = setup
        .events()
        .iter()
        .filter(|event| **event == FakeEvent::Pause(Duration::from_millis(200)))
        .count();
    assert_eq!(waits, 8);
}

#[test]
fn send_counts_the_second_return() {
    let mut setup = Setup::new(&spec());
    setup.land_on_open_url();
    let area = setup.area.clone();
    let returns = Rc::new(Cell::new(0));
    let seen = Rc::clone(&returns);
    setup.desktop().on_key(move |key, _| {
        if key == Key::Return {
            seen.set(seen.get() + 1);
            if seen.get() == 2 {
                area.set_value_text(None);
            }
        }
    });
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(2));
    assert_eq!(returns.get(), 2);
}

#[test]
fn send_counts_a_vanished_composer_as_sent() {
    let mut setup = Setup::new(&spec());
    setup.land_on_open_url();
    let area = setup.area.clone();
    setup.desktop().on_key(move |key, _| {
        if key == Key::Return {
            area.fail_value(AxError::InvalidUiElement);
        }
    });
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(1));
    assert_eq!(setup.keys().len(), 1);
}

#[test]
fn send_compares_line_endings_loosely() {
    let mut setup = Setup::working(&spec());
    assert_eq!(
        setup
            .driver
            .send_prompt(&target(), "one\r\ntwo\rthree", false),
        Ok(1)
    );
}

#[test]
fn send_accepts_doubled_ax_paragraph_breaks_without_changing_the_prompt() {
    for prompt in ["one\ntwo\nthree", "one\n\ntwo", "один\nдва"] {
        let mut setup = Setup::working(&spec());
        setup.area.on_set_value(|area, text| {
            area.set_value_text(Some(&text.replace('\n', "\n\n")));
        });
        assert_eq!(setup.driver.send_prompt(&target(), prompt, false), Ok(1));
        assert_eq!(setup.keys(), vec![(Key::Return, plain())]);
        let writes: Vec<_> = setup
            .actions()
            .into_iter()
            .filter(|event| matches!(event, FakeEvent::SetValue { .. }))
            .collect();
        assert_eq!(writes, vec![set_value(prompt)]);
    }
}

#[test]
fn send_does_not_count_doubled_ax_paragraph_breaks_as_submission() {
    let mut setup = Setup::new(&spec());
    setup.land_on_open_url();
    setup.area.on_set_value(|area, text| {
        area.set_value_text(Some(&text.replace('\n', "\n\n")));
    });
    assert_eq!(
        setup.driver.send_prompt(&target(), "one\ntwo", false),
        Err(UiError::StillInComposer)
    );
    assert_eq!(setup.keys(), vec![(Key::Return, plain()); 2]);
}

#[test]
fn send_rejects_changed_text_despite_doubled_ax_paragraph_breaks() {
    let mut setup = Setup::working(&spec());
    setup.area.on_set_value(|area, text| {
        if !text.is_empty() {
            area.set_value_text(Some("one\n\nwrong"));
        }
    });
    assert_eq!(
        setup.driver.send_prompt(&target(), "one\ntwo", false),
        Err(UiError::ComposerRejected)
    );
    assert!(setup.keys().is_empty());
    assert_eq!(setup.area.value_text(), Some(String::new()));
}

// ---- failures before the window ----

#[test]
fn untrusted_is_not_trusted() {
    let mut setup = Setup::working(&spec());
    setup.desktop().set_trusted(false);
    assert!(!setup.driver.trusted());
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::NotTrusted)
    );
    assert!(setup.events().is_empty());
}

#[test]
fn locked_session_is_locked_with_no_event() {
    let mut setup = Setup::working(&spec());
    setup.desktop().set_session(Some(SessionState {
        locked: true,
        on_console: true,
    }));
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::Locked)
    );
    assert_eq!(setup.driver.stop_turn(&target()), Err(UiError::Locked));
    assert_eq!(setup.driver.locate(), Err(UiError::Locked));
    assert!(setup.events().is_empty());
}

#[test]
fn no_pid_is_not_running() {
    let mut setup = Setup::working(&spec());
    setup.desktop().set_conductor_pid(None);
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::NotRunning)
    );
    assert!(setup.events().is_empty());
}

#[test]
fn window_read_cannot_complete_is_not_responding() {
    let mut setup = Setup::working(&spec());
    setup.app.fail_children(AxError::CannotComplete);
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::NotResponding)
    );
    assert!(setup.events().is_empty());
}

#[test]
fn window_read_api_disabled_is_not_trusted() {
    let mut setup = Setup::working(&spec());
    setup.app.fail_children(AxError::ApiDisabled);
    assert_eq!(setup.driver.locate(), Err(UiError::NotTrusted));
}

#[test]
fn no_window_is_no_window() {
    let mut driver = Driver::new(FakeDesktop::new(FakeNode::new("AXApplication")));
    assert_eq!(
        driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::NoWindow)
    );
    assert_eq!(driver.locate(), Err(UiError::NoWindow));
    assert!(driver.desktop().events().is_empty());
}

#[test]
fn empty_branch_is_no_branch() {
    let mut setup = Setup::working(&spec());
    // Checked before anything else, even trust.
    setup.desktop().set_trusted(false);
    let mut target = target();
    target.branch = String::new();
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(UiError::NoBranch)
    );
    assert_eq!(setup.driver.stop_turn(&target), Err(UiError::NoBranch));
    assert_eq!(setup.driver.new_chat(&target), Err(UiError::NoBranch));
    assert!(setup.events().is_empty());
}

// ---- front and keys ----

#[test]
fn refused_activation_is_not_frontmost() {
    let mut setup = Setup::working(&spec());
    setup.desktop().set_frontmost(Some(1));
    setup.desktop().refuse_activation();
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::NotFrontmost)
    );
    assert!(setup.events().contains(&FakeEvent::Activate(PID)));
    setup.assert_untouched_composer();
}

#[test]
fn activation_brings_conductor_forward() {
    let mut setup = Setup::working(&spec());
    setup.desktop().set_frontmost(Some(1));
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(1));
    let actions = setup.actions();
    let activate = actions
        .iter()
        .position(|event| *event == FakeEvent::Activate(PID))
        .expect("activated");
    let write = actions
        .iter()
        .position(|event| *event == set_value(PROMPT))
        .expect("written");
    assert!(activate < write);
}

#[test]
fn key_failure_is_a_key_error() {
    let mut setup = Setup::working(&spec());
    setup.desktop().fail_keys("no event");
    assert_eq!(
        setup.driver.send_prompt(&target(), PROMPT, false),
        Err(UiError::Key("no event".to_owned()))
    );
    assert_eq!(
        setup.driver.stop_turn(&target()),
        Err(UiError::Key("no event".to_owned()))
    );
}

// ---- tabs ----

#[test]
fn duplicate_title_is_several_tabs() {
    let mut setup = Setup::working(&WindowSpec {
        chats: vec!["One".to_owned(), "Same".to_owned(), "Same".to_owned()],
        ..spec()
    });
    let mut target = target();
    target.tab = tab(1, 3, Some("Same"));
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(UiError::SeveralTabs("Same".to_owned()))
    );
    assert!(setup.presses().is_empty());
    setup.assert_untouched_composer();
}

#[test]
fn missing_title_is_tab_not_found() {
    let mut setup = Setup::working(&spec());
    let mut target = target();
    target.tab = tab(5, 5, Some("Nope"));
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(UiError::TabNotFound(5))
    );
    target.tab = tab(4, 4, None);
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(UiError::TabNotFound(4))
    );
    assert!(setup.presses().is_empty());
    setup.assert_untouched_composer();
}

#[test]
fn press_that_does_not_select_is_tab_not_selected() {
    let mut setup = Setup::working(&spec());
    setup
        .radio("Three")
        .on_press(|radio| radio.set_selected(false));
    let mut target = target();
    target.tab = tab(3, 3, Some("Three"));
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(UiError::TabNotSelected)
    );
    assert_eq!(setup.presses(), vec![Some("Close chat Three".to_owned())]);
    assert!(setup
        .events()
        .contains(&FakeEvent::Pause(Duration::from_millis(500))));
    setup.assert_untouched_composer();
}

#[test]
fn no_tab_presses_no_radio() {
    let mut setup = Setup::working(&spec());
    let mut target = target();
    target.tab = None;
    assert_eq!(setup.driver.send_prompt(&target, PROMPT, false), Ok(1));
    assert!(setup.presses().is_empty());
    assert!(setup.radio("Two").is_selected());
}

#[test]
fn two_chats_without_a_strip_is_no_chat_strip() {
    let mut setup = Setup::working(&WindowSpec {
        chats: Vec::new(),
        ..spec()
    });
    let mut target = target();
    target.tab = tab(2, 2, None);
    assert_eq!(
        setup.driver.send_prompt(&target, PROMPT, false),
        Err(UiError::NoChatStrip)
    );
    setup.assert_untouched_composer();

    // A workspace with a single chat may show no strip.
    target.tab = tab(1, 1, None);
    assert_eq!(setup.driver.send_prompt(&target, PROMPT, false), Ok(1));
}

// ---- stop and new chat ----

#[test]
fn stop_selects_the_tab_then_posts_cmd_l_and_cmd_shift_delete() {
    let mut setup = Setup::working(&spec());
    let mut target = target();
    target.tab = tab(3, 3, Some("Three"));
    assert_eq!(setup.driver.stop_turn(&target), Ok(()));
    let actions = setup.actions();
    assert_eq!(
        actions,
        vec![
            FakeEvent::OpenUrl(LINK.to_owned()),
            FakeEvent::Press(Some("Close chat Three".to_owned())),
            FakeEvent::Key {
                pid: PID,
                key: Key::L,
                modifiers: command(),
            },
            FakeEvent::Key {
                pid: PID,
                key: Key::Delete,
                modifiers: Modifiers {
                    command: true,
                    shift: true,
                    ..Modifiers::default()
                },
            },
        ]
    );
    let events = setup.events();
    assert_eq!(
        &events[events.len() - 4..],
        &[
            FakeEvent::Key {
                pid: PID,
                key: Key::L,
                modifiers: command(),
            },
            FakeEvent::Pause(Duration::from_millis(200)),
            actions[3].clone(),
            FakeEvent::Pause(Duration::from_millis(300)),
        ]
    );
    assert!(setup.radio("Three").is_selected());
    assert_eq!(setup.area.value_text(), Some("draft".to_owned()));
}

#[test]
fn new_chat_opens_the_link_without_session_and_posts_cmd_l_and_cmd_t() {
    let mut setup = Setup::working(&spec());
    let mut target = target();
    target.session_id = None;
    target.tab = None;
    assert_eq!(setup.driver.new_chat(&target), Ok(()));
    assert_eq!(
        setup.actions(),
        vec![
            FakeEvent::OpenUrl("conductor://workspace?id=ws-1".to_owned()),
            FakeEvent::Key {
                pid: PID,
                key: Key::L,
                modifiers: command(),
            },
            FakeEvent::Key {
                pid: PID,
                key: Key::T,
                modifiers: command(),
            },
        ]
    );
}

#[test]
fn stop_and_new_chat_post_no_key_when_the_workspace_never_shows() {
    let mut setup = Setup::new(&spec());
    let mut target = target();
    target.workspace_name = None;
    assert_eq!(setup.driver.stop_turn(&target), Err(not_focused()));
    target.session_id = None;
    assert_eq!(setup.driver.new_chat(&target), Err(not_focused()));
    assert!(setup.keys().is_empty());
    assert!(setup.presses().is_empty());
}

// ---- locate ----

#[test]
fn locate_reports_the_view_without_touching_it() {
    let mut setup = Setup::new(&WindowSpec {
        branch: "user/feature-x".to_owned(),
        ..spec()
    });
    assert_eq!(
        setup.driver.locate(),
        Ok(ViewReport {
            pane_header: Some(TARGET_HEADER.to_owned()),
            chat_tabs: 3,
            selected_tab: Some(2),
            composer: true,
        })
    );
    assert!(!setup.events().iter().any(|event| matches!(
        event,
        FakeEvent::Press(_)
            | FakeEvent::SetValue { .. }
            | FakeEvent::SetFocused { .. }
            | FakeEvent::Key { .. }
    )));
    assert!(setup.events().is_empty());
}

#[test]
fn locate_without_a_pane_reports_nothing() {
    let app = FakeNode::new("AXApplication").with_child(
        FakeNode::new("AXWindow")
            .with_label("Conductor")
            .with_child(FakeNode::new("AXGroup")),
    );
    let mut driver = Driver::new(FakeDesktop::new(app));
    assert_eq!(driver.locate(), Ok(ViewReport::default()));
    assert!(driver.desktop().events().is_empty());
}

// ---- window choice ----

fn dialog() -> FakeNode {
    FakeNode::new("AXWindow")
        .with_subrole("AXDialog")
        .with_child(FakeNode::new("AXGroup"))
}

/// A `Setup` whose app lists `before` ahead of the main window, which is a standard window.
fn setup_behind(before: Vec<FakeNode>) -> Setup {
    let setup = Setup::working(&spec());
    let window = setup
        .app
        .find_role("AXWindow")
        .expect("window")
        .with_subrole("AXStandardWindow");
    let app = before
        .into_iter()
        .fold(FakeNode::new("AXApplication"), FakeNode::with_child)
        .with_child(window);
    Setup {
        driver: Driver::new(FakeDesktop::new(app.clone())),
        app,
        area: setup.area.clone(),
    }
    .with_reactions()
}

impl Setup {
    fn with_reactions(self) -> Setup {
        self.land_on_open_url();
        self.return_empties_composer();
        self
    }
}

#[test]
fn a_dialog_listed_before_the_main_window_is_skipped() {
    let mut setup = setup_behind(vec![dialog()]);
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(1));

    let mut setup = setup_behind(vec![dialog()]);
    land(&setup.app);
    assert_eq!(
        setup.driver.locate(),
        Ok(ViewReport {
            pane_header: Some(TARGET_HEADER.to_owned()),
            chat_tabs: 3,
            selected_tab: Some(2),
            composer: true,
        })
    );
}

#[test]
fn a_send_on_the_dialog_first_app_succeeds_and_locate_still_reports_the_pane() {
    let mut setup = setup_behind(vec![dialog()]);
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(1));
    assert_eq!(
        setup.driver.locate(),
        Ok(ViewReport {
            pane_header: Some(TARGET_HEADER.to_owned()),
            chat_tabs: 3,
            selected_tab: Some(2),
            composer: true,
        })
    );
}

#[test]
fn the_standard_window_with_the_pane_wins_over_one_without() {
    let bare = FakeNode::new("AXWindow")
        .with_subrole("AXStandardWindow")
        .with_child(FakeNode::new("AXGroup"));
    let mut setup = setup_behind(vec![bare.clone()]);
    assert_eq!(setup.driver.send_prompt(&target(), PROMPT, false), Ok(1));

    let mut setup = setup_behind(vec![dialog(), bare]);
    land(&setup.app);
    assert_eq!(
        setup.driver.locate().map(|report| report.composer),
        Ok(true)
    );
}

#[test]
fn only_a_dialog_window_is_not_focused_and_locates_nothing() {
    let app = FakeNode::new("AXApplication").with_child(dialog());
    let mut driver = Driver::new(FakeDesktop::new(app));
    assert_eq!(
        driver.send_prompt(&target(), PROMPT, false),
        Err(not_focused())
    );
    assert_eq!(driver.locate(), Ok(ViewReport::default()));
}

// ---- open_link ----

const CREATE_LINK: &str = "conductor://prompt=hello%20there&path=%2Ftmp%2Frepo";

#[test]
fn open_link_opens_exactly_the_given_url() {
    let mut setup = Setup::new(&spec());
    assert_eq!(setup.driver.open_link(CREATE_LINK), Ok(()));
    assert_eq!(setup.events(), [FakeEvent::OpenUrl(CREATE_LINK.to_owned())]);
}

#[test]
fn open_link_refuses_when_locked_without_opening_anything() {
    let mut setup = Setup::new(&spec());
    setup.desktop().set_session(Some(SessionState {
        locked: true,
        on_console: true,
    }));
    assert_eq!(setup.driver.open_link(CREATE_LINK), Err(UiError::Locked));
    assert!(setup.events().is_empty());
}

#[test]
fn open_link_refuses_when_untrusted_without_opening_anything() {
    let mut setup = Setup::new(&spec());
    setup.desktop().set_trusted(false);
    assert_eq!(
        setup.driver.open_link(CREATE_LINK),
        Err(UiError::NotTrusted)
    );
    assert!(setup.events().is_empty());
}

#[test]
fn open_link_refuses_when_conductor_is_not_running_without_opening_anything() {
    let mut setup = Setup::new(&spec());
    setup.desktop().set_conductor_pid(None);
    assert_eq!(
        setup.driver.open_link(CREATE_LINK),
        Err(UiError::NotRunning)
    );
    assert!(setup.events().is_empty());
}

#[test]
fn open_link_checks_trust_before_the_lock_and_the_lock_before_the_pid() {
    let mut setup = Setup::new(&spec());
    setup.desktop().set_trusted(false);
    setup.desktop().set_session(Some(SessionState {
        locked: true,
        on_console: true,
    }));
    setup.desktop().set_conductor_pid(None);
    assert_eq!(
        setup.driver.open_link(CREATE_LINK),
        Err(UiError::NotTrusted)
    );
    setup.desktop().set_trusted(true);
    assert_eq!(setup.driver.open_link(CREATE_LINK), Err(UiError::Locked));
    assert!(setup.events().is_empty());
}
