//! The UI thread: one owner of every Accessibility call, fed through a bounded priority queue.
//!
//! The driver is not `Send`, so it is made on the thread and never leaves it. Async code holds a
//! `UiHandle`, queues a job with `run`, and awaits the result. One job runs at a time and a
//! running job is never interrupted; the jobs that wait run `Interactive` before `Background`,
//! first in first out within a priority.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use tokio::sync::oneshot;

use super::driver::UiDriver;
use crate::contract::Priority;

/// At most this many jobs wait behind the running one; the next caller is refused.
pub const MAX_WAITING: usize = 4;

/// The name of the UI thread.
const THREAD_NAME: &str = "conductor-remote-ui";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UiRunError {
    #[error("Conductor's UI is busy — {waiting} operation(s) already queued. Try again shortly.")]
    Busy { waiting: usize },
    #[error("the relay's UI thread failed - try again")]
    Crashed,
}

/// A queued job, already wrapped so that it catches its own panic and answers its caller.
type Job = Box<dyn FnOnce(&mut dyn UiDriver) + Send + 'static>;

#[derive(Default)]
struct State {
    interactive: VecDeque<Job>,
    background: VecDeque<Job>,
    /// Every handle is gone: the thread ends once the queue is empty.
    closed: bool,
    /// The thread is gone (or never started) without a driver: nothing will run again.
    crashed: bool,
}

impl State {
    fn waiting(&self) -> usize {
        self.interactive.len() + self.background.len()
    }

    fn next(&mut self) -> Option<Job> {
        self.interactive
            .pop_front()
            .or_else(|| self.background.pop_front())
    }
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    /// Jobs run outside the lock, so a poisoned lock holds consistent data.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Nothing will run again: refuses every later job and drops the queued ones, which answers
    /// their callers `Crashed`. The jobs are dropped outside the lock, since dropping one may
    /// drop a handle, and that locks.
    fn mark_crashed(&self) {
        let (interactive, background) = {
            let mut state = self.lock();
            state.crashed = true;
            (
                std::mem::take(&mut state.interactive),
                std::mem::take(&mut state.background),
            )
        };
        drop(interactive);
        drop(background);
    }
}

/// Closes the queue when the last handle is dropped.
struct CloseOnDrop(Arc<Shared>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.lock().closed = true;
        self.0.wake.notify_all();
    }
}

pub struct UiActor;

impl UiActor {
    /// Starts the thread "conductor-remote-ui", makes the driver on it with `factory`, and
    /// returns the handle. The thread ends when every handle is dropped and the queue is empty.
    pub fn spawn<F>(factory: F) -> UiHandle
    where
        F: FnOnce() -> Box<dyn UiDriver> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let started = std::thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || serve(&thread_shared, factory));
        if let Err(error) = started {
            tracing::error!(%error, "could not start the UI thread");
            shared.mark_crashed();
        }
        UiHandle {
            shared: Arc::clone(&shared),
            _close: Arc::new(CloseOnDrop(shared)),
        }
    }
}

/// The body of the UI thread.
fn serve<F>(shared: &Shared, factory: F)
where
    F: FnOnce() -> Box<dyn UiDriver>,
{
    let mut driver = match catch_unwind(AssertUnwindSafe(factory)) {
        Ok(driver) => driver,
        Err(payload) => {
            tracing::error!(
                panic = %panic_message(payload.as_ref()),
                "the UI driver factory panicked"
            );
            shared.mark_crashed();
            return;
        }
    };
    loop {
        let job = {
            let mut state = shared.lock();
            loop {
                if let Some(job) = state.next() {
                    break Some(job);
                }
                if state.closed {
                    break None;
                }
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        match job {
            Some(job) => job(driver.as_mut()),
            None => return,
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[derive(Clone)]
pub struct UiHandle {
    shared: Arc<Shared>,
    /// Shared by every clone; its drop closes the queue.
    _close: Arc<CloseOnDrop>,
}

impl UiHandle {
    /// Queues `job`; it runs on the UI thread with the driver. Refused at once with `Busy`
    /// when `MAX_WAITING` jobs already wait. A panicking job answers `Crashed` and the
    /// thread goes on with the next job.
    pub fn run<R, J>(
        &self,
        priority: Priority,
        job: J,
    ) -> impl Future<Output = Result<R, UiRunError>> + Send + 'static
    where
        R: Send + 'static,
        J: FnOnce(&mut dyn UiDriver) -> R + Send + 'static,
    {
        let queued = self.enqueue(priority, job);
        async move {
            match queued {
                Err(error) => Err(error),
                // The sender is dropped unanswered only when the thread is gone.
                Ok(answer) => answer.await.unwrap_or(Err(UiRunError::Crashed)),
            }
        }
    }

    /// How many jobs wait now (the running one not counted).
    pub fn waiting(&self) -> usize {
        self.shared.lock().waiting()
    }

    fn enqueue<R, J>(
        &self,
        priority: Priority,
        job: J,
    ) -> Result<oneshot::Receiver<Result<R, UiRunError>>, UiRunError>
    where
        R: Send + 'static,
        J: FnOnce(&mut dyn UiDriver) -> R + Send + 'static,
    {
        let (reply, answer) = oneshot::channel();
        let wrapped: Job = Box::new(move |driver: &mut dyn UiDriver| {
            let outcome = match catch_unwind(AssertUnwindSafe(|| job(driver))) {
                Ok(value) => Ok(value),
                Err(payload) => {
                    tracing::error!(
                        panic = %panic_message(payload.as_ref()),
                        "a UI job panicked"
                    );
                    Err(UiRunError::Crashed)
                }
            };
            // The caller may have dropped its future; the job has run and the result is unwanted.
            let _ = reply.send(outcome);
        });

        let mut state = self.shared.lock();
        let refusal = if state.crashed {
            Some(UiRunError::Crashed)
        } else if state.waiting() >= MAX_WAITING {
            Some(UiRunError::Busy {
                waiting: state.waiting(),
            })
        } else {
            None
        };
        if let Some(error) = refusal {
            // The job is dropped outside the lock: it may own a handle, and dropping that locks.
            drop(state);
            drop(wrapped);
            return Err(error);
        }
        match priority {
            Priority::Interactive => state.interactive.push_back(wrapped),
            Priority::Background => state.background.push_back(wrapped),
        }
        drop(state);
        self.shared.wake.notify_one();
        Ok(answer)
    }
}
