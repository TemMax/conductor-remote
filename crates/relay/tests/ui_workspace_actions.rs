//! The workspace controls over the fake desktop: closing a chat, archiving, Continue, the status
//! of a sidebar row, and the New workspace dialog. Nothing here reaches the Mac.

use std::time::Duration;

use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::driver::{create_link, Tab, Target, UiDriver, UiError};
use conductor_remote::ui::fake::{
    add_workspace_ui, conductor_app, main_pane, FakeDesktop, FakeEvent, FakeNode, WindowSpec,
    WorkspaceUi, WorkspaceUiSpec,
};
use conductor_remote::ui::keys::{Key, Modifiers};
use conductor_remote::ui::screen::SessionState;

const PID: i32 = 4242;
const LINK: &str = "conductor://workspace?id=ws-1&session=s-2";
const CLOSE_ALERT: &str = "Close running chat?";
const CLOSE_ANYWAY: &str = "Close anyway ⌘ Enter";
const ARCHIVE_ANYWAY: &str = "Stop agents and archive ⌘ Enter";
const STATUSES: [&str; 5] = ["Backlog", "In progress", "In review", "Done", "Canceled"];

/// A window already showing the target's workspace, its second chat selected.
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

/// Chat `index` (1-based) of workspace "beta", titled `title`.
fn target_tab(index: usize, title: &str) -> Target {
    Target {
        workspace_id: "ws-1".to_owned(),
        session_id: Some("s-2".to_owned()),
        repo: Some("relay".to_owned()),
        branch: "user/feature-x".to_owned(),
        workspace_name: Some("beta".to_owned()),
        tab: Some(Tab {
            index,
            count: 3,
            title: Some(title.to_owned()),
        }),
    }
}

/// The second chat, the one `window()` has selected.
fn target() -> Target {
    target_tab(2, "Two")
}

fn command() -> Modifiers {
    Modifiers {
        command: true,
        ..Modifiers::default()
    }
}

fn command_shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..command()
    }
}

fn key(key: Key, modifiers: Modifiers) -> FakeEvent {
    FakeEvent::Key {
        pid: PID,
        key,
        modifiers,
    }
}

fn press(label: &str) -> FakeEvent {
    FakeEvent::Press(Some(label.to_owned()))
}

fn pause(millis: u64) -> FakeEvent {
    FakeEvent::Pause(Duration::from_millis(millis))
}

/// `events` followed by `count` pauses of `millis`.
fn then_pauses(mut events: Vec<FakeEvent>, count: usize, millis: u64) -> Vec<FakeEvent> {
    events.extend(std::iter::repeat_n(pause(millis), count));
    events
}

struct Setup {
    driver: Driver<FakeDesktop>,
    app: FakeNode,
    ui: WorkspaceUi,
}

impl Setup {
    fn new(running: &[&str], continue_button: bool) -> Setup {
        Setup::over(&window(), running, continue_button)
    }

    fn over(window: &WindowSpec, running: &[&str], continue_button: bool) -> Setup {
        let app = conductor_app(window);
        let desktop = FakeDesktop::new(app.clone());
        let ui = add_workspace_ui(
            &desktop,
            &WorkspaceUiSpec {
                running: running.iter().map(|title| (*title).to_owned()).collect(),
                continue_button,
            },
        );
        Setup {
            driver: Driver::new(desktop),
            app,
            ui,
        }
    }

    fn events(&self) -> Vec<FakeEvent> {
        self.driver.desktop().events()
    }

