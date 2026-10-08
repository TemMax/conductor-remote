//! The model menu and the agent settings over the fake desktop: reading the model names, and
//! applying a patch through the composer's menus. Nothing here reaches the Mac.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use conductor_remote::agent::{AgentPatch, Effort};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::driver::{AgentFailure, AgentOutcome, Target, UiDriver, UiError};
use conductor_remote::ui::fake::{
    add_agent_menus, conductor_app, main_pane, AgentMenuSpec, AgentMenus, FakeDesktop, FakeEvent,
    FakeNode, WindowSpec,
};
use conductor_remote::ui::node::UiNode;
use conductor_remote::ui::screen::SessionState;

const LINK: &str = "conductor://workspace?id=ws-1&session=s-1";
/// The model pop-up's label under `menus()`.
const POPUP: &str = "Change agent (Sonnet 4.6 · Medium)";
const PLAN_ON_ITEM: &str = "Exit plan mode ⇧ Tab";
const PLAN_OFF_ITEM: &str = "Plan mode ⇧ Tab";

/// A window already showing the target's workspace and its only chat.
fn window() -> WindowSpec {
    WindowSpec {
        repo: "relay".to_owned(),
        branch: "user/feature-x".to_owned(),
        sidebar: vec!["beta".to_owned()],
        chats: vec!["One".to_owned()],
        selected: 0,
        composer_value: None,
    }
}

fn target() -> Target {
    Target {
        workspace_id: "ws-1".to_owned(),
        session_id: Some("s-1".to_owned()),
        repo: Some("relay".to_owned()),
        branch: "user/feature-x".to_owned(),
        workspace_name: Some("beta".to_owned()),
        tab: None,
    }
}

