//! The UI thread: ordering, the bounded queue, panics, dropped callers and the thread's own
//! identity. The driver is a fake that records its calls; nothing here reaches the Mac.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use conductor_remote::contract::Priority;
use conductor_remote::ui::actor::{UiActor, UiHandle, UiRunError, MAX_WAITING};
use conductor_remote::ui::driver::{Target, UiDriver, UiError, ViewReport};

type Calls = Arc<Mutex<Vec<String>>>;

/// Records every call. `new_chat` on the workspace "hold" blocks until the gate is released.
struct FakeDriver {
    calls: Calls,
    gate: mpsc::Receiver<()>,
}

impl UiDriver for FakeDriver {
    fn trusted(&self) -> bool {
        true
    }

    fn send_prompt(&mut self, target: &Target, text: &str, queue: bool) -> Result<u32, UiError> {
        self.calls.lock().unwrap().push(format!(
            "send_prompt:{}:{text}:{queue}",
            target.workspace_id
        ));
        Ok(1)
    }

    fn stop_turn(&mut self, target: &Target) -> Result<(), UiError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("stop_turn:{}", target.workspace_id));
        Ok(())
    }

    fn new_chat(&mut self, target: &Target) -> Result<(), UiError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("new_chat:{}", target.workspace_id));
        if target.workspace_id == "hold" {
            let _ = self.gate.recv();
        }
        Ok(())
    }

    fn locate(&mut self) -> Result<ViewReport, UiError> {
        self.calls.lock().unwrap().push("locate".to_owned());
        Ok(ViewReport::default())
    }

    fn open_link(&mut self, url: &str) -> Result<(), UiError> {
        self.calls.lock().unwrap().push(format!("open_link:{url}"));
        Ok(())
    }
}

impl Drop for FakeDriver {
    fn drop(&mut self) {
        self.calls.lock().unwrap().push("driver dropped".to_owned());
    }
}

fn target(workspace_id: &str) -> Target {
    Target {
        workspace_id: workspace_id.to_owned(),
        session_id: None,
        repo: None,
        branch: String::new(),
        workspace_name: None,
        tab: None,
    }
}

