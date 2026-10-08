//! Watches for the process that started the relay going away, so a relay started by the menu-bar
//! app stops with it.

use std::time::Duration;

use tokio::sync::oneshot;

/// The name of the watching thread.
const THREAD_NAME: &str = "parent-watch";

/// Resolves once `parent_id()` no longer returns what it is compared against: the process that
/// started this one is gone and the system gave it another parent. That is `expected` when given,
/// else what `parent_id()` returned at the call. Looks at once, then every `every`, so with
/// `expected` a parent that is already different resolves the receiver without a wait.
///
/// The watching thread ends after sending, or when the receiver is dropped. A thread that could
/// not start leaves the receiver closed, which is not "gone".
pub fn parent_gone(
    parent_id: impl Fn() -> u32 + Send + 'static,
    every: Duration,
    expected: Option<u32>,
) -> oneshot::Receiver<()> {
    let (gone, receiver) = oneshot::channel();
    let started_by = expected.unwrap_or_else(&parent_id);
    let started = std::thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || loop {
            if gone.is_closed() {
                return;
            }
            if parent_id() != started_by {
                // The receiver may have been dropped since the check above: nobody is waiting.
                let _ = gone.send(());
                return;
            }
            std::thread::sleep(every);
        });
    if let Err(error) = started {
        tracing::error!(%error, "could not start the thread that watches the parent process");
    }
    receiver
}
