//! The send loop over a fake UI run and a fake receipt read, in paused time.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conductor_remote::delivery::deliver::{
    deliver, send_budget, AttemptError, Delivery, DeliveryTimings,
};
use conductor_remote::reads::receipts::Receipt;
use tokio::time::Instant;

/// One scripted run of the fake UI.
#[derive(Clone)]
struct Step {
    result: Result<(), AttemptError>,
    /// The receipt shows this long after the run returns.
    lands_after: Option<Duration>,
}

fn ok() -> Step {
    Step {
        result: Ok(()),
        lands_after: None,
    }
}

fn failed(message: &str) -> AttemptError {
    AttemptError {
        message: message.to_owned(),
        sent_nothing: false,
        terminal: false,
        lock: false,
    }
}

fn err(error: AttemptError) -> Step {
    Step {
        result: Err(error),
        lands_after: None,
    }
}

fn lands(mut step: Step, after: Duration) -> Step {
    step.lands_after = Some(after);
    step
}

#[derive(Default)]
struct Fake {
    /// The runs still to play; an empty script returns `Ok` and never lands.
    script: VecDeque<Step>,
    /// When each run started.
    starts: Vec<Instant>,
    /// The deadline each run was given.
    deadlines: Vec<Instant>,
    /// When each read happened.
    probes: Vec<Instant>,
    /// The receipt shows to reads at or after this time.
    landed_at: Option<Instant>,
    /// The receipt shows from this read on (1-based), whatever the time.
    lands_on_probe: Option<usize>,
}

fn receipt() -> Receipt {
    Receipt::Outbox {
        id: "outbox-1".to_owned(),
    }
}

fn fake(script: Vec<Step>) -> Arc<Mutex<Fake>> {
    Arc::new(Mutex::new(Fake {
        script: script.into(),
        ..Fake::default()
    }))
}

async fn run(state: &Arc<Mutex<Fake>>, budget: Duration) -> Delivery {
    let attempt_state = Arc::clone(state);
    let probe_state = Arc::clone(state);
    deliver(
        move |deadline| {
            let state = Arc::clone(&attempt_state);
            async move {
                let mut fake = state.lock().unwrap();
                let now = Instant::now();
                fake.starts.push(now);
                fake.deadlines.push(deadline);
                let step = fake.script.pop_front().unwrap_or_else(ok);
                if let Some(after) = step.lands_after {
                    fake.landed_at = Some(now + after);
                }
                step.result
            }
        },
        move || {
            let state = Arc::clone(&probe_state);
            async move {
                let mut fake = state.lock().unwrap();
                let now = Instant::now();
                fake.probes.push(now);
                let by_time = fake.landed_at.is_some_and(|at| now >= at);
                let by_count = fake.lands_on_probe.is_some_and(|n| fake.probes.len() >= n);
                (by_time || by_count).then(receipt)
            }
        },
        budget,
        DeliveryTimings::default(),
    )
    .await
}

const UNCONFIRMED: &str =
    "Could not confirm the sent message in Conductor. Check the chat before trying again.";

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[tokio::test(start_paused = true)]
async fn delivered_before_any_attempt() {
    let state = fake(vec![]);
    let start = Instant::now();
    state.lock().unwrap().landed_at = Some(start);

    let delivery = run(&state, secs(20)).await;

    assert_eq!(
        delivery,
        Delivery {
            attempts: 0,
            receipt: Some(receipt()),
            error: None,
            locked: false,
        }
    );
    let fake = state.lock().unwrap();
    assert!(fake.starts.is_empty(), "the attempt must never run");
    assert_eq!(fake.probes, vec![start]);
}

#[tokio::test(start_paused = true)]
async fn delivered_on_the_first_run_while_watching() {
    let state = fake(vec![lands(ok(), secs(1))]);
    let start = Instant::now();

    let delivery = run(&state, secs(20)).await;

    assert_eq!(delivery.attempts, 1);
    assert_eq!(delivery.receipt, Some(receipt()));
    assert_eq!(delivery.error, None);
    assert!(!delivery.locked);
    let fake = state.lock().unwrap();
    assert_eq!(fake.starts, vec![start]);
    // The top check, then reads at 0, 0.3, 0.6, 0.9 and 1.2 s: the fifth window read finds it.
    assert_eq!(fake.probes.len(), 6);
    assert_eq!(*fake.probes.last().unwrap(), start + millis(1200));
    assert_eq!(Instant::now(), start + millis(1200));
}

#[tokio::test(start_paused = true)]
async fn a_late_landing_is_found_by_the_top_check_and_not_run_again() {
    let state = fake(vec![]);
    let start = Instant::now();
    // The top check, 21 window reads (0 to 6 s every 300 ms), then the top check of the next
    // round: the receipt shows only to that read, after the window closed.
    state.lock().unwrap().lands_on_probe = Some(23);

    let delivery = run(&state, secs(55)).await;

    assert_eq!(delivery.attempts, 1);
    assert_eq!(delivery.receipt, Some(receipt()));
    let fake = state.lock().unwrap();
    assert_eq!(
        fake.starts.len(),
        1,
        "a landed prompt must not be typed again"
    );
    assert_eq!(fake.probes.len(), 23);
    assert_eq!(
        fake.probes[21],
        start + secs(6),
        "the window ends with a read"
    );
    assert_eq!(fake.probes[22], start + secs(6));
}