fn model(name: &str, effort: &str, fast: bool) -> (String, String, bool) {
    (name.to_owned(), effort.to_owned(), fast)
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Five models, Sonnet 4.6 (Medium) current; the two GPT models open a new chat. Plan is off.
fn menus() -> AgentMenuSpec {
    AgentMenuSpec {
        models: vec![
            model("Opus 4.7", "High", false),
            model("Sonnet 4.6", "Medium", false),
            model("Haiku 4.5", "", false),
            model("GPT-5", "Medium", false),
            model("GPT-5 Codex", "High", false),
        ],
        current: 1,
        new_chat_models: names(&["GPT-5", "GPT-5 Codex"]),
        efforts: names(&["Low", "Medium", "High", "Extra high", "Max"]),
        fast_item: true,
        plan: Some(false),
    }
}

/// `menus()` with the current model's effort and fast changed.
fn menus_showing(effort: &str, fast: bool) -> AgentMenuSpec {
    let mut spec = menus();
    spec.models[1] = model("Sonnet 4.6", effort, fast);
    spec
}

struct Setup {
    driver: Driver<FakeDesktop>,
    app: FakeNode,
    menus: AgentMenus,
}

impl Setup {
    fn new(spec: &AgentMenuSpec) -> Setup {
        let app = conductor_app(&window());
        let menus = add_agent_menus(&app, spec);
        Setup {
            driver: Driver::new(FakeDesktop::new(app.clone())),
            app,
            menus,
        }
    }

    fn set(&mut self, patch: &AgentPatch) -> Result<AgentOutcome, AgentFailure> {
        self.driver.set_agent(&target(), patch)
    }

    fn list(&mut self) -> Result<Vec<String>, UiError> {
        self.driver.list_models(&target())
    }

    fn events(&self) -> Vec<FakeEvent> {
        self.driver.desktop().events()
    }

    fn presses(&self) -> Vec<String> {
        presses(self.driver.desktop())
    }

    fn pauses(&self) -> Vec<Duration> {
        pauses(self.driver.desktop())
    }

    fn lock(&self) {
        self.driver.desktop().set_session(Some(SessionState {
            locked: true,
            on_console: true,
        }));
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

fn pauses(desktop: &FakeDesktop) -> Vec<Duration> {
    desktop
        .events()
        .into_iter()
        .filter_map(|event| match event {
            FakeEvent::Pause(duration) => Some(duration),
            _ => None,
        })
        .collect()
}

/// A `conductor_app` window whose composer group holds `controls` and nothing that reacts.
fn inert(controls: &[FakeNode]) -> Driver<FakeDesktop> {
    let app = conductor_app(&window());
    let composer = app.find_label("composer").expect("composer group");
    for control in controls {
        composer.add_child(control.clone());
    }
    Driver::new(FakeDesktop::new(app))
}

fn pop_up(label: &str) -> FakeNode {
    FakeNode::new("AXPopUpButton").with_label(label)
}

fn press(label: &str) -> FakeEvent {
    FakeEvent::Press(Some(label.to_owned()))
}

fn pause(millis: u64) -> FakeEvent {
    FakeEvent::Pause(Duration::from_millis(millis))
}

fn link() -> FakeEvent {
    FakeEvent::OpenUrl(LINK.to_owned())
}

fn nine(millis: u64) -> Vec<Duration> {
    vec![Duration::from_millis(millis); 9]
}

fn model_patch(name: &str) -> AgentPatch {
    AgentPatch {
        model: Some(name.to_owned()),
        ..AgentPatch::default()
    }
}

fn effort_patch(effort: Effort) -> AgentPatch {
    AgentPatch {
        effort: Some(effort),
        ..AgentPatch::default()
    }
}

fn fast_patch(fast: bool) -> AgentPatch {
    AgentPatch {
        fast: Some(fast),
        ..AgentPatch::default()
    }
}

fn plan_patch(plan: bool) -> AgentPatch {
    AgentPatch {
        plan: Some(plan),
        ..AgentPatch::default()
    }
}

fn same_chat() -> Result<AgentOutcome, AgentFailure> {
    Ok(AgentOutcome { new_chat: false })
}

fn failed(error: UiError, new_chat: bool) -> Result<AgentOutcome, AgentFailure> {
    Err(AgentFailure { error, new_chat })
}

// ---- list_models ----

#[test]
fn list_models_reads_the_names_in_menu_order_and_closes_the_menu() {
    let mut setup = Setup::new(&menus());
    assert_eq!(
        setup.list(),
        Ok(names(&[
            "Opus 4.7",
            "Sonnet 4.6",
            "Haiku 4.5",
            "GPT-5",
            "GPT-5 Codex"
        ]))
    );
    assert!(!setup.menus.menu_open());
    // The deep link, the press that opens the menu and the press that closes it; no wait.
    assert_eq!(setup.events(), vec![link(), press(POPUP), press(POPUP)]);
    assert_eq!(setup.menus.shown(), POPUP);
}

#[test]
fn list_models_reads_a_menu_that_is_already_open() {
    let mut setup = Setup::new(&menus());
    setup
        .app
        .find_label(POPUP)
        .expect("pop-up")
        .press()
        .expect("press");
    assert!(setup.menus.menu_open());
    assert_eq!(setup.list().map(|names| names.len()), Ok(5));
    assert!(!setup.menus.menu_open());
    // One press by the test, one by the close.
    assert_eq!(setup.presses(), vec![POPUP, POPUP]);
}

#[test]
fn list_models_without_the_pop_up_is_no_model_picker() {
    let mut driver = inert(&[]);
    assert_eq!(driver.list_models(&target()), Err(UiError::NoModelPicker));
    assert!(presses(driver.desktop()).is_empty());
}

#[test]
fn list_models_without_a_composer_is_no_composer() {
    let app = conductor_app(&window());
    let composer = app.find_label("composer").expect("composer group");
    main_pane(&app).remove_child(&composer);
    let mut driver = Driver::new(FakeDesktop::new(app));
    assert_eq!(driver.list_models(&target()), Err(UiError::NoComposer));
}

#[test]
fn list_models_with_a_menu_that_never_opens_is_menu_not_opened() {
    let mut driver = inert(&[pop_up(POPUP)]);
    assert_eq!(
        driver.list_models(&target()),
        Err(UiError::MenuNotOpened("model".to_owned()))
    );
    // Ten looks, nine pauses between them.
    assert_eq!(presses(driver.desktop()), vec![POPUP]);
    assert_eq!(pauses(driver.desktop()), nine(150));
}

#[test]
fn list_models_with_a_menu_that_stays_open_is_menu_stuck() {
    let mut setup = Setup::new(&menus());
    setup.menus.keep_menus_open();
    assert_eq!(setup.list(), Err(UiError::MenuStuck));
    // Opened, then pressed twice to close, ten looks after each.
    assert_eq!(setup.presses(), vec![POPUP, POPUP, POPUP]);
    assert_eq!(setup.pauses(), [nine(150), nine(150)].concat());
}

#[test]
fn list_models_needs_a_branch_and_an_unlocked_mac() {
    let mut setup = Setup::new(&menus());
    let mut no_branch = target();
    no_branch.branch = String::new();
    assert_eq!(setup.driver.list_models(&no_branch), Err(UiError::NoBranch));
    setup.lock();
    assert_eq!(setup.list(), Err(UiError::Locked));
    assert!(setup.events().is_empty());
}

// ---- the model ----

#[test]
fn a_model_is_picked_by_its_exact_name() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&model_patch("Opus 4.7")), same_chat());
    assert_eq!(setup.menus.shown(), "Change agent (Opus 4.7 · High)");
    assert!(!setup.menus.menu_open());
    // The pick closed the menu, so nothing is pressed to close it.
    assert_eq!(
        setup.events(),
        vec![link(), press(POPUP), press("Opus 4.7 High")]
    );
}

