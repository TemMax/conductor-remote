//! Keep-awake: the caffeinate arguments, arming, re-arming, disarming and expiry.
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use conductor_remote::host::nosleep::{Child, NoSleep, Spawner, MAX_SECONDS};
use serde_json::json;

#[derive(Default)]
struct ChildFlags {
    killed: AtomicBool,
    exited: AtomicBool,
}

struct FakeChild {
    pid: u32,
    flags: Arc<ChildFlags>,
}

impl Child for FakeChild {
    fn id(&self) -> u32 {
        self.pid
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        let done =
            self.flags.killed.load(Ordering::SeqCst) || self.flags.exited.load(Ordering::SeqCst);
        Ok(done.then_some(0))
    }
    fn kill(&mut self) -> io::Result<()> {
        self.flags.killed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct FakeSpawner {
    unavailable: bool,
    fail: bool,
    next_pid: AtomicU32,
    calls: Mutex<Vec<Vec<String>>>,
    children: Mutex<Vec<Arc<ChildFlags>>>,
}

impl Spawner for FakeSpawner {
    fn available(&self) -> bool {
        !self.unavailable
    }
    fn spawn(&self, args: &[String]) -> io::Result<Box<dyn Child>> {
        if self.fail {
            return Err(io::Error::other("no such program"));
        }
        self.calls.lock().unwrap().push(args.to_vec());
        let flags = Arc::new(ChildFlags::default());
        self.children.lock().unwrap().push(flags.clone());
        let pid = 1000 + self.next_pid.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeChild { pid, flags }))
    }
}

fn setup(prevent_screen_lock: bool) -> (NoSleep, Arc<FakeSpawner>, Arc<AtomicI64>) {
    setup_with(FakeSpawner::default(), prevent_screen_lock)
}

