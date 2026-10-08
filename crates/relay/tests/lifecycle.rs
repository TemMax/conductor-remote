//! The watcher's state: how launch and terminate events move the status.

use conductor_remote::contract::ConductorStatus;
use conductor_remote::lifecycle::{still_running, StatusTracker};

const WATCHED: &str = "com.example.watched";
const OTHER: &str = "com.example.other";

#[test]
fn starts_with_the_status_it_was_constructed_with() {
    let stopped = StatusTracker::new(WATCHED, ConductorStatus::NotRunning);
    assert_eq!(stopped.status(), ConductorStatus::NotRunning);
    assert_eq!(*stopped.subscribe().borrow(), ConductorStatus::NotRunning);

    let running = StatusTracker::new(WATCHED, ConductorStatus::Running);
    assert_eq!(running.status(), ConductorStatus::Running);
    assert_eq!(*running.subscribe().borrow(), ConductorStatus::Running);
}

#[test]
fn a_launch_of_the_watched_app_flips_to_running_and_a_subscriber_sees_it() {
    let tracker = StatusTracker::new(WATCHED, ConductorStatus::NotRunning);
    let mut subscriber = tracker.subscribe();

    tracker.on_launched(Some(WATCHED));

    assert_eq!(tracker.status(), ConductorStatus::Running);
    assert!(subscriber.has_changed().unwrap());
    assert_eq!(*subscriber.borrow_and_update(), ConductorStatus::Running);
}

#[test]
fn a_terminate_of_the_watched_app_flips_back_to_not_running() {
    let tracker = StatusTracker::new(WATCHED, ConductorStatus::NotRunning);
    let mut subscriber = tracker.subscribe();
    tracker.on_launched(Some(WATCHED));
    subscriber.mark_unchanged();

    tracker.on_terminated(Some(WATCHED));

    assert_eq!(tracker.status(), ConductorStatus::NotRunning);
    assert!(subscriber.has_changed().unwrap());
    assert_eq!(*subscriber.borrow_and_update(), ConductorStatus::NotRunning);
}

#[tokio::test]
async fn a_waiting_subscriber_is_woken_by_each_change() {
    let tracker = StatusTracker::new(WATCHED, ConductorStatus::NotRunning);
    let mut subscriber = tracker.subscribe();

    tracker.on_launched(Some(WATCHED));
    subscriber.changed().await.unwrap();
    assert_eq!(*subscriber.borrow_and_update(), ConductorStatus::Running);

    tracker.on_terminated(Some(WATCHED));
    subscriber.changed().await.unwrap();
    assert_eq!(*subscriber.borrow_and_update(), ConductorStatus::NotRunning);
}

#[test]
fn events_for_another_app_change_nothing() {
    let stopped = StatusTracker::new(WATCHED, ConductorStatus::NotRunning);
    let subscriber = stopped.subscribe();
    stopped.on_launched(Some(OTHER));
    assert_eq!(stopped.status(), ConductorStatus::NotRunning);
    assert!(!subscriber.has_changed().unwrap());

    let running = StatusTracker::new(WATCHED, ConductorStatus::Running);
    let subscriber = running.subscribe();
    running.on_terminated(Some(OTHER));
    assert_eq!(running.status(), ConductorStatus::Running);
    assert!(!subscriber.has_changed().unwrap());
}

#[test]
fn events_with_no_bundle_identifier_change_nothing() {
    let stopped = StatusTracker::new(WATCHED, ConductorStatus::NotRunning);
    let subscriber = stopped.subscribe();
    stopped.on_launched(None);
    assert_eq!(stopped.status(), ConductorStatus::NotRunning);
    assert!(!subscriber.has_changed().unwrap());

    let running = StatusTracker::new(WATCHED, ConductorStatus::Running);
    let subscriber = running.subscribe();
    running.on_terminated(None);
    assert_eq!(running.status(), ConductorStatus::Running);
    assert!(!subscriber.has_changed().unwrap());
}

#[test]
fn a_repeated_launch_does_not_notify_again() {
    let tracker = StatusTracker::new(WATCHED, ConductorStatus::NotRunning);
    let mut subscriber = tracker.subscribe();
    tracker.on_launched(Some(WATCHED));
    assert_eq!(*subscriber.borrow_and_update(), ConductorStatus::Running);

    tracker.on_launched(Some(WATCHED));

    assert_eq!(tracker.status(), ConductorStatus::Running);
    assert!(!subscriber.has_changed().unwrap());
}

#[test]
fn a_repeated_terminate_does_not_notify_again() {
    let tracker = StatusTracker::new(WATCHED, ConductorStatus::NotRunning);
    let subscriber = tracker.subscribe();

    tracker.on_terminated(Some(WATCHED));

    assert_eq!(tracker.status(), ConductorStatus::NotRunning);
    assert!(!subscriber.has_changed().unwrap());
}

#[test]
fn with_no_instances_nothing_is_still_running() {
    assert!(!still_running(&[], Some(100)));
}

#[test]
fn the_terminated_instance_alone_is_not_still_running() {
    assert!(!still_running(&[(100, false)], Some(100)));
}

#[test]
fn another_live_instance_is_still_running() {
    assert!(still_running(&[(100, false), (200, false)], Some(100)));
}

#[test]
fn another_instance_that_is_itself_terminated_is_not_still_running() {
    assert!(!still_running(&[(100, false), (200, true)], Some(100)));
}

#[test]
fn with_an_unknown_terminated_pid_a_live_instance_is_still_running() {
    assert!(still_running(&[(100, false)], None));
}