#[test]
fn a_model_is_picked_by_a_unique_prefix() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&model_patch("Hai")), same_chat());
    assert_eq!(setup.menus.shown(), "Change agent (Haiku 4.5)");
    assert!(!setup.menus.menu_open());
}

#[test]
fn a_model_is_picked_in_another_letter_case() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&model_patch("oPUS 4.7")), same_chat());
    assert_eq!(setup.menus.shown(), "Change agent (Opus 4.7 · High)");

    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&model_patch("OPUS")), same_chat());
    assert_eq!(setup.menus.shown(), "Change agent (Opus 4.7 · High)");
}

#[test]
fn an_exact_name_wins_over_the_names_it_starts() {
    let mut setup = Setup::new(&menus());
    assert_eq!(
        setup.set(&model_patch("gpt-5")),
        Ok(AgentOutcome { new_chat: true })
    );
    assert_eq!(setup.menus.shown(), "Change agent (GPT-5 · Medium)");
}

#[test]
fn an_unknown_model_is_no_model_and_the_menu_is_closed() {
    let mut setup = Setup::new(&menus());
    assert_eq!(
        setup.set(&model_patch("Llama")),
        failed(UiError::NoModel("Llama".to_owned()), false)
    );
    assert!(!setup.menus.menu_open());
    assert_eq!(setup.presses(), vec![POPUP, POPUP]);
    assert_eq!(setup.menus.shown(), POPUP);
}

#[test]
fn an_ambiguous_prefix_is_several_models() {
    let mut setup = Setup::new(&menus());
    assert_eq!(
        setup.set(&model_patch("GPT")),
        failed(UiError::SeveralModels("GPT".to_owned()), false)
    );
    assert!(!setup.menus.menu_open());
    assert_eq!(setup.presses(), vec![POPUP, POPUP]);
}

#[test]
fn the_current_model_is_skipped_without_a_press() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&model_patch("sonnet 4.6")), same_chat());
    assert_eq!(setup.events(), vec![link()]);
}

#[test]
fn a_prefix_of_the_current_model_presses_no_item() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&model_patch("Son")), same_chat());
    // The menu was needed to resolve the prefix; it is closed again.
    assert_eq!(setup.presses(), vec![POPUP, POPUP]);
    assert!(!setup.menus.menu_open());
    assert_eq!(setup.menus.shown(), POPUP);
}

#[test]
fn a_new_chat_model_reports_the_new_chat() {
    let mut setup = Setup::new(&menus());
    let opened = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&opened);
    setup
        .menus
        .on_new_chat(move |name| seen.borrow_mut().push(name.to_owned()));
    assert_eq!(
        setup.set(&model_patch("GPT-5 Codex")),
        Ok(AgentOutcome { new_chat: true })
    );
    assert_eq!(*opened.borrow(), names(&["GPT-5 Codex"]));
    assert_eq!(setup.menus.shown(), "Change agent (GPT-5 Codex · High)");
    assert_eq!(
        setup.presses(),
        vec![POPUP, "GPT-5 Codex High Opens in new chat"]
    );
}

#[test]
fn a_failure_after_a_new_chat_model_still_reports_the_new_chat() {
    let mut setup = Setup::new(&menus());
    let patch = AgentPatch {
        model: Some("GPT-5".to_owned()),
        effort: Some(Effort::Ultracode),
        ..AgentPatch::default()
    };
    assert_eq!(
        setup.set(&patch),
        failed(UiError::NoEffort("Ultracode".to_owned()), true)
    );
    assert_eq!(setup.menus.shown(), "Change agent (GPT-5 · Medium)");
    assert!(!setup.menus.menu_open());
}

