//! The send loop: receipts, retries and the budget.
//!
//! [`deliver`] runs the UI through a closure and looks for the receipt through another, so it
//! knows neither the window nor the database; tests pass fakes for both.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::reads::receipts::Receipt;

/// The waits of one send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryTimings {
    /// How long a run is watched for its receipt.
    pub confirm_window: Duration,
    /// How often the receipt is checked while watching.
    pub poll: Duration,
    /// Another run starts only with at least this plus `confirm_window` left.
    pub min_attempt: Duration,
    /// Kept back from each run's deadline for its confirm.
    pub min_confirm: Duration,
    /// The wait after a run that sent nothing and found no receipt (a busy UI answers at once).
    pub retry_pause: Duration,
}

impl Default for DeliveryTimings {
    fn default() -> DeliveryTimings {
        DeliveryTimings {
            confirm_window: Duration::from_secs(6),
            poll: Duration::from_millis(300),
            min_attempt: Duration::from_secs(12),
            min_confirm: Duration::from_secs(2),
            retry_pause: Duration::from_secs(1),
        }
    }
}

/// The budget when the phone names none.
const DEFAULT_BUDGET: Duration = Duration::from_secs(20);
/// Kept back from the phone's own timeout so the answer reaches it in time.
const CLIENT_MARGIN: Duration = Duration::from_secs(5);
const MIN_BUDGET: Duration = Duration::from_secs(18);
const MAX_BUDGET: Duration = Duration::from_secs(55);

/// The budget of one send: `None` or 0 → 20 s; otherwise `asked − 5 s` (saturating), at least 18 s and at most 55 s.
pub fn send_budget(client_timeout_ms: Option<u64>) -> Duration {
    match client_timeout_ms {
        None | Some(0) => DEFAULT_BUDGET,
        Some(ms) => Duration::from_millis(ms)
            .saturating_sub(CLIENT_MARGIN)
            .clamp(MIN_BUDGET, MAX_BUDGET),
    }
}

/// Why one run failed, as the loop needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptError {
    pub message: String,
    /// Nothing was typed or pressed: check for a receipt once, without the confirm window.
    pub sent_nothing: bool,
    /// Another run in this request cannot help.
    pub terminal: bool,
    /// The Mac is locked.
    pub lock: bool,
}

/// The outcome of one send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub attempts: u32,
    /// `Some` exactly when the prompt was delivered.
    pub receipt: Option<Receipt>,
    /// The phone's text when it was not delivered.
    pub error: Option<String>,
    /// The last run failed because the Mac is locked.
    pub locked: bool,
}

impl Delivery {
    fn delivered(attempts: u32, receipt: Receipt) -> Delivery {
        Delivery {
            attempts,
            receipt: Some(receipt),
            error: None,
            locked: false,
        }
    }
}

/// `attempt(deadline)` runs the UI once and must finish before `deadline`; `probe()` looks for
/// the receipt (a failed read is `None`).
///
/// The loop, with `deadline = start + budget`:
/// 1. `probe()`; a receipt → delivered (attempts may be 0).
/// 2. One more run: `attempt(deadline − min_confirm)`.
/// 3. A run that sent nothing gets one `probe()`; any other run is watched: `probe()` every
///    `poll` until `min(now + confirm_window, deadline)`, ending with a check. A receipt →
///    delivered.
/// 4. A terminal or lock error stops.
/// 5. Less than `min_attempt + confirm_window` left stops; after a run that sent nothing, wait
///    `retry_pause`; back to 1, whose check keeps a late landing from being typed again.
pub async fn deliver<A, AF, P, PF>(
    mut attempt: A,
    mut probe: P,
    budget: Duration,
    timings: DeliveryTimings,
) -> Delivery
where
    A: FnMut(Instant) -> AF,
    AF: Future<Output = Result<(), AttemptError>>,
    P: FnMut() -> PF,
    PF: Future<Output = Option<Receipt>>,
{
    let start = Instant::now();
    let deadline = start + budget;
    let run_deadline = deadline.checked_sub(timings.min_confirm).unwrap_or(start);
    let mut attempts: u32 = 0;
    let mut last: Result<(), AttemptError>;

    loop {
        if let Some(receipt) = probe().await {
            return Delivery::delivered(attempts, receipt);
        }

        attempts += 1;
        last = attempt(run_deadline).await;
        let sent_nothing = matches!(&last, Err(error) if error.sent_nothing);

        if sent_nothing {
            if let Some(receipt) = probe().await {
                return Delivery::delivered(attempts, receipt);
            }
        } else {
            let until = (Instant::now() + timings.confirm_window).min(deadline);
            loop {
                if let Some(receipt) = probe().await {
                    return Delivery::delivered(attempts, receipt);
                }
                let now = Instant::now();
                if now >= until {
                    break;
                }
                tokio::time::sleep_until((now + timings.poll).min(until)).await;
            }
        }

        if let Err(error) = &last {
            if error.terminal || error.lock {
                break;
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left < timings.min_attempt + timings.confirm_window {
            break;
        }
        if sent_nothing {
            tokio::time::sleep(timings.retry_pause).await;
        }
    }

    let tried = if attempts > 1 {
        format!(" (tried {attempts}\u{00d7})")
    } else {
        String::new()
    };
    let (error, locked) = match last {
        Ok(()) => (
            format!(
                "Could not confirm the sent message in Conductor{tried}. \
                 Check the chat before trying again."
            ),
            false,
        ),
        Err(error) => (format!("{}{tried}", error.message), error.lock),
    };
    Delivery {
        attempts,
        receipt: None,
        error: Some(error),
        locked,
    }
}
