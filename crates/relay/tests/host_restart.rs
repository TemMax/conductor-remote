use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use conductor_remote::delivery::WriteAnswer;
use conductor_remote::host::restart::{restart, AppControl, RestartTimings};
use serde_json::json;
use tokio::time::Instant;

/// A Conductor that quits and shows its window after a number of looks, counted from the call that
/// asks for it (the quit) or from the launch (the window).
struct FakeApp {
    state: Mutex<State>,
    quit_looks: Option<usize>,
    window_looks: Option<usize>,
    launch_error: Option<&'static str>,
}

struct State {
    running: bool,
    terminated: bool,
    launched: bool,
    quit_looks_left: usize,
    window_looks_left: usize,
    calls: Vec<&'static str>,
    threads: Vec<ThreadId>,
    launches: Vec<Instant>,
}

impl FakeApp {
    /// After `terminate`, `running` answers true `quit_looks` more times and then false (`None`:
    /// it never stops). After `launch`, `has_window` answers false `window_looks` times and then
    /// true (`None`: never).
    fn new(running: bool, quit_looks: Option<usize>, window_looks: Option<usize>) -> Arc<FakeApp> {
        FakeApp::build(running, quit_looks, window_looks, None)
    }

    fn failing_launch(message: &'static str) -> Arc<FakeApp> {
        FakeApp::build(true, Some(0), Some(0), Some(message))
    }

    fn build(
        running: bool,
        quit_looks: Option<usize>,
        window_looks: Option<usize>,
        launch_error: Option<&'static str>,
    ) -> Arc<FakeApp> {
        Arc::new(FakeApp {
            state: Mutex::new(State {
                running,
                terminated: false,
                launched: false,
                quit_looks_left: quit_looks.unwrap_or(0),
                window_looks_left: window_looks.unwrap_or(0),
                calls: Vec::new(),
                threads: Vec::new(),
                launches: Vec::new(),
            }),
            quit_looks,
            window_looks,
            launch_error,
        })
    }

    fn calls(&self) -> Vec<&'static str> {
        self.state.lock().unwrap().calls.clone()
    }

    fn count(&self, name: &str) -> usize {
        self.calls().iter().filter(|call| **call == name).count()
    }

    /// When each `launch` was called.
    fn launches(&self) -> Vec<Instant> {
        self.state.lock().unwrap().launches.clone()
    }

    fn threads(&self) -> Vec<ThreadId> {
        self.state.lock().unwrap().threads.clone()
    }
}

impl AppControl for FakeApp {
    fn running(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        state.calls.push("running");
        state.threads.push(std::thread::current().id());
        if !state.terminated || self.quit_looks.is_none() {
            return state.running;
        }
        if state.quit_looks_left > 0 {
            state.quit_looks_left -= 1;
            return true;
        }
        state.running = false;
        false
    }

    fn terminate(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        state.calls.push("terminate");
        state.threads.push(std::thread::current().id());
        state.terminated = true;
        true
    }

    fn launch(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        state.calls.push("launch");
        state.launches.push(Instant::now());
        state.threads.push(std::thread::current().id());
        if let Some(message) = self.launch_error {
            return Err(message.to_string());
        }
        state.launched = true;
        state.running = true;
        Ok(())
    }

    fn has_window(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        state.calls.push("has_window");
        state.threads.push(std::thread::current().id());
        if !state.launched || self.window_looks.is_none() {
            return false;
        }
        if state.window_looks_left > 0 {
            state.window_looks_left -= 1;
            return false;
        }
        true
    }
}

async fn run(app: &Arc<FakeApp>, working: usize, locked: bool, stop_agents: bool) -> WriteAnswer {
    restart(
        app.clone(),
        working,
        locked,
        stop_agents,
        RestartTimings::default(),
    )
    .await
}

fn failure(status: u16, error: &str) -> WriteAnswer {
    WriteAnswer::json(status, json!({ "ok": false, "error": error }))
}

#[test]
fn the_default_timings() {
    let timings = RestartTimings::default();
    assert_eq!(timings.quit_wait, Duration::from_secs(15));
    assert_eq!(timings.window_wait, Duration::from_secs(30));
    assert_eq!(timings.poll, Duration::from_millis(250));
}