#[test]
fn a_failure_before_the_model_press_reports_no_new_chat() {
    // The model is not in the list: nothing was pressed in the menu.
    let mut setup = Setup::new(&menus());
    let patch = AgentPatch {
        model: Some("GPT-4".to_owned()),
        effort: Some(Effort::High),
        ..AgentPatch::default()
    };
    assert_eq!(
        setup.set(&patch),
        failed(UiError::NoModel("GPT-4".to_owned()), false)
    );

    // The menu never opens: the new-chat item is never reached.
    let mut driver = inert(&[pop_up(POPUP)]);
    assert_eq!(
        driver.set_agent(&target(), &model_patch("GPT-5")),
        failed(UiError::MenuNotOpened("model".to_owned()), false)
    );
    assert_eq!(presses(driver.desktop()), vec![POPUP]);
    assert_eq!(pauses(driver.desktop()), nine(150));
}

#[test]
fn an_ignored_model_press_is_model_not_applied() {
    let mut setup = Setup::new(&menus());
    setup.menus.ignore_model_presses();
    assert_eq!(
        setup.set(&model_patch("opus")),
        failed(UiError::ModelNotApplied("Opus 4.7".to_owned()), false)
    );
    assert_eq!(setup.presses(), vec![POPUP, "Opus 4.7 High"]);
    assert_eq!(setup.pauses(), nine(200));
    assert_eq!(setup.menus.shown(), POPUP);
    assert!(!setup.menus.menu_open());
}

#[test]
fn an_ignored_new_chat_model_press_still_reports_the_new_chat() {
    let mut setup = Setup::new(&menus());
    setup.menus.ignore_model_presses();
    assert_eq!(
        setup.set(&model_patch("GPT-5")),
        failed(UiError::ModelNotApplied("GPT-5".to_owned()), true)
    );
}

#[test]
fn a_model_step_that_fails_stops_the_steps_after_it() {
    let mut setup = Setup::new(&menus());
    let patch = AgentPatch {
        model: Some("Llama".to_owned()),
        effort: Some(Effort::High),
        plan: Some(true),
        fast: Some(true),
    };
    assert_eq!(
        setup.set(&patch),
        failed(UiError::NoModel("Llama".to_owned()), false)
    );
    assert_eq!(setup.presses(), vec![POPUP, POPUP]);
    assert_eq!(setup.menus.shown(), POPUP);
    assert_eq!(setup.menus.plan(), Some(false));
}

// ---- the effort ----

#[test]
fn each_effort_level_picks_its_label() {
    let levels = [
        (Effort::None, "Off"),
        (Effort::Low, "Low"),
        (Effort::Medium, "Medium"),
        (Effort::High, "High"),
        (Effort::Xhigh, "Extra high"),
        (Effort::Max, "Max"),
        (Effort::Ultracode, "Ultracode"),
    ];
    for (effort, label) in levels {
        let from = if label == "Low" { "High" } else { "Low" };
        let mut spec = menus_showing(from, false);
        spec.efforts = names(&[
            "Off",
            "Low",
            "Medium",
            "High",
            "Extra high",
            "Max",
            "Ultracode",
        ]);
        let mut setup = Setup::new(&spec);
        let popup = format!("Change agent (Sonnet 4.6 · {from})");
        assert_eq!(setup.set(&effort_patch(effort)), same_chat(), "{label}");
        assert_eq!(
            setup.menus.shown(),
            format!("Change agent (Sonnet 4.6 · {label})")
        );
        assert!(!setup.menus.menu_open(), "{label}");
        // The pop-up, the Effort entry, the level, then the pop-up again to close the menu.
        assert_eq!(
            setup.presses(),
            vec![
                popup,
                format!("Effort {from}"),
                label.to_owned(),
                format!("Change agent (Sonnet 4.6 · {label})"),
            ]
        );
        assert!(setup.pauses().is_empty(), "{label}");
    }
}

#[test]
fn ultracode_picks_ultra_when_only_that_exists() {
    let mut spec = menus();
    spec.efforts = names(&["Low", "Medium", "High", "Ultra"]);
    let mut setup = Setup::new(&spec);
    assert_eq!(setup.set(&effort_patch(Effort::Ultracode)), same_chat());
    assert_eq!(setup.menus.shown(), "Change agent (Sonnet 4.6 · Ultra)");
}

