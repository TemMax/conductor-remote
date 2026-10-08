//! The macOS bindings: the list of running apps, the workspace notifications, the run loop.

use std::ptr::NonNull;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use block2::RcBlock;
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidTerminateApplicationNotification,
};
use objc2_foundation::{NSNotification, NSNotificationName, NSOperationQueue, NSRunLoop, NSString};

use super::tracker::{still_running, StatusTracker};
use crate::contract::ConductorStatus;

/// Whether an app with this bundle identifier is among the running applications right now.
pub(super) fn current_status(bundle_id: &str) -> ConductorStatus {
    let running = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .any(|app| bundle_id_of(&app).as_deref() == Some(bundle_id));
    if running {
        ConductorStatus::Running
    } else {
        ConductorStatus::NotRunning
    }
}

/// Feed the tracker from the workspace's launch and terminate notifications. The handlers run on
/// the main thread, when its run loop runs.
pub(super) fn observe(tracker: Arc<StatusTracker>) {
    // SAFETY: both are immutable constants exported by AppKit, initialised when it is loaded.
    let (launched, terminated) = unsafe {
        (
            NSWorkspaceDidLaunchApplicationNotification,
            NSWorkspaceDidTerminateApplicationNotification,
        )
    };
    let on_launch = Arc::clone(&tracker);
    add_observer(launched, move |app| {
        on_launch.on_launched(app.and_then(bundle_id_of).as_deref())
    });
    add_observer(terminated, move |app| {
        let bundle_id = app.and_then(bundle_id_of);
        if bundle_id.as_deref() == Some(tracker.bundle_id())
            && still_running(&instances_of(tracker.bundle_id()), app.and_then(pid_of))
        {
            // Another instance of the watched app is still up.
            return;
        }
        tracker.on_terminated(bundle_id.as_deref());
    });
}

fn add_observer(
    name: &NSNotificationName,
    handler: impl Fn(Option<&NSRunningApplication>) + 'static,
) {
    let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
        // SAFETY: the notification center passes a valid notification that outlives this call,
        // and the reference does not escape it.
        let notification = unsafe { notification.as_ref() };
        // SAFETY: an immutable constant exported by AppKit, initialised when it is loaded.
        let key: &NSString = unsafe { NSWorkspaceApplicationKey };
        // The app the notification is about. A checked cast: anything but a running application
        // yields `None`.
        let app = notification
            .userInfo()
            .and_then(|info| info.objectForKey(key))
            .and_then(|app| app.downcast::<NSRunningApplication>().ok());
        handler(app.as_deref());
    });
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: `name` is a valid notification name, a nil object means any sender, and the block
    // takes the single notification argument this method calls it with. The center copies the
    // block and delivers on the main queue, so the block never runs on another thread.
    let _token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(name),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    };
    // The center holds the registration until it is removed, and it never is: the watcher lives
    // as long as the process. The token is only needed for removal, so it is dropped here.
}

/// The running instances of the app with this bundle identifier, as (process id, already
/// terminated). The one that just terminated may still be listed.
fn instances_of(bundle_id: &str) -> Vec<(i32, bool)> {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id))
        .iter()
        .map(|app| (app.processIdentifier(), app.isTerminated()))
        .collect()
}

/// The process id of an app, or `None` for an app that has none.
fn pid_of(app: &NSRunningApplication) -> Option<i32> {
    let pid = app.processIdentifier();
    (pid != -1).then_some(pid)
}

fn bundle_id_of(app: &NSRunningApplication) -> Option<String> {
    app.bundleIdentifier().map(|id| id.to_string())
}

pub(super) fn run_main_loop() -> ! {
    let run_loop = NSRunLoop::mainRunLoop();
    loop {
        // Blocks for as long as the run loop has an input source, which the workspace provides.
        run_loop.run();
        // It returns at once when there is none; wait rather than spin until one appears.
        thread::sleep(Duration::from_secs(1));
    }
}