// 200: restarted.

#[tokio::test(start_paused = true)]
async fn a_restart_quits_relaunches_and_reports_the_whole_time() {
    // Four looks until it is gone (at 0, 250, 500, 750 ms), three until the window shows (at 0,
    // 250, 500 ms): 750 + 500 ms.
    let app = FakeApp::new(true, Some(3), Some(2));
    let answer = run(&app, 0, false, false).await;
    assert_eq!(
        answer,
        WriteAnswer::json(200, json!({ "ok": true, "ms": 1250 }))
    );
    let calls = app.calls();
    let terminate = calls.iter().position(|call| *call == "terminate").unwrap();
    let launch = calls.iter().position(|call| *call == "launch").unwrap();
    assert!(terminate < launch);
    assert_eq!(app.count("terminate"), 1);
    assert_eq!(app.count("launch"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_conductor_that_is_not_running_is_only_launched() {
    let app = FakeApp::new(false, Some(0), Some(0));
    let answer = run(&app, 0, false, false).await;
    assert_eq!(
        answer,
        WriteAnswer::json(200, json!({ "ok": true, "ms": 0 }))
    );
    assert_eq!(app.count("terminate"), 0);
    assert_eq!(app.count("launch"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_quit_and_a_window_at_the_last_moment_still_count() {
    // The looks at 15 s and at 30 s see what they wait for: every wait checks before it gives up.
    let app = FakeApp::new(true, Some(60), Some(120));
    let answer = run(&app, 0, false, false).await;
    assert_eq!(
        answer,
        WriteAnswer::json(200, json!({ "ok": true, "ms": 45_000 }))
    );
}

#[tokio::test(start_paused = true)]
async fn stop_agents_lets_a_restart_through_with_chats_mid_turn() {
    let app = FakeApp::new(true, Some(0), Some(0));
    let answer = run(&app, 4, false, true).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body["ok"], json!(true));
    assert_eq!(app.count("terminate"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_app_is_asked_off_the_async_thread() {
    let app = FakeApp::new(true, Some(1), Some(1));
    let here = std::thread::current().id();
    assert_eq!(run(&app, 0, false, false).await.status, 200);
    let threads = app.threads();
    assert!(!threads.is_empty());
    assert!(threads.iter().all(|thread| *thread != here));
}

// 409: locked.

#[tokio::test(start_paused = true)]
async fn a_locked_mac_is_refused_without_touching_the_app() {
    let app = FakeApp::new(true, Some(0), Some(0));
    let answer = run(&app, 0, true, false).await;
    assert_eq!(
        answer,
        failure(
            409,
            "The Mac is locked - unlock it before restarting Conductor."
        )
    );
    assert!(app.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_lock_comes_before_the_running_chats_and_stop_agents_does_not_lift_it() {
    let app = FakeApp::new(true, Some(0), Some(0));
    let expected = failure(
        409,
        "The Mac is locked - unlock it before restarting Conductor.",
    );
    assert_eq!(run(&app, 2, true, false).await, expected);
    assert_eq!(run(&app, 2, true, true).await, expected);
    assert!(app.calls().is_empty());
}

// 409: chats mid-turn.

#[tokio::test(start_paused = true)]
async fn one_chat_mid_turn_is_refused_in_the_singular() {
    let app = FakeApp::new(true, Some(0), Some(0));
    let answer = run(&app, 1, false, false).await;
    assert_eq!(
        answer,
        WriteAnswer::json(
            409,
            json!({
                "ok": false,
                "agentsRunning": true,
                "working": 1,
                "error": "1 chat is mid-turn. Restarting Conductor ends it.",
            })
        )
    );
    assert!(app.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn several_chats_mid_turn_are_refused_in_the_plural() {
    let app = FakeApp::new(true, Some(0), Some(0));
    let answer = run(&app, 3, false, false).await;
    assert_eq!(
        answer,
        WriteAnswer::json(
            409,
            json!({
                "ok": false,
                "agentsRunning": true,
                "working": 3,
                "error": "3 chats are mid-turn. Restarting Conductor ends them.",
            })
        )
    );
    assert!(app.calls().is_empty());
}

// 502: did not quit.

#[tokio::test(start_paused = true)]
async fn a_conductor_that_will_not_quit_gives_up_after_fifteen_seconds() {
    let app = FakeApp::new(true, None, Some(0));
    let started = Instant::now();
    let answer = run(&app, 0, false, false).await;
    assert_eq!(
        answer,
        failure(
            502,
            "Conductor did not quit — quit it on your Mac and try again."
        )
    );
    assert_eq!(started.elapsed(), Duration::from_secs(15));
    assert_eq!(app.count("terminate"), 1);
    assert_eq!(app.count("launch"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_quit_one_look_too_late_is_a_failure() {
    // Gone only at 15.25 s; the last look is at 15 s.
    let app = FakeApp::new(true, Some(61), Some(0));
    let answer = run(&app, 0, false, false).await;
    assert_eq!(answer.status, 502);
    assert_eq!(app.count("launch"), 0);
}

// 502: launch failed.

#[tokio::test(start_paused = true)]
async fn a_launch_error_is_passed_on_with_its_text() {
    let app = FakeApp::failing_launch("could not launch Conductor: no such app");
    let answer = run(&app, 0, false, false).await;
    assert_eq!(
        answer,
        failure(502, "could not launch Conductor: no such app")
    );
    assert_eq!(app.count("has_window"), 0);
}

// 502: no window.

#[tokio::test(start_paused = true)]
async fn a_relaunch_without_a_window_gives_up_after_thirty_seconds() {
    let app = FakeApp::new(true, Some(0), None);
    let started = Instant::now();
    let answer = run(&app, 0, false, false).await;
    assert_eq!(
        answer,
        failure(502, "Conductor started but showed no window in time.")
    );
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    // The launch, and the two asks for a window at 3 s and 10 s.
    assert_eq!(app.count("launch"), 3);
}

#[tokio::test(start_paused = true)]
async fn a_slow_window_is_asked_for_again() {
    // 48 looks without a window (at 0, 250, ..., 11 750 ms); the first to see one is at 12 s.
    let app = FakeApp::new(false, Some(0), Some(48));
    let started = Instant::now();
    let answer = run(&app, 0, false, false).await;
    assert_eq!(
        answer,
        WriteAnswer::json(200, json!({ "ok": true, "ms": 12_000 }))
    );
    assert_eq!(app.count("launch"), 3);
    let moments: Vec<Duration> = app
        .launches()
        .iter()
        .map(|launch| launch.duration_since(started))
        .collect();
    assert_eq!(
        moments,
        [
            Duration::ZERO,
            Duration::from_secs(3),
            Duration::from_secs(10)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_window_one_look_too_late_is_a_failure() {
    // First true look at 30.25 s; the last look is at 30 s.
    let app = FakeApp::new(false, Some(0), Some(121));
    let answer = run(&app, 0, false, false).await;
    assert_eq!(
        answer,
        failure(502, "Conductor started but showed no window in time.")
    );
}

// The waits follow the timings they are given.

#[tokio::test(start_paused = true)]
async fn the_waits_use_the_given_timings() {
    let timings = RestartTimings {
        quit_wait: Duration::from_secs(2),
        window_wait: Duration::from_secs(3),
        poll: Duration::from_secs(1),
    };
    let stuck = FakeApp::new(true, None, Some(0));
    let started = Instant::now();
    let answer = restart(stuck.clone(), 0, false, false, timings).await;
    assert_eq!(answer.status, 502);
    assert_eq!(started.elapsed(), Duration::from_secs(2));
    // One look to decide to quit, then looks at 0 s, 1 s and 2 s.
    assert_eq!(stuck.count("running"), 1 + 3);

    let blind = FakeApp::new(false, Some(0), None);
    let started = Instant::now();
    let answer = restart(blind.clone(), 0, false, false, timings).await;
    assert_eq!(answer.status, 502);
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    assert_eq!(blind.count("has_window"), 4);
}