#[test]
fn ultracode_prefers_its_first_label_whatever_the_menu_order() {
    let mut spec = menus();
    spec.efforts = names(&["Ultra", "Ultracode"]);
    let mut setup = Setup::new(&spec);
    assert_eq!(setup.set(&effort_patch(Effort::Ultracode)), same_chat());
    assert_eq!(setup.menus.shown(), "Change agent (Sonnet 4.6 · Ultracode)");
}

#[test]
fn a_level_the_menu_lacks_is_no_effort() {
    let mut spec = menus();
    spec.efforts = names(&["Low", "Medium", "High"]);
    let mut setup = Setup::new(&spec);
    assert_eq!(
        setup.set(&effort_patch(Effort::Max)),
        failed(UiError::NoEffort("Max".to_owned()), false)
    );
    assert_eq!(setup.presses(), vec![POPUP, "Effort Medium", POPUP]);
    assert!(!setup.menus.menu_open());
    assert_eq!(setup.menus.shown(), POPUP);
}

#[test]
fn a_menu_without_an_effort_entry_is_no_effort() {
    let mut spec = menus();
    spec.efforts = Vec::new();
    let mut setup = Setup::new(&spec);
    assert_eq!(
        setup.set(&effort_patch(Effort::Xhigh)),
        failed(UiError::NoEffort("Extra high".to_owned()), false)
    );
    assert_eq!(setup.presses(), vec![POPUP, POPUP]);
    assert!(!setup.menus.menu_open());

    // The label NoEffort names is the level's first.
    let mut setup = Setup::new(&spec);
    assert_eq!(
        setup.set(&effort_patch(Effort::Ultracode)),
        failed(UiError::NoEffort("Ultracode".to_owned()), false)
    );
}

#[test]
fn an_ignored_effort_press_is_effort_not_applied() {
    let mut setup = Setup::new(&menus());
    setup.menus.ignore_effort_presses();
    assert_eq!(
        setup.set(&effort_patch(Effort::High)),
        failed(UiError::EffortNotApplied("High".to_owned()), false)
    );
    assert_eq!(setup.presses(), vec![POPUP, "Effort Medium", "High", POPUP]);
    assert_eq!(setup.pauses(), nine(200));
    assert!(!setup.menus.menu_open());
}

#[test]
fn an_effort_already_shown_is_skipped_without_a_press() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&effort_patch(Effort::Medium)), same_chat());
    assert_eq!(setup.events(), vec![link()]);

    // Either label of a level counts.
    let mut setup = Setup::new(&menus_showing("Ultra", false));
    assert_eq!(setup.set(&effort_patch(Effort::Ultracode)), same_chat());
    assert_eq!(setup.events(), vec![link()]);

    // Fast, which the label names last, is not taken for the effort.
    let mut setup = Setup::new(&menus_showing("Medium", true));
    assert_eq!(setup.set(&effort_patch(Effort::Medium)), same_chat());
    assert_eq!(setup.events(), vec![link()]);
}

#[test]
fn an_effort_entry_that_opens_nothing_is_menu_not_opened() {
    // A model menu that is already open and whose Effort entry does not react.
    let menu = FakeNode::new("AXMenu")
        .with_label(POPUP)
        .with_child(FakeNode::new("AXMenuItem").with_label("Effort Medium"));
    let mut driver = inert(&[pop_up(POPUP), FakeNode::new("AXGroup").with_child(menu)]);
    assert_eq!(
        driver.set_agent(&target(), &effort_patch(Effort::High)),
        failed(UiError::MenuNotOpened("effort".to_owned()), false)
    );
    // The entry, nine pauses for its menu, then two presses to close a menu that stays.
    assert_eq!(
        presses(driver.desktop()),
        vec!["Effort Medium", POPUP, POPUP]
    );
    assert_eq!(
        pauses(driver.desktop()),
        [nine(150), nine(150), nine(150)].concat()
    );
}

// ---- Fast ----

#[test]
fn fast_is_switched_on() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&fast_patch(true)), same_chat());
    assert_eq!(
        setup.menus.shown(),
        "Change agent (Sonnet 4.6 · Medium · Fast)"
    );
    assert!(!setup.menus.menu_open());
    assert_eq!(
        setup.events(),
        vec![
            link(),
            press(POPUP),
            press("Fast"),
            press("Change agent (Sonnet 4.6 · Medium · Fast)"),
        ]
    );
}