#[tokio::test(start_paused = true)]
async fn sent_nothing_gets_one_read_then_the_retry_pause() {
    let busy = AttemptError {
        sent_nothing: true,
        ..failed("Conductor is busy")
    };
    let state = fake(vec![err(busy), lands(ok(), Duration::ZERO)]);
    let start = Instant::now();

    let delivery = run(&state, secs(20)).await;

    assert_eq!(delivery.attempts, 2);
    assert_eq!(delivery.receipt, Some(receipt()));
    let fake = state.lock().unwrap();
    assert_eq!(fake.starts, vec![start, start + secs(1)]);
    // Top check and the single read of the first run at 0 s (no window), the pause, then the
    // top check and the first window read of the second run at 1 s.
    assert_eq!(
        fake.probes,
        vec![start, start, start + secs(1), start + secs(1)]
    );
}

#[tokio::test(start_paused = true)]
async fn sent_nothing_found_by_its_single_read() {
    let busy = AttemptError {
        sent_nothing: true,
        ..failed("Conductor is busy")
    };
    let state = fake(vec![lands(err(busy), Duration::ZERO)]);
    let start = Instant::now();

    let delivery = run(&state, secs(20)).await;

    assert_eq!(delivery.attempts, 1);
    assert_eq!(delivery.receipt, Some(receipt()));
    let fake = state.lock().unwrap();
    assert_eq!(fake.probes, vec![start, start]);
}

#[tokio::test(start_paused = true)]
async fn a_terminal_error_stops_after_one_run() {
    let terminal = AttemptError {
        terminal: true,
        ..failed("The chat is archived")
    };
    let state = fake(vec![err(terminal)]);
    let start = Instant::now();

    let delivery = run(&state, secs(55)).await;

    assert_eq!(
        delivery,
        Delivery {
            attempts: 1,
            receipt: None,
            error: Some("The chat is archived".to_owned()),
            locked: false,
        }
    );
    assert_eq!(state.lock().unwrap().starts.len(), 1);
    // Something may have been typed, so the run was still watched for its receipt.
    assert_eq!(Instant::now(), start + secs(6));
}

#[tokio::test(start_paused = true)]
async fn a_lock_stops_after_one_run() {
    let locked = AttemptError {
        lock: true,
        sent_nothing: true,
        ..failed("The Mac is locked")
    };
    let state = fake(vec![err(locked)]);
    let start = Instant::now();

    let delivery = run(&state, secs(55)).await;

    assert_eq!(
        delivery,
        Delivery {
            attempts: 1,
            receipt: None,
            error: Some("The Mac is locked".to_owned()),
            locked: true,
        }
    );
    let fake = state.lock().unwrap();
    assert_eq!(fake.starts.len(), 1);
    assert_eq!(fake.probes, vec![start, start]);
}

#[tokio::test(start_paused = true)]
async fn retries_until_the_budget_is_too_small() {
    let state = fake(vec![]);
    let start = Instant::now();

    let delivery = run(&state, secs(55)).await;

    assert_eq!(delivery.attempts, 7);
    assert_eq!(delivery.receipt, None);
    assert_eq!(
        delivery.error.as_deref(),
        Some(
            "Could not confirm the sent message in Conductor (tried 7\u{00d7}). \
             Check the chat before trying again."
        )
    );
    let fake = state.lock().unwrap();
    assert_eq!(fake.deadlines, vec![start + secs(53); 7]);
    let expected: Vec<Instant> = (0..7).map(|run| start + secs(6 * run)).collect();
    assert_eq!(fake.starts, expected);
    // The seventh window closes at 42 s, leaving 13 s, less than 12 s + 6 s.
    assert_eq!(Instant::now(), start + secs(42));
}

#[tokio::test(start_paused = true)]
async fn the_unconfirmed_text_after_one_run() {
    let state = fake(vec![]);

    let delivery = run(&state, secs(18)).await;

    assert_eq!(delivery.attempts, 1);
    assert_eq!(delivery.error.as_deref(), Some(UNCONFIRMED));
    assert!(!delivery.locked);
}

#[tokio::test(start_paused = true)]
async fn the_unconfirmed_text_when_the_last_run_returned_ok() {
    let state = fake(vec![err(failed("The composer was not found")), ok()]);

    let delivery = run(&state, secs(25)).await;

    assert_eq!(delivery.attempts, 2);
    assert_eq!(
        delivery.error.as_deref(),
        Some(
            "Could not confirm the sent message in Conductor (tried 2\u{00d7}). \
             Check the chat before trying again."
        )
    );
}

#[tokio::test(start_paused = true)]
async fn the_last_message_text_after_one_run() {
    let state = fake(vec![err(failed("The composer was not found"))]);

    let delivery = run(&state, secs(18)).await;

    assert_eq!(delivery.attempts, 1);
    assert_eq!(
        delivery.error.as_deref(),
        Some("The composer was not found")
    );
}

#[tokio::test(start_paused = true)]
async fn the_last_message_text_names_the_tries() {
    let state = fake(vec![
        err(failed("The composer was not found")),
        err(failed("The send button stayed disabled")),
    ]);

    let delivery = run(&state, secs(25)).await;

    assert_eq!(delivery.attempts, 2);
    assert_eq!(
        delivery.error.as_deref(),
        Some("The send button stayed disabled (tried 2\u{00d7})")
    );
    assert!(!delivery.locked);
}

#[test]
fn send_budget_follows_the_client_timeout() {
    assert_eq!(send_budget(None), secs(20));
    assert_eq!(send_budget(Some(0)), secs(20));
    assert_eq!(send_budget(Some(10_000)), secs(18));
    assert_eq!(send_budget(Some(30_000)), secs(25));
    assert_eq!(send_budget(Some(75_000)), secs(55));
    assert_eq!(send_budget(Some(200_000)), secs(55));
}

#[test]
fn default_timings() {
    assert_eq!(
        DeliveryTimings::default(),
        DeliveryTimings {
            confirm_window: secs(6),
            poll: millis(300),
            min_attempt: secs(12),
            min_confirm: secs(2),
            retry_pause: secs(1),
        }
    );
}