fn setup_with(
    spawner: FakeSpawner,
    prevent_screen_lock: bool,
) -> (NoSleep, Arc<FakeSpawner>, Arc<AtomicI64>) {
    let spawner = Arc::new(spawner);
    let now = Arc::new(AtomicI64::new(1_700_000_000_000));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    (
        NoSleep::new(spawner.clone(), prevent_screen_lock, clock),
        spawner,
        now,
    )
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

#[test]
fn arguments_without_screen_lock_prevention() {
    let (mut nosleep, spawner, _) = setup(false);
    nosleep.arm(3600).unwrap();
    let pid = std::process::id().to_string();
    assert_eq!(
        spawner.calls.lock().unwrap()[0],
        strings(&["-i", "-m", "-s", "-t", "3600", "-w", &pid])
    );
}

#[test]
fn arguments_with_screen_lock_prevention() {
    let (mut nosleep, spawner, _) = setup(true);
    nosleep.arm(90).unwrap();
    let pid = std::process::id().to_string();
    assert_eq!(
        spawner.calls.lock().unwrap()[0],
        strings(&["-i", "-m", "-s", "-t", "90", "-d", "-w", &pid])
    );
}

#[test]
fn idle_state_is_not_armed() {
    let (mut nosleep, _, _) = setup(false);
    let state = nosleep.state();
    assert!(state.available);
    assert!(!state.armed);
    assert_eq!(state.until, None);
    assert_eq!(state.pid, None);
    assert_eq!(state.max_seconds, MAX_SECONDS);
}

#[test]
fn unavailable_spawner_reads_unavailable() {
    let (mut nosleep, _, _) = setup_with(
        FakeSpawner {
            unavailable: true,
            ..Default::default()
        },
        false,
    );
    assert!(!nosleep.state().available);
}

#[test]
fn arm_reports_the_window() {
    let (mut nosleep, spawner, now) = setup(false);
    let armed = nosleep.arm(60).unwrap();
    assert!(armed.armed);
    assert_eq!(armed.pid, Some(1000));
    assert_eq!(armed.until, Some(now.load(Ordering::SeqCst) + 60_000));
    assert_eq!(nosleep.state(), armed);
    assert_eq!(spawner.calls.lock().unwrap().len(), 1);
}

#[test]
fn rearm_kills_the_old_child_and_starts_a_new_one() {
    let (mut nosleep, spawner, now) = setup(false);
    nosleep.arm(60).unwrap();
    now.fetch_add(10_000, Ordering::SeqCst);
    let second = nosleep.arm(120).unwrap();
    let children = spawner.children.lock().unwrap();
    assert_eq!(children.len(), 2);
    assert!(children[0].killed.load(Ordering::SeqCst));
    assert!(!children[1].killed.load(Ordering::SeqCst));
    assert_eq!(second.pid, Some(1001));
    assert_eq!(second.until, Some(now.load(Ordering::SeqCst) + 120_000));
}

#[test]
fn disarm_kills_the_child() {
    let (mut nosleep, spawner, _) = setup(false);
    nosleep.arm(60).unwrap();
    let state = nosleep.disarm();
    assert!(spawner.children.lock().unwrap()[0]
        .killed
        .load(Ordering::SeqCst));
    assert!(!state.armed);
    assert_eq!(state.pid, None);
    assert_eq!(nosleep.state(), state);
}

#[test]
fn disarm_when_idle_is_harmless() {
    let (mut nosleep, spawner, _) = setup(false);
    assert!(!nosleep.disarm().armed);
    assert!(spawner.children.lock().unwrap().is_empty());
}

#[test]
fn an_exited_child_reads_as_not_armed() {
    let (mut nosleep, spawner, _) = setup(true);
    assert!(nosleep.arm(5).unwrap().armed);
    spawner.children.lock().unwrap()[0]
        .exited
        .store(true, Ordering::SeqCst);
    let state = nosleep.state();
    assert!(!state.armed);
    assert_eq!(state.until, None);
    assert_eq!(state.pid, None);
    assert!(!state.prevents_screen_lock);
    // The expired child is gone: disarming does not kill it again, and arming starts afresh.
    nosleep.disarm();
    assert!(!spawner.children.lock().unwrap()[0]
        .killed
        .load(Ordering::SeqCst));
    assert!(nosleep.arm(5).unwrap().armed);
    assert_eq!(spawner.children.lock().unwrap().len(), 2);
}

#[test]
fn seconds_are_bounded() {
    let (mut nosleep, spawner, _) = setup(false);
    let message = "seconds must be between 1 and 604800";
    assert_eq!(nosleep.arm(0).unwrap_err(), message);
    assert_eq!(nosleep.arm(MAX_SECONDS + 1).unwrap_err(), message);
    assert!(spawner.calls.lock().unwrap().is_empty());
    assert!(nosleep.arm(1).is_ok());
    assert!(nosleep.arm(MAX_SECONDS).is_ok());
    assert_eq!(MAX_SECONDS, 604_800);
}

#[test]
fn a_rejected_arm_leaves_the_running_window_alone() {
    let (mut nosleep, spawner, _) = setup(false);
    nosleep.arm(60).unwrap();
    assert!(nosleep.arm(0).is_err());
    assert!(!spawner.children.lock().unwrap()[0]
        .killed
        .load(Ordering::SeqCst));
    assert!(nosleep.state().armed);
}

#[test]
fn a_failed_spawn_is_an_error_and_not_armed() {
    let (mut nosleep, _, _) = setup_with(
        FakeSpawner {
            fail: true,
            ..Default::default()
        },
        false,
    );
    let err = nosleep.arm(60).unwrap_err();
    assert!(err.contains("no such program"), "{err}");
    assert!(!nosleep.state().armed);
}

#[test]
fn state_json_uses_the_phones_shape() {
    let (mut nosleep, _, now) = setup(true);
    assert_eq!(
        serde_json::to_value(nosleep.state()).unwrap(),
        json!({
            "available": true,
            "armed": false,
            "until": null,
            "pid": null,
            "preventsScreenLock": false,
            "maxSeconds": 604800
        })
    );
    nosleep.arm(60).unwrap();
    assert_eq!(
        serde_json::to_value(nosleep.state()).unwrap(),
        json!({
            "available": true,
            "armed": true,
            "until": now.load(Ordering::SeqCst) + 60_000,
            "pid": 1000,
            "preventsScreenLock": true,
            "maxSeconds": 604800
        })
    );
    assert!(serde_json::to_value(nosleep.state())
        .unwrap()
        .get("willSleep")
        .is_none());
}