#[test]
fn fast_is_switched_off() {
    let mut setup = Setup::new(&menus_showing("Medium", true));
    assert_eq!(setup.set(&fast_patch(false)), same_chat());
    assert_eq!(setup.menus.shown(), POPUP);
    assert!(!setup.menus.menu_open());
    assert_eq!(
        setup.presses(),
        vec!["Change agent (Sonnet 4.6 · Medium · Fast)", "Fast", POPUP]
    );
}

#[test]
fn fast_already_as_wanted_presses_nothing() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&fast_patch(false)), same_chat());
    assert_eq!(setup.events(), vec![link()]);

    let mut setup = Setup::new(&menus_showing("Medium", true));
    assert_eq!(setup.set(&fast_patch(true)), same_chat());
    assert_eq!(setup.events(), vec![link()]);
}

#[test]
fn fast_on_a_model_without_an_effort_is_not_read_as_its_effort() {
    let mut spec = menus();
    spec.models[2] = model("Haiku 4.5", "", true);
    spec.current = 2;
    let mut setup = Setup::new(&spec);
    assert_eq!(setup.menus.shown(), "Change agent (Haiku 4.5 · Fast)");
    assert_eq!(setup.set(&fast_patch(true)), same_chat());
    assert_eq!(setup.events(), vec![link()]);
    assert_eq!(setup.set(&fast_patch(false)), same_chat());
    assert_eq!(setup.menus.shown(), "Change agent (Haiku 4.5)");
}

#[test]
fn a_menu_without_a_fast_item_is_no_fast() {
    let mut spec = menus();
    spec.fast_item = false;
    let mut setup = Setup::new(&spec);
    assert_eq!(setup.set(&fast_patch(true)), failed(UiError::NoFast, false));
    assert_eq!(setup.presses(), vec![POPUP, POPUP]);
    assert!(!setup.menus.menu_open());
}

#[test]
fn a_fast_item_already_as_wanted_is_not_pressed_and_the_label_is_waited_for() {
    // The item says Fast is on, the pop-up never does.
    let menu = FakeNode::new("AXMenu").with_label(POPUP).with_child(
        FakeNode::new("AXMenuItem")
            .with_label("Fast")
            .with_flag(true),
    );
    let mut driver = inert(&[pop_up(POPUP), FakeNode::new("AXGroup").with_child(menu)]);
    assert_eq!(
        driver.set_agent(&target(), &fast_patch(true)),
        failed(UiError::FastNotApplied, false)
    );
    // Only the two presses that try to close the menu.
    assert_eq!(presses(driver.desktop()), vec![POPUP, POPUP]);
    assert_eq!(
        pauses(driver.desktop()),
        [nine(200), nine(150), nine(150)].concat()
    );
}

// ---- Plan ----

#[test]
fn plan_is_switched_on() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&plan_patch(true)), same_chat());
    assert_eq!(setup.menus.plan(), Some(true));
    assert!(!setup.menus.menu_open());
    // Add, the Plan item, a pause, then Add twice: once to read the state, once to close.
    assert_eq!(
        setup.events(),
        vec![
            link(),
            press("Add"),
            press(PLAN_OFF_ITEM),
            pause(300),
            press("Add"),
            press("Add"),
        ]
    );
}

#[test]
fn plan_is_switched_off() {
    let mut spec = menus();
    spec.plan = Some(true);
    let mut setup = Setup::new(&spec);
    assert_eq!(setup.set(&plan_patch(false)), same_chat());
    assert_eq!(setup.menus.plan(), Some(false));
    assert!(!setup.menus.menu_open());
    assert_eq!(setup.presses(), vec!["Add", PLAN_ON_ITEM, "Add", "Add"]);
}

#[test]
fn plan_already_as_wanted_presses_only_add() {
    let mut setup = Setup::new(&menus());
    assert_eq!(setup.set(&plan_patch(false)), same_chat());
    assert_eq!(setup.events(), vec![link(), press("Add"), press("Add")]);
    assert_eq!(setup.menus.plan(), Some(false));
    assert!(!setup.menus.menu_open());

    let mut spec = menus();
    spec.plan = Some(true);
    let mut setup = Setup::new(&spec);
    assert_eq!(setup.set(&plan_patch(true)), same_chat());
    assert_eq!(setup.presses(), vec!["Add", "Add"]);
    assert_eq!(setup.menus.plan(), Some(true));
}

