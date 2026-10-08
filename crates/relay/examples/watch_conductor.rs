//! Manual check of the lifecycle watcher: prints whether Conductor is running, then every change.
//!
//! ```sh
//! cargo run -p conductor-remote --example watch_conductor
//! ```
//!
//! Open and quit Conductor while it runs; stop it with Ctrl-C.

use std::thread;

use conductor_remote::contract::CONDUCTOR_BUNDLE_ID;
use conductor_remote::lifecycle;

fn main() {
    let conductor = lifecycle::start(CONDUCTOR_BUNDLE_ID);
    println!("initial: {:?}", conductor.status());

    let mut changes = conductor.subscribe();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        runtime.block_on(async {
            while changes.changed().await.is_ok() {
                println!("changed: {:?}", *changes.borrow_and_update());
            }
        });
    });

    lifecycle::run_main_loop()
}
