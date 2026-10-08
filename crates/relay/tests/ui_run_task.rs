//! Starting and stopping a workspace's Run task over the fake desktop and the fake Run strip.
//! Nothing here reaches the Mac.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::driver::{RunOutcome, Target, UiDriver, UiError};
use conductor_remote::ui::fake::{
    add_run_strip, conductor_app, main_pane, FakeDesktop, FakeEvent, FakeNode, RunStrip,
    RunStripSpec, WindowSpec,
};
use conductor_remote::ui::screen::SessionState;

const PID: i32 = 4242;
const LINK: &str = "conductor://workspace?id=ws-1";
const SELECT_TASK: &str = "Select task";

/// A window already showing the target's workspace.
fn window() -> WindowSpec {
    WindowSpec {
        repo: "relay".to_owned(),
        branch: "user/feature-x".to_owned(),
        sidebar: vec!["alpha".to_owned(), "beta".to_owned()],
        chats: vec!["One".to_owned()],
        selected: 0,
        composer_value: None,
    }
}

/// Workspace "beta"; a Run task belongs to no chat.
fn target() -> Target {
    Target {
        workspace_id: "ws-1".to_owned(),
        session_id: None,
        repo: Some("relay".to_owned()),
        branch: "user/feature-x".to_owned(),
        workspace_name: Some("beta".to_owned()),
        tab: None,
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

/// The deep link, then these presses.
fn link_then(presses: &[&str]) -> Vec<FakeEvent> {
    let mut events = vec![FakeEvent::OpenUrl(LINK.to_owned())];
    events.extend(presses.iter().map(|label| press(label)));
    events
}

fn changed(task: &str) -> Result<RunOutcome, UiError> {
    Ok(RunOutcome {
        changed: true,
        task: Some(task.to_owned()),
    })
}

struct Setup {
    driver: Driver<FakeDesktop>,
    app: FakeNode,
    strip: RunStrip,
    /// Every start and stop the strip made, in order.
    seen: Rc<RefCell<Vec<String>>>,
}

impl Setup {
    /// The tasks "Web", "Api docs" and "Worker": `selected` is the one the Run button names,
    /// `running` the one that runs.
    fn new(selected: usize, running: Option<usize>) -> Setup {
        Setup::with_tasks(&["Web", "Api docs", "Worker"], selected, running)
    }

    fn with_tasks(tasks: &[&str], selected: usize, running: Option<usize>) -> Setup {
        let app = conductor_app(&window());
        let strip = add_run_strip(
            &app,
            &RunStripSpec {
                tasks: tasks.iter().map(|task| (*task).to_owned()).collect(),
                selected,
                running,
            },
        );
        let seen = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::clone(&seen);
        strip.on_start(move |name| log.borrow_mut().push(format!("start {name}")));
        let log = Rc::clone(&seen);
        strip.on_stop(move |name| log.borrow_mut().push(format!("stop {name}")));
        Setup {
            driver: Driver::new(FakeDesktop::new(app.clone())),
            app,
            strip,
            seen,
        }
    }

    fn events(&self) -> Vec<FakeEvent> {
        self.driver.desktop().events()
    }

    fn seen(&self) -> Vec<String> {
        self.seen.borrow().clone()
    }

    fn menu_open(&self) -> bool {
        self.app.find_role("AXMenu").is_some()
    }
}

/// A driver over a window with no Run strip, and its app.
fn bare() -> (Driver<FakeDesktop>, FakeNode) {
    let app = conductor_app(&window());
    (Driver::new(FakeDesktop::new(app.clone())), app)
}

// ---- start ----

#[test]
fn starting_with_no_name_presses_the_run_button_of_the_selected_task() {
    let mut setup = Setup::new(1, None);

    assert_eq!(
        setup.driver.run_task(&target(), None, true),
        changed("Api docs")
    );

    assert_eq!(setup.strip.running().as_deref(), Some("Api docs"));
    assert_eq!((setup.strip.starts(), setup.strip.stops()), (1, 0));
    // The Stop button shows on the first look, so nothing is waited for.
    assert_eq!(setup.events(), link_then(&["Run Api docs"]));
}

#[test]
fn starting_the_task_the_button_names_presses_the_button_and_opens_no_menu() {
    let mut setup = Setup::new(1, None);

    assert_eq!(
        setup.driver.run_task(&target(), Some("Api docs"), true),
        changed("Api docs")
    );

    assert_eq!(setup.seen(), ["start Api docs"]);
    assert_eq!(setup.events(), link_then(&["Run Api docs"]));
    assert!(!setup.menu_open());
}

#[test]
fn starting_another_task_goes_through_select_task() {
    let mut setup = Setup::new(0, None);

    assert_eq!(
        setup.driver.run_task(&target(), Some("Worker"), true),
        changed("Worker")
    );

    assert_eq!(setup.strip.running().as_deref(), Some("Worker"));
    assert_eq!(setup.seen(), ["start Worker"]);
    assert_eq!(setup.events(), link_then(&[SELECT_TASK, "Worker"]));
    assert!(!setup.menu_open());
}

#[test]
fn an_unknown_name_is_no_run_task_and_leaves_no_menu_open() {
    let mut setup = Setup::new(0, None);

    assert_eq!(
        setup.driver.run_task(&target(), Some("Storybook"), true),
        Err(UiError::NoRunTask("Storybook".to_owned()))
    );

    assert!(!setup.menu_open());
    assert_eq!(setup.strip.running(), None);
    assert!(setup.seen().is_empty());
    // The pop-up opens the menu, and is pressed once more to close it.
    assert_eq!(setup.events(), link_then(&[SELECT_TASK, SELECT_TASK]));
}

#[test]
fn a_name_is_matched_exactly_not_by_prefix() {
    let mut setup = Setup::new(0, None);

    assert_eq!(
        setup.driver.run_task(&target(), Some("Api"), true),
        Err(UiError::NoRunTask("Api".to_owned()))
    );

    assert!(setup.seen().is_empty());
    assert!(!setup.menu_open());
}

#[test]
fn an_unknown_name_with_one_task_presses_nothing() {
    let mut setup = Setup::with_tasks(&["Web"], 0, None);

    assert_eq!(
        setup.driver.run_task(&target(), Some("Worker"), true),
        Err(UiError::NoRunTask("Worker".to_owned()))
    );

    // One task shows no Select task pop-up.
    assert_eq!(setup.events(), link_then(&[]));
    assert_eq!(setup.strip.running(), None);
}

#[test]
fn a_select_task_menu_that_never_opens_is_menu_not_opened() {
    let (mut driver, app) = bare();
    main_pane(&app).add_child(
        FakeNode::new("AXTabGroup")
            .with_child(FakeNode::new("AXRadioButton").with_label("Run"))
            .with_child(FakeNode::new("AXButton").with_label("Run Web"))
            .with_child(FakeNode::new("AXPopUpButton").with_label(SELECT_TASK)),
    );

    assert_eq!(
        driver.run_task(&target(), Some("Worker"), true),
        Err(UiError::MenuNotOpened(SELECT_TASK.to_owned()))
    );

    // Ten looks, 150 ms between two of them.
    assert_eq!(
        driver.desktop().events(),
        then_pauses(link_then(&[SELECT_TASK]), 9, 150)
    );
}

#[test]
fn starting_the_task_that_already_runs_changes_nothing() {
    let mut setup = Setup::new(0, Some(1));
    let unchanged = Ok(RunOutcome {
        changed: false,
        task: Some("Api docs".to_owned()),
    });

    assert_eq!(
        setup.driver.run_task(&target(), Some("Api docs"), true),
        unchanged
    );
    // No name asks for whatever the strip would start: the task that runs.
    assert_eq!(setup.driver.run_task(&target(), None, true), unchanged);

    assert_eq!(setup.strip.running().as_deref(), Some("Api docs"));
    assert!(setup.seen().is_empty());
    assert_eq!(
        setup.events(),
        [
            FakeEvent::OpenUrl(LINK.to_owned()),
            FakeEvent::OpenUrl(LINK.to_owned())
        ]
    );
}

#[test]
fn starting_another_task_while_one_runs_stops_the_first_then_starts_the_other() {
    let mut setup = Setup::new(0, Some(0));

    assert_eq!(
        setup.driver.run_task(&target(), Some("Api docs"), true),
        changed("Api docs")
    );

    assert_eq!(setup.strip.running().as_deref(), Some("Api docs"));
    assert_eq!(setup.seen(), ["stop Web", "start Api docs"]);
    assert_eq!((setup.strip.starts(), setup.strip.stops()), (1, 1));
    assert_eq!(
        setup.events(),
        link_then(&["Stop Web", SELECT_TASK, "Api docs"])
    );
    assert!(!setup.menu_open());
}

#[test]
fn a_start_that_changes_nothing_is_run_not_changed() {
    let mut setup = Setup::new(0, None);
    setup.strip.ignore_presses();

    assert_eq!(
        setup.driver.run_task(&target(), None, true),
        Err(UiError::RunNotChanged)
    );

    assert_eq!(setup.strip.running(), None);
    assert_eq!((setup.strip.starts(), setup.strip.stops()), (0, 0));
    // Fifteen looks, 200 ms between two of them.
    assert_eq!(
        setup.events(),
        then_pauses(link_then(&["Run Web"]), 14, 200)
    );
}

#[test]
fn the_strip_is_read_afresh_after_the_press() {
    let (mut driver, app) = bare();
    let pane = main_pane(&app);
    let old = FakeNode::new("AXTabGroup");
    let run = FakeNode::new("AXButton").with_label("Run Web");
    old.add_child(run.clone());
    pane.add_child(old.clone());
    // Conductor re-renders the whole strip on a start: the tab group pressed in is gone.
    run.on_press(move |_| {
        pane.remove_child(&old);
        pane.add_child(
            FakeNode::new("AXTabGroup")
                .with_child(FakeNode::new("AXButton").with_label("Stop Web")),
        );
    });

    assert_eq!(driver.run_task(&target(), None, true), changed("Web"));
    assert_eq!(driver.desktop().events(), link_then(&["Run Web"]));
}

// ---- stop ----

#[test]
fn stopping_a_running_task_presses_its_stop_button() {
    let mut setup = Setup::new(0, Some(2));

    assert_eq!(
        setup.driver.run_task(&target(), None, false),
        changed("Worker")
    );

    assert_eq!(setup.strip.running(), None);
    assert_eq!(setup.seen(), ["stop Worker"]);
    assert_eq!(setup.events(), link_then(&["Stop Worker"]));
}

#[test]
fn stopping_ignores_the_name() {
    let mut setup = Setup::new(0, Some(2));

    assert_eq!(
        setup.driver.run_task(&target(), Some("Web"), false),
        changed("Worker")
    );

    assert_eq!(setup.seen(), ["stop Worker"]);
}

#[test]
fn stopping_when_nothing_runs_changes_nothing() {
    let mut setup = Setup::new(0, None);

    assert_eq!(
        setup.driver.run_task(&target(), None, false),
        Ok(RunOutcome {
            changed: false,
            task: None,
        })
    );

    assert!(setup.seen().is_empty());
    assert_eq!(setup.events(), link_then(&[]));
}

#[test]
fn a_stop_that_changes_nothing_is_run_not_changed() {
    let mut setup = Setup::new(0, Some(0));
    setup.strip.ignore_presses();

    assert_eq!(
        setup.driver.run_task(&target(), None, false),
        Err(UiError::RunNotChanged)
    );

    assert_eq!(setup.strip.running().as_deref(), Some("Web"));
    assert_eq!((setup.strip.starts(), setup.strip.stops()), (0, 0));
    assert_eq!(
        setup.events(),
        then_pauses(link_then(&["Stop Web"]), 14, 200)
    );
}

#[test]
fn a_stop_before_another_start_that_changes_nothing_starts_nothing() {
    let mut setup = Setup::new(0, Some(0));
    setup.strip.ignore_presses();

    assert_eq!(
        setup.driver.run_task(&target(), Some("Worker"), true),
        Err(UiError::RunNotChanged)
    );

    assert_eq!(setup.strip.running().as_deref(), Some("Web"));
    assert_eq!(
        setup.events(),
        then_pauses(link_then(&["Stop Web"]), 14, 200)
    );
}

// ---- the checks before any press ----

#[test]
fn a_pane_without_a_run_strip_is_no_run_strip() {
    let (mut driver, _app) = bare();

    assert_eq!(
        driver.run_task(&target(), None, true),
        Err(UiError::NoRunStrip)
    );
    assert_eq!(
        driver.run_task(&target(), None, false),
        Err(UiError::NoRunStrip)
    );

    assert_eq!(
        driver.desktop().events(),
        [
            FakeEvent::OpenUrl(LINK.to_owned()),
            FakeEvent::OpenUrl(LINK.to_owned())
        ]
    );
}

#[test]
fn a_tab_group_below_the_pane_s_children_is_not_the_strip() {
    let (mut driver, app) = bare();
    // The strip is a direct child of the pane; this one is a level deeper.
    main_pane(&app).add_child(FakeNode::new("AXGroup").with_child(
        FakeNode::new("AXTabGroup").with_child(FakeNode::new("AXButton").with_label("Run Web")),
    ));

    assert_eq!(
        driver.run_task(&target(), None, true),
        Err(UiError::NoRunStrip)
    );
    assert_eq!(driver.desktop().events(), link_then(&[]));
}

#[test]
fn a_locked_mac_is_locked_before_anything_is_pressed() {
    let mut setup = Setup::new(0, Some(0));
    setup.driver.desktop().set_session(Some(SessionState {
        locked: true,
        on_console: true,
    }));

    assert_eq!(
        setup.driver.run_task(&target(), None, true),
        Err(UiError::Locked)
    );
    assert_eq!(
        setup.driver.run_task(&target(), None, false),
        Err(UiError::Locked)
    );

    assert!(setup.events().is_empty());
    assert_eq!(setup.strip.running().as_deref(), Some("Web"));
}

#[test]
fn a_workspace_without_a_branch_is_no_branch() {
    let mut setup = Setup::new(0, None);
    let target = Target {
        branch: String::new(),
        ..target()
    };

    assert_eq!(
        setup.driver.run_task(&target, None, true),
        Err(UiError::NoBranch)
    );
    assert!(setup.events().is_empty());
}

#[test]
fn conductor_is_brought_to_the_front_before_the_first_press() {
    let mut setup = Setup::new(0, None);
    setup.driver.desktop().set_frontmost(Some(1));

    assert_eq!(setup.driver.run_task(&target(), None, true), changed("Web"));

    assert_eq!(
        setup.events(),
        [
            FakeEvent::OpenUrl(LINK.to_owned()),
            FakeEvent::Activate(PID),
            press("Run Web"),
        ]
    );
}

#[test]
fn a_front_that_cannot_be_taken_presses_nothing() {
    let mut setup = Setup::new(0, None);
    setup.driver.desktop().set_frontmost(Some(1));
    setup.driver.desktop().refuse_activation();

    assert_eq!(
        setup.driver.run_task(&target(), None, true),
        Err(UiError::NotFrontmost)
    );

    assert_eq!(setup.strip.running(), None);
    assert!(setup
        .events()
        .iter()
        .all(|event| !matches!(event, FakeEvent::Press(_))));
}