#[test]
fn an_add_menu_without_a_plan_item_is_no_plan() {
    let mut spec = menus();
    spec.plan = None;
    let mut setup = Setup::new(&spec);
    assert_eq!(setup.set(&plan_patch(true)), failed(UiError::NoPlan, false));
    assert_eq!(setup.presses(), vec!["Add", "Add"]);
    assert!(!setup.menus.menu_open());
}

#[test]
fn a_composer_without_an_add_pop_up_is_no_plan() {
    let mut driver = inert(&[pop_up(POPUP)]);
    assert_eq!(
        driver.set_agent(&target(), &plan_patch(true)),
        failed(UiError::NoPlan, false)
    );
    assert!(presses(driver.desktop()).is_empty());
}

#[test]
fn an_add_menu_that_never_opens_is_menu_not_opened() {
    let mut driver = inert(&[pop_up("Add")]);
    assert_eq!(
        driver.set_agent(&target(), &plan_patch(true)),
        failed(UiError::MenuNotOpened("Add".to_owned()), false)
    );
    assert_eq!(presses(driver.desktop()), vec!["Add"]);
    assert_eq!(pauses(driver.desktop()), nine(150));
}

#[test]
fn a_plan_item_that_changes_nothing_is_plan_not_applied() {
    // An Add pop-up whose menu always offers "Plan mode", whatever is pressed.
    let slot = FakeNode::new("AXGroup");
    let add = pop_up("Add");
    let holder = slot.clone();
    add.on_press(move |_| match holder.child_nodes().first() {
        Some(open) => holder.remove_child(open),
        None => {
            let item = FakeNode::new("AXMenuItem").with_label(PLAN_OFF_ITEM);
            let closing = holder.clone();
            item.on_press(move |_| {
                if let Some(open) = closing.child_nodes().first() {
                    closing.remove_child(open);
                }
            });
            holder.add_child(FakeNode::new("AXMenu").with_label("Add").with_child(item));
        }
    });
    let mut driver = inert(&[add, slot.clone()]);
    assert_eq!(
        driver.set_agent(&target(), &plan_patch(true)),
        failed(UiError::PlanNotApplied, false)
    );
    assert_eq!(
        presses(driver.desktop()),
        vec!["Add", PLAN_OFF_ITEM, "Add", "Add"]
    );
    assert!(slot.child_nodes().is_empty());
}

// ---- the whole patch ----

#[test]
fn a_patch_with_all_four_fields_applies_them_in_order() {
    let mut setup = Setup::new(&menus());
    let patch = AgentPatch {
        model: Some("Opus 4.7".to_owned()),
        effort: Some(Effort::Max),
        plan: Some(true),
        fast: Some(true),
    };
    assert_eq!(setup.set(&patch), same_chat());
    assert_eq!(setup.menus.shown(), "Change agent (Opus 4.7 · Max · Fast)");
    assert_eq!(setup.menus.plan(), Some(true));
    assert!(!setup.menus.menu_open());
    assert_eq!(
        setup.events(),
        vec![
            link(),
            // The model: its pick closes the menu.
            press(POPUP),
            press("Opus 4.7 High"),
            // The effort, in the menu opened again.
            press("Change agent (Opus 4.7 · High)"),
            press("Effort High"),
            press("Max"),
            // Fast, in the menu the effort left open; then the menu is closed.
            press("Fast"),
            press("Change agent (Opus 4.7 · Max · Fast)"),
            // Plan.
            press("Add"),
            press(PLAN_OFF_ITEM),
            pause(300),
            press("Add"),
            press("Add"),
        ]
    );
}

#[test]
fn a_patch_already_in_place_presses_only_add() {
    let mut setup = Setup::new(&menus());
    let patch = AgentPatch {
        model: Some("Sonnet 4.6".to_owned()),
        effort: Some(Effort::Medium),
        plan: Some(false),
        fast: Some(false),
    };
    assert_eq!(setup.set(&patch), same_chat());
    assert_eq!(setup.presses(), vec!["Add", "Add"]);
}

#[test]
fn a_model_menu_that_stays_open_is_menu_stuck() {
    let mut setup = Setup::new(&menus());
    setup.menus.keep_menus_open();
    // The effort was applied; the menu it was applied in cannot be closed.
    assert_eq!(
        setup.set(&effort_patch(Effort::High)),
        failed(UiError::MenuStuck, false)
    );
    let stuck = "Change agent (Sonnet 4.6 · High)";
    assert_eq!(setup.menus.shown(), stuck);
    assert_eq!(
        setup.presses(),
        vec![POPUP, "Effort Medium", "High", stuck, stuck]
    );
    assert_eq!(setup.pauses(), [nine(150), nine(150)].concat());
}