/// A job that records `label` as a `stop_turn` call.
fn mark(label: &'static str) -> impl FnOnce(&mut dyn UiDriver) + Send + 'static {
    move |driver| {
        driver.stop_turn(&target(label)).unwrap();
    }
}

struct Rig {
    handle: UiHandle,
    calls: Calls,
    release: mpsc::Sender<()>,
}

fn rig() -> Rig {
    let calls: Calls = Arc::default();
    let (release, gate) = mpsc::channel();
    let factory_calls = Arc::clone(&calls);
    let handle = UiActor::spawn(move || {
        Box::new(FakeDriver {
            calls: factory_calls,
            gate,
        })
    });
    Rig {
        handle,
        calls,
        release,
    }
}

impl Rig {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Holds the UI thread inside a running job; returns once that job has started, so the
    /// jobs queued next are certainly waiting.
    async fn hold(&self) -> impl std::future::Future<Output = Result<(), UiRunError>> {
        let (started_tx, started_rx) = mpsc::channel();
        let held = self.handle.run(Priority::Interactive, move |driver| {
            started_tx.send(()).unwrap();
            driver.new_chat(&target("hold")).unwrap();
        });
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("the holding job never started");
        held
    }

    fn release(&self) {
        self.release.send(()).unwrap();
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[tokio::test]
async fn jobs_run_in_arrival_order() {
    let rig = rig();
    let held = rig.hold().await;
    let first = rig.handle.run(Priority::Interactive, mark("a"));
    let second = rig.handle.run(Priority::Interactive, mark("b"));
    let third = rig.handle.run(Priority::Interactive, mark("c"));
    assert_eq!(rig.handle.waiting(), 3);
    rig.release();
    held.await.unwrap();
    first.await.unwrap();
    second.await.unwrap();
    third.await.unwrap();
    assert_eq!(
        rig.calls(),
        ["new_chat:hold", "stop_turn:a", "stop_turn:b", "stop_turn:c"]
    );
}

#[tokio::test]
async fn jobs_return_their_value() {
    let rig = rig();
    let report = rig
        .handle
        .run(Priority::Background, |driver| driver.locate().unwrap())
        .await
        .unwrap();
    assert_eq!(report, ViewReport::default());
    let trusted = rig
        .handle
        .run(Priority::Interactive, |driver| driver.trusted())
        .await
        .unwrap();
    assert!(trusted);
}

#[tokio::test]
async fn interactive_runs_before_background_and_each_priority_keeps_its_order() {
    let rig = rig();
    let held = rig.hold().await;
    let background_one = rig.handle.run(Priority::Background, mark("background 1"));
    let background_two = rig.handle.run(Priority::Background, mark("background 2"));
    let interactive_one = rig.handle.run(Priority::Interactive, mark("interactive 1"));
    let interactive_two = rig.handle.run(Priority::Interactive, mark("interactive 2"));
    assert_eq!(rig.handle.waiting(), 4);
    rig.release();
    held.await.unwrap();
    background_one.await.unwrap();
    background_two.await.unwrap();
    interactive_one.await.unwrap();
    interactive_two.await.unwrap();
    assert_eq!(
        rig.calls(),
        [
            "new_chat:hold",
            "stop_turn:interactive 1",
            "stop_turn:interactive 2",
            "stop_turn:background 1",
            "stop_turn:background 2",
        ]
    );
}

#[tokio::test]
async fn a_fifth_waiter_is_refused_and_the_queue_still_runs_the_four() {
    let rig = rig();
    let held = rig.hold().await;
    let labels = ["one", "two", "three", "four"];
    assert_eq!(labels.len(), MAX_WAITING);
    let queued: Vec<_> = labels
        .iter()
        .map(|label| rig.handle.run(Priority::Background, mark(label)))
        .collect();
    assert_eq!(rig.handle.waiting(), 4);

    let refused = rig
        .handle
        .run(Priority::Interactive, mark("five"))
        .await
        .unwrap_err();
    assert_eq!(refused, UiRunError::Busy { waiting: 4 });
    assert_eq!(
        refused.to_string(),
        "Conductor's UI is busy — 4 operation(s) already queued. Try again shortly."
    );
    assert_eq!(rig.handle.waiting(), 4, "a refused job is not queued");

    rig.release();
    held.await.unwrap();
    for job in queued {
        job.await.unwrap();
    }
    assert_eq!(
        rig.calls(),
        [
            "new_chat:hold",
            "stop_turn:one",
            "stop_turn:two",
            "stop_turn:three",
            "stop_turn:four"
        ]
    );
}

#[tokio::test]
async fn the_running_job_is_not_counted_as_waiting() {
    let rig = rig();
    assert_eq!(rig.handle.waiting(), 0);
    let held = rig.hold().await;
    assert_eq!(rig.handle.waiting(), 0);
    let first = rig.handle.run(Priority::Background, mark("a"));
    assert_eq!(rig.handle.waiting(), 1);
    let second = rig.handle.run(Priority::Interactive, mark("b"));
    assert_eq!(rig.handle.waiting(), 2);
    rig.release();
    held.await.unwrap();
    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(rig.handle.waiting(), 0);
}

#[tokio::test]
async fn a_panicking_job_answers_crashed_and_the_next_job_runs() {
    let rig = rig();
    let held = rig.hold().await;
    let panicking = rig.handle.run(Priority::Interactive, |_driver| -> u32 {
        panic!("the job went wrong");
    });
    let next = rig.handle.run(Priority::Interactive, |driver| {
        driver.stop_turn(&target("after")).unwrap();
        7
    });
    rig.release();
    held.await.unwrap();
    assert_eq!(panicking.await, Err(UiRunError::Crashed));
    assert_eq!(next.await, Ok(7));
    assert_eq!(rig.calls(), ["new_chat:hold", "stop_turn:after"]);

    let later = rig
        .handle
        .run(Priority::Background, |driver| driver.trusted())
        .await;
    assert_eq!(later, Ok(true), "the thread outlives the panic");
}

#[tokio::test]
async fn a_dropped_caller_does_not_cancel_its_job() {
    let rig = rig();
    let held = rig.hold().await;
    let abandoned = rig.handle.run(Priority::Interactive, mark("abandoned"));
    drop(abandoned);
    assert_eq!(rig.handle.waiting(), 1);
    let kept = rig.handle.run(Priority::Interactive, mark("kept"));
    rig.release();
    held.await.unwrap();
    kept.await.unwrap();
    assert_eq!(
        rig.calls(),
        ["new_chat:hold", "stop_turn:abandoned", "stop_turn:kept"]
    );
}

#[tokio::test]
async fn the_driver_is_made_and_used_on_the_ui_thread() {
    let factory_thread: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = Arc::clone(&factory_thread);
    let (_release, gate) = mpsc::channel();
    let handle = UiActor::spawn(move || {
        *seen.lock().unwrap() = std::thread::current().name().map(str::to_owned);
        Box::new(FakeDriver {
            calls: Arc::default(),
            gate,
        })
    });
    let job_thread = handle
        .run(Priority::Interactive, |_driver| {
            std::thread::current().name().map(str::to_owned)
        })
        .await
        .unwrap();
    assert_eq!(job_thread.as_deref(), Some("conductor-remote-ui"));
    assert_eq!(
        factory_thread.lock().unwrap().as_deref(),
        Some("conductor-remote-ui")
    );
}

#[tokio::test]
async fn a_factory_panic_makes_every_run_answer_crashed() {
    let handle = UiActor::spawn(|| panic!("no driver today"));
    for priority in [Priority::Interactive, Priority::Background] {
        assert_eq!(
            handle.run(priority, |driver| driver.trusted()).await,
            Err(UiRunError::Crashed)
        );
    }
    assert_eq!(handle.waiting(), 0);
}

#[tokio::test]
async fn the_thread_ends_when_every_handle_is_dropped_and_the_queue_is_empty() {
    let rig = rig();
    let held = rig.hold().await;
    let queued = rig.handle.run(Priority::Background, mark("last"));
    let clone = rig.handle.clone();
    let Rig {
        handle,
        calls,
        release,
    } = rig;
    drop(handle);
    drop(clone);
    // The queue is not empty yet: the thread must still run both jobs.
    release.send(()).unwrap();
    held.await.unwrap();
    queued.await.unwrap();
    wait_until("the UI thread to end", || {
        calls.lock().unwrap().last().map(String::as_str) == Some("driver dropped")
    });
    assert_eq!(
        *calls.lock().unwrap(),
        ["new_chat:hold", "stop_turn:last", "driver dropped"]
    );
}
