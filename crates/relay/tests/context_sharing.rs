//! Requests for the context breakdown of one chat that arrive together compute it once.
//!
//! The build hook is process-wide, so this file has one test.

#[path = "support/seed_context.rs"]
mod seed_context;
mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use conductor_remote::reads::context::{
    context_computations, set_context_build_hook, ContextBreakdown,
};
use conductor_remote::reads::Reads;
use seed_context::{CHAT, OTHER};
use support::TestDb;

/// Bounded waits, so a test that fails does not hang.
const LIMIT: Duration = Duration::from_secs(20);

/// What the hook and the test tell each other: the computation of `CHAT` has started, and it
/// may go on.
#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Gate {
    fn entered(&self) {
        self.state.lock().unwrap().0 = true;
        self.changed.notify_all();
    }

    fn wait_until_entered(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, LIMIT, |(entered, _)| !*entered)
            .unwrap();
        assert!(state.0, "the computation never started");
    }

    fn wait_until_released(&self) {
        let state = self.state.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(state, LIMIT, |(_, released)| !*released)
            .unwrap();
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

fn request(reads: &Arc<Reads>, chat: &'static str) -> mpsc::Receiver<Option<ContextBreakdown>> {
    let (sender, receiver) = mpsc::channel();
    let reads = Arc::clone(reads);
    std::thread::spawn(move || {
        let _ = sender.send(reads.context_breakdown(chat).unwrap());
    });
    receiver
}

#[test]
fn requests_for_one_chat_share_one_computation_and_other_chats_are_not_held_up() {
    let test = TestDb::new();
    seed_context::seed(&test.conn());
    let reads = Arc::new(Reads::new(test.db(), test.root()));

    let gate = Arc::new(Gate::default());
    let hold = Arc::clone(&gate);
    set_context_build_hook(Some(Arc::new(move |chat: &str| {
        // The hook is called for every chat; only the computation of `CHAT` is held open.
        if chat == CHAT {
            hold.entered();
            hold.wait_until_released();
        }
    })));

    let first = request(&reads, CHAT);
    gate.wait_until_entered();

    // A second request for the same chat, started while the first is held open.
    let second_returned = Arc::new(AtomicBool::new(false));
    let second = {
        let returned = Arc::clone(&second_returned);
        let reads = Arc::clone(&reads);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = reads.context_breakdown(CHAT).unwrap();
            returned.store(true, Ordering::SeqCst);
            let _ = sender.send(result);
        });
        receiver
    };
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !second_returned.load(Ordering::SeqCst),
        "the second request must wait for the first"
    );

    // Another chat is not held up by the held one.
    let other = request(&reads, OTHER)
        .recv_timeout(LIMIT)
        .expect("a request for another chat completes while the first chat is held");
    assert!(other.is_some());
    assert!(!second_returned.load(Ordering::SeqCst));
    assert_eq!(context_computations(test.path(), OTHER), 1);

    gate.release();
    let first = first
        .recv_timeout(LIMIT)
        .expect("the first request returns");
    let second = second
        .recv_timeout(LIMIT)
        .expect("the second request returns");
    set_context_build_hook(None);

    assert!(first.is_some());
    assert_eq!(first, second, "the second request gets the first's result");
    assert_eq!(context_computations(test.path(), CHAT), 1);
}