#[test]
fn a_stuck_model_menu_stops_the_plan_step() {
    let mut setup = Setup::new(&menus());
    setup.menus.keep_menus_open();
    let patch = AgentPatch {
        effort: Some(Effort::High),
        plan: Some(true),
        ..AgentPatch::default()
    };
    assert_eq!(setup.set(&patch), failed(UiError::MenuStuck, false));
    assert_eq!(setup.menus.plan(), Some(false));
    assert!(!setup.presses().iter().any(|label| label == "Add"));
}

#[test]
fn a_step_error_wins_over_a_menu_that_stays_open() {
    let mut spec = menus();
    spec.fast_item = false;
    let mut setup = Setup::new(&spec);
    setup.menus.keep_menus_open();
    assert_eq!(setup.set(&fast_patch(true)), failed(UiError::NoFast, false));
    // The close was still tried, twice.
    assert_eq!(setup.presses(), vec![POPUP, POPUP, POPUP]);
}

#[test]
fn an_add_menu_that_stays_open_is_menu_stuck() {
    let mut setup = Setup::new(&menus());
    setup.menus.keep_menus_open();
    assert_eq!(
        setup.set(&plan_patch(false)),
        failed(UiError::MenuStuck, false)
    );
    assert_eq!(setup.presses(), vec!["Add", "Add"]);
    assert_eq!(setup.pauses(), nine(150));
}

#[test]
fn an_empty_patch_presses_nothing_and_opens_no_link() {
    let mut setup = Setup::new(&menus());
    assert_eq!(
        setup.set(&AgentPatch::default()),
        Ok(AgentOutcome::default())
    );
    assert!(setup.events().is_empty());

    // Not even the checks run: a locked Mac and a missing branch do not matter.
    setup.lock();
    let mut no_branch = target();
    no_branch.branch = String::new();
    assert_eq!(
        setup.driver.set_agent(&no_branch, &AgentPatch::default()),
        Ok(AgentOutcome::default())
    );
    assert!(setup.events().is_empty());
}

#[test]
fn a_locked_mac_is_locked_before_anything_is_pressed() {
    let mut setup = Setup::new(&menus());
    setup.lock();
    let patch = AgentPatch {
        model: Some("GPT-5".to_owned()),
        effort: Some(Effort::High),
        plan: Some(true),
        fast: Some(true),
    };
    assert_eq!(setup.set(&patch), failed(UiError::Locked, false));
    assert!(setup.events().is_empty());
    assert_eq!(setup.menus.shown(), POPUP);
    assert_eq!(setup.menus.plan(), Some(false));
}

#[test]
fn a_workspace_without_a_branch_is_no_branch() {
    let mut setup = Setup::new(&menus());
    let mut no_branch = target();
    no_branch.branch = String::new();
    assert_eq!(
        setup.driver.set_agent(&no_branch, &model_patch("Opus 4.7")),
        failed(UiError::NoBranch, false)
    );
    assert!(setup.events().is_empty());
}

#[test]
fn a_pop_up_whose_label_is_not_the_agent_form_is_no_model_picker() {
    let mut driver = inert(&[pop_up("Change agent")]);
    assert_eq!(
        driver.set_agent(&target(), &model_patch("Opus 4.7")),
        failed(UiError::NoModelPicker, false)
    );
    assert!(presses(driver.desktop()).is_empty());
}

#[test]
fn the_model_menu_is_closed_after_an_error_in_each_menu_step() {
    // Step 1: the model is unknown.
    let mut setup = Setup::new(&menus());
    assert!(setup.set(&model_patch("Llama")).is_err());
    assert!(!setup.menus.menu_open());

    // Step 2: the level is missing, with the Effort menu open by then.
    let mut setup = Setup::new(&menus());
    assert_eq!(
        setup.set(&effort_patch(Effort::Ultracode)),
        failed(UiError::NoEffort("Ultracode".to_owned()), false)
    );
    assert!(!setup.menus.menu_open());
    assert!(setup.app.find_role("AXMenu").is_none());

    // Step 3: no Fast item.
    let mut spec = menus();
    spec.fast_item = false;
    let mut setup = Setup::new(&spec);
    assert!(setup.set(&fast_patch(true)).is_err());
    assert!(!setup.menus.menu_open());
    assert!(setup.app.find_role("AXMenu").is_none());
}