    fn presses(&self) -> Vec<String> {
        presses(self.driver.desktop())
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

    /// The titles of the chats still in the strip, in order.
    fn chats(&self) -> Vec<String> {
        let strip = main_pane(&self.app)
            .find_role("AXTabGroup")
            .expect("a chat tab group")
            .child_nodes()
            .remove(0);
        strip
            .child_nodes()
            .iter()
            .filter_map(FakeNode::label_text)
            .filter_map(|label| label.strip_prefix("Close chat ").map(str::to_owned))
            .collect()
    }
}

/// The labels of the pressed nodes, in order.
fn presses(desktop: &FakeDesktop) -> Vec<String> {
    desktop
        .events()
        .into_iter()
        .filter_map(|event| match event {
            FakeEvent::Press(label) => Some(label.unwrap_or_default()),
            _ => None,
        })
        .collect()
}

/// A driver over a window with no workspace controls, and its app.
fn bare() -> (Driver<FakeDesktop>, FakeNode) {
    let app = conductor_app(&window());
    (Driver::new(FakeDesktop::new(app.clone())), app)
}

/// A driver over a window whose Cmd+W shows a "Close running chat?" alert with these buttons,
/// none of which does anything.
fn with_inert_alert(buttons: &[&str]) -> Driver<FakeDesktop> {
    let (driver, app) = bare();
    let web_area = app.find_role("AXWebArea").expect("a web area");
    let labels: Vec<String> = buttons.iter().map(|label| (*label).to_owned()).collect();
    driver.desktop().on_key(move |key, _| {
        if key == Key::W {
            let alert = labels.iter().fold(
                FakeNode::new("AXGroup")
                    .with_subrole("AXApplicationAlertDialog")
                    .with_label(CLOSE_ALERT),
                |alert, label| alert.with_child(FakeNode::new("AXButton").with_label(label)),
            );
            web_area.add_child(FakeNode::new("AXGroup").with_child(alert));
        }
    });
    driver
}

/// What a close of the already selected chat records up to and including Cmd+W.
fn close_keys() -> Vec<FakeEvent> {
    vec![
        FakeEvent::OpenUrl(LINK.to_owned()),
        key(Key::L, command()),
        pause(200),
        key(Key::W, command()),
    ]
}

// ---- close_chat ----

#[test]
fn closing_an_idle_chat_closes_the_selected_target_tab_and_nothing_else() {
    let mut setup = Setup::new(&[], false);

    assert_eq!(
        setup.driver.close_chat(&target_tab(3, "Three"), false),
        Ok(())
    );

    assert_eq!(setup.ui.closed(), ["Three"]);
    assert_eq!(setup.chats(), ["One", "Two"]);
    assert!(!setup.ui.archived());
    assert!(!setup.ui.dialog_open());
    // The tab is selected first; no alert shows in five looks.
    assert_eq!(
        setup.events(),
        then_pauses(
            vec![
                FakeEvent::OpenUrl(LINK.to_owned()),
                press("Close chat Three"),
                pause(500),
                key(Key::L, command()),
                pause(200),
                key(Key::W, command()),
            ],
            4,
            200
        )
    );
}

#[test]
fn closing_a_running_chat_without_confirm_needs_confirmation() {
    let mut setup = Setup::new(&["Two"], false);

    assert_eq!(
        setup.driver.close_chat(&target(), false),
        Err(UiError::NeedsConfirmation)
    );

    assert!(setup.ui.closed().is_empty());
    assert_eq!(setup.chats(), ["One", "Two", "Three"]);
    assert!(!setup.ui.dialog_open());
    let mut expected = close_keys();
    expected.push(press("Cancel"));
    assert_eq!(setup.events(), expected);
}

#[test]
fn closing_a_running_chat_with_confirm_closes_it() {
    let mut setup = Setup::new(&["Two"], false);

    assert_eq!(setup.driver.close_chat(&target(), true), Ok(()));

    assert_eq!(setup.ui.closed(), ["Two"]);
    assert_eq!(setup.chats(), ["One", "Three"]);
    assert!(!setup.ui.dialog_open());
    let mut expected = close_keys();
    expected.push(press(CLOSE_ANYWAY));
    assert_eq!(setup.events(), expected);
}

#[test]
fn an_idle_chat_is_closed_whatever_confirm_says() {
    let mut setup = Setup::new(&["One"], false);

    assert_eq!(setup.driver.close_chat(&target(), true), Ok(()));

    assert_eq!(setup.ui.closed(), ["Two"]);
    assert!(setup.presses().is_empty());
}

#[test]
fn an_alert_without_the_wanted_button_is_stuck() {
    for (buttons, confirm) in [(["Cancel"], true), ([CLOSE_ANYWAY], false)] {
        let mut driver = with_inert_alert(&buttons);

        assert_eq!(
            driver.close_chat(&target(), confirm),
            Err(UiError::DialogStuck)
        );

        assert_eq!(driver.desktop().events(), close_keys());
    }
}

#[test]
fn an_alert_that_stays_after_its_button_is_stuck() {
    for (confirm, pressed) in [(true, CLOSE_ANYWAY), (false, "Cancel")] {
        let mut driver = with_inert_alert(&["Cancel", CLOSE_ANYWAY]);

        assert_eq!(
            driver.close_chat(&target(), confirm),
            Err(UiError::DialogStuck)
        );

        // Ten looks for the alert to go: nine pauses.
        let mut expected = close_keys();
        expected.push(press(pressed));
        assert_eq!(driver.desktop().events(), then_pauses(expected, 9, 150));
    }
}

// ---- archive ----

#[test]
fn archiving_an_idle_workspace_archives_it() {
    let mut setup = Setup::new(&[], false);

    // The chat tab is not selected: the workspace is archived, not a chat.
    assert_eq!(setup.driver.archive(&target_tab(3, "Three"), false), Ok(()));

    assert!(setup.ui.archived());
    assert!(setup.ui.closed().is_empty());
    assert!(!setup.ui.dialog_open());
    assert_eq!(
        setup.events(),
        then_pauses(
            vec![
                FakeEvent::OpenUrl(LINK.to_owned()),
                key(Key::L, command()),
                pause(200),
                key(Key::A, command_shift()),
            ],
            4,
            200
        )
    );
}

#[test]
fn archiving_with_a_running_chat_without_confirm_needs_confirmation() {
    let mut setup = Setup::new(&["One"], false);

    assert_eq!(
        setup.driver.archive(&target(), false),
        Err(UiError::NeedsConfirmation)
    );

    assert!(!setup.ui.archived());
    assert!(!setup.ui.dialog_open());
    assert_eq!(setup.presses(), ["Cancel"]);
    assert_eq!(
        setup.keys(),
        [(Key::L, command()), (Key::A, command_shift())]
    );
}

#[test]
fn archiving_with_a_running_chat_with_confirm_archives_it() {
    let mut setup = Setup::new(&["One"], false);

    assert_eq!(setup.driver.archive(&target(), true), Ok(()));

    assert!(setup.ui.archived());
    assert!(!setup.ui.dialog_open());
    assert_eq!(setup.presses(), [ARCHIVE_ANYWAY]);
    assert_eq!(
        setup.keys(),
        [(Key::L, command()), (Key::A, command_shift())]
    );
}

// ---- press_continue ----

#[test]
fn continue_is_pressed() {
    let mut setup = Setup::new(&[], true);

    assert_eq!(setup.driver.press_continue(&target()), Ok(()));

    assert!(setup.ui.continued());
    assert_eq!(
        setup.events(),
        [FakeEvent::OpenUrl(LINK.to_owned()), press("Continue")]
    );
}

#[test]
fn continue_is_brought_to_the_front_before_it_is_pressed() {
    let mut setup = Setup::new(&[], true);
    setup.driver.desktop().set_frontmost(Some(1));

    assert_eq!(setup.driver.press_continue(&target()), Ok(()));

    assert_eq!(
        setup.events(),
        [
            FakeEvent::OpenUrl(LINK.to_owned()),
            FakeEvent::Activate(PID),
            press("Continue")
        ]
    );
}

#[test]
fn no_continue_button_is_no_continue() {
    let mut setup = Setup::new(&[], false);

    assert_eq!(
        setup.driver.press_continue(&target()),
        Err(UiError::NoContinue)
    );

    assert!(!setup.ui.continued());
    assert_eq!(setup.events(), [FakeEvent::OpenUrl(LINK.to_owned())]);
}

#[test]
fn only_a_direct_button_labelled_exactly_continue_counts() {
    let mut setup = Setup::new(&[], false);
    let pane = main_pane(&setup.app);
    pane.add_child(FakeNode::new("AXButton").with_label("Continue in a new chat"));
    pane.add_child(FakeNode::new("AXLink").with_label("Continue"));
    pane.add_child(
        FakeNode::new("AXGroup").with_child(FakeNode::new("AXButton").with_label("Continue")),
    );

    assert_eq!(
        setup.driver.press_continue(&target()),
        Err(UiError::NoContinue)
    );

    assert!(setup.presses().is_empty());
}

// ---- set_status ----

#[test]
fn each_status_is_set_on_the_named_row() {
    for label in STATUSES {
        let mut setup = Setup::new(&[], false);

        assert_eq!(setup.driver.set_status(&target(), "alpha", label), Ok(()));

        assert_eq!(
            setup.ui.status(),
            Some(("alpha".to_owned(), label.to_owned()))
        );
        assert!(!setup.ui.menu_open());
        assert_eq!(
            setup.events(),
            [
                FakeEvent::OpenUrl(LINK.to_owned()),
                FakeEvent::ShowMenu(Some("alpha".to_owned())),
                press("Set status"),
                press(label),
            ]
        );
    }
}

#[test]
fn an_unknown_row_is_no_sidebar_row() {
    let mut setup = Setup::new(&[], false);

    assert_eq!(
        setup.driver.set_status(&target(), "gamma", "Done"),
        Err(UiError::NoSidebarRow("gamma".to_owned()))
    );

    assert_eq!(setup.ui.status(), None);
    assert_eq!(setup.events(), [FakeEvent::OpenUrl(LINK.to_owned())]);
}

#[test]
fn two_rows_with_the_same_title_are_no_sidebar_row() {
    let mut window = window();
    window.sidebar.push("beta".to_owned());
    let mut setup = Setup::over(&window, &[], false);

    assert_eq!(
        setup.driver.set_status(&target(), "beta", "Done"),
        Err(UiError::NoSidebarRow("beta".to_owned()))
    );

    assert_eq!(setup.ui.status(), None);
    assert_eq!(setup.events(), [FakeEvent::OpenUrl(LINK.to_owned())]);
}

#[test]
fn an_unknown_label_is_no_status_and_the_menus_are_closed() {
    let mut setup = Setup::new(&[], false);

    assert_eq!(
        setup.driver.set_status(&target(), "beta", "Blocked"),
        Err(UiError::NoStatus("Blocked".to_owned()))
    );

    assert_eq!(setup.ui.status(), None);
    assert!(!setup.ui.menu_open());
    assert_eq!(
        setup.events(),
        [
            FakeEvent::OpenUrl(LINK.to_owned()),
            FakeEvent::ShowMenu(Some("beta".to_owned())),
            press("Set status"),
            key(Key::Escape, Modifiers::default()),
            key(Key::Escape, Modifiers::default()),
        ]
    );
}

#[test]
fn a_row_menu_that_never_opens_is_no_status_menu() {
    let (mut driver, _) = bare();

    assert_eq!(
        driver.set_status(&target(), "beta", "Done"),
        Err(UiError::NoStatusMenu)
    );

    // Ten looks for the row menu: nine pauses.
    assert_eq!(
        driver.desktop().events(),
        then_pauses(
            vec![
                FakeEvent::OpenUrl(LINK.to_owned()),
                FakeEvent::ShowMenu(Some("beta".to_owned())),
            ],
            9,
            150
        )
    );
}

#[test]
fn a_status_menu_that_never_opens_is_no_status_menu() {
    let (mut driver, app) = bare();
    let web_area = app.find_role("AXWebArea").expect("a web area");
    app.find_label("beta")
        .expect("the row")
        .on_show_menu(move |_| {
            let menu = FakeNode::new("AXMenu")
                .with_child(FakeNode::new("AXMenuItem").with_label("Set status"));
            web_area.add_child(FakeNode::new("AXGroup").with_child(menu));
        });

    assert_eq!(
        driver.set_status(&target(), "beta", "Done"),
        Err(UiError::NoStatusMenu)
    );

    assert_eq!(
        driver.desktop().events(),
        then_pauses(
            vec![
                FakeEvent::OpenUrl(LINK.to_owned()),
                FakeEvent::ShowMenu(Some("beta".to_owned())),
                press("Set status"),
            ],
            9,
            150
        )
    );
}

// ---- confirm_create ----

#[test]
fn confirm_create_presses_create_in_the_dialog_a_create_link_opened() {
    let mut setup = Setup::new(&[], false);
    let link = create_link(Some("fix the tests"), None);
    assert_eq!(setup.driver.open_link(&link), Ok(()));
    assert!(setup.ui.dialog_open());

    assert_eq!(setup.driver.confirm_create(), Ok(()));

    assert_eq!(setup.ui.created(), 1);
    assert!(!setup.ui.dialog_open());
    assert_eq!(setup.events(), [FakeEvent::OpenUrl(link), press("Create")]);
}

#[test]
fn without_a_dialog_confirm_create_gives_up_after_twenty_looks() {
    let mut setup = Setup::new(&[], false);

    assert_eq!(setup.driver.confirm_create(), Err(UiError::NoCreateDialog));

    assert_eq!(setup.ui.created(), 0);
    // Twenty looks: nineteen pauses, and nothing else.
    assert_eq!(setup.events(), then_pauses(Vec::new(), 19, 250));
}

#[test]
fn a_window_that_cannot_be_read_counts_as_no_dialog() {
    let mut driver = Driver::new(FakeDesktop::new(FakeNode::new("AXApplication")));

    assert_eq!(driver.confirm_create(), Err(UiError::NoCreateDialog));

    assert_eq!(driver.desktop().events(), then_pauses(Vec::new(), 19, 250));
}

#[test]
fn a_dialog_without_a_create_button_is_no_create_dialog() {
    let (mut driver, app) = bare();
    let dialog = FakeNode::new("AXGroup")
        .with_subrole("AXApplicationDialog")
        .with_label("New workspace")
        .with_child(FakeNode::new("AXButton").with_label("Create and open"));
    app.find_role("AXWebArea")
        .expect("a web area")
        .add_child(FakeNode::new("AXGroup").with_child(dialog));

    assert_eq!(driver.confirm_create(), Err(UiError::NoCreateDialog));

    assert!(driver.desktop().events().is_empty());
}

// ---- every command ----

#[test]
fn a_locked_mac_stops_every_command_before_anything_is_pressed_or_posted() {
    let mut setup = Setup::new(&["Two"], true);
    setup.driver.desktop().set_session(Some(SessionState {
        locked: true,
        on_console: true,
    }));
    let target = target();

    assert_eq!(setup.driver.close_chat(&target, true), Err(UiError::Locked));
    assert_eq!(
        setup.driver.set_status(&target, "beta", "Done"),
        Err(UiError::Locked)
    );
    assert_eq!(setup.driver.archive(&target, true), Err(UiError::Locked));
    assert_eq!(setup.driver.press_continue(&target), Err(UiError::Locked));
    assert_eq!(setup.driver.confirm_create(), Err(UiError::Locked));

    assert!(setup.events().is_empty());
    assert!(setup.ui.closed().is_empty());
    assert!(!setup.ui.archived());
    assert!(!setup.ui.continued());
    assert_eq!(setup.ui.status(), None);
}

#[test]
fn a_target_without_a_branch_is_refused_before_anything_happens() {
    let mut setup = Setup::new(&[], true);
    let target = Target {
        branch: String::new(),
        ..target()
    };

    assert_eq!(
        setup.driver.close_chat(&target, true),
        Err(UiError::NoBranch)
    );
    assert_eq!(
        setup.driver.set_status(&target, "beta", "Done"),
        Err(UiError::NoBranch)
    );
    assert_eq!(setup.driver.archive(&target, true), Err(UiError::NoBranch));
    assert_eq!(setup.driver.press_continue(&target), Err(UiError::NoBranch));

    assert!(setup.events().is_empty());
}
