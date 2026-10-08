//! The real desktop, through the methods that read only this process's own state or the list of
//! running apps. No test here calls `application`, `open_url`, `frontmost_pid`, `activate` or
//! `post_key`, so none touches another app's window.

use conductor_remote::ui::ax::is_trusted;
use conductor_remote::ui::desktop::Desktop;
use conductor_remote::ui::system::{SystemDesktop, AX_TIMEOUT_SECONDS};

#[test]
fn constructs_with_new_and_default() {
    let _made = SystemDesktop::new();
    let _default = SystemDesktop::default();
}

#[test]
fn trusted_matches_the_process_grant() {
    let desktop = SystemDesktop::new();
    assert_eq!(desktop.trusted(), is_trusted(false));
}

#[test]
fn session_and_conductor_pid_return() {
    let desktop = SystemDesktop::new();
    let _ = desktop.session();
    let _ = desktop.conductor_pid();
}

#[test]
fn the_timeout_is_two_seconds() {
    assert_eq!(AX_TIMEOUT_SECONDS, 2.0);
}
