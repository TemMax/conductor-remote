//! The chat actions' contract: the new `UiError` texts and the `UiDriver` defaults.

use conductor_remote::ui::driver::{Target, UiDriver, UiError, ViewReport};

#[test]
fn the_new_errors_read_as_specified() {
    let cases = [
        (
            UiError::NeedsConfirmation,
            "Conductor asks to confirm - an agent is still working",
        ),
        (
            UiError::DialogStuck,
            "couldn't dismiss Conductor's dialog - dismiss it on your Mac and try again.",
        ),
        (
            UiError::NoSidebarRow("fix-login".to_owned()),
            "couldn't find fix-login in Conductor's sidebar",
        ),
        (
            UiError::NoStatusMenu,
            "couldn't open the workspace's Set status menu",
        ),
        (
            UiError::NoStatus("In review".to_owned()),
            "Conductor has no status named In review",
        ),
        (
            UiError::NoContinue,
            "Conductor shows no Continue button for this workspace - is its pull request merged?",
        ),
        (
            UiError::NoCreateDialog,
            "Conductor did not show its New workspace dialog",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}

#[test]
fn needs_confirmation_sent_nothing_and_is_not_a_lock() {
    assert!(UiError::NeedsConfirmation.sent_nothing());
    assert!(!UiError::NeedsConfirmation.is_lock());
}

struct Minimal;

impl UiDriver for Minimal {
    fn trusted(&self) -> bool {
        true
    }
    fn send_prompt(&mut self, _: &Target, _: &str, _: bool) -> Result<u32, UiError> {
        Ok(1)
    }
    fn stop_turn(&mut self, _: &Target) -> Result<(), UiError> {
        Ok(())
    }
    fn new_chat(&mut self, _: &Target) -> Result<(), UiError> {
        Ok(())
    }
    fn locate(&mut self) -> Result<ViewReport, UiError> {
        Ok(ViewReport::default())
    }
    fn open_link(&mut self, _: &str) -> Result<(), UiError> {
        Ok(())
    }
}

#[test]
fn a_driver_without_chat_actions_says_there_is_no_window() {
    let target = Target {
        workspace_id: "w1".to_owned(),
        session_id: Some("s1".to_owned()),
        repo: None,
        branch: "main".to_owned(),
        workspace_name: None,
        tab: None,
    };
    let mut driver = Minimal;
    assert_eq!(driver.close_chat(&target, false), Err(UiError::NoWindow));
    assert_eq!(
        driver.set_status(&target, "row", "In review"),
        Err(UiError::NoWindow)
    );
    assert_eq!(driver.archive(&target, true), Err(UiError::NoWindow));
    assert_eq!(driver.press_continue(&target), Err(UiError::NoWindow));
    assert_eq!(driver.confirm_create(), Err(UiError::NoWindow));
}
