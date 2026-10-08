//! The real desktop: the Accessibility API, AppKit and CoreGraphics.

use std::marker::PhantomData;
use std::time::Duration;

use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSString, NSURL};

use super::ax::{is_trusted, Element};
use super::desktop::Desktop;
use super::keys::{Key, Modifiers};
use super::screen::{session_state, SessionState};
use crate::contract::CONDUCTOR_BUNDLE_ID;

/// Every Accessibility message waits at most this long.
pub const AX_TIMEOUT_SECONDS: f32 = 2.0;

/// The Mac itself. Make it on the thread that will use it: the elements it hands out are not `Send`.
pub struct SystemDesktop {
    _thread_bound: PhantomData<*const ()>,
}

impl SystemDesktop {
    /// Sets the messaging timeout on the system-wide element (a failure is logged with
    /// `tracing::warn!` and ignored).
    pub fn new() -> SystemDesktop {
        if let Err(error) = Element::system_wide().set_messaging_timeout(AX_TIMEOUT_SECONDS) {
            tracing::warn!(%error, "could not set the Accessibility messaging timeout");
        }
        SystemDesktop {
            _thread_bound: PhantomData,
        }
    }
}

impl Default for SystemDesktop {
    fn default() -> Self {
        SystemDesktop::new()
    }
}

/// The application element of `pid` with the bounded timeout. The timeout set on the system-wide
/// element in `new` is process-wide; setting it again here is a second guard, and a failure is
/// ignored.
fn bounded_application(pid: i32) -> Element {
    let element = Element::application(pid);
    let _ = element.set_messaging_timeout(AX_TIMEOUT_SECONDS);
    element
}

impl Desktop for SystemDesktop {
    type Node = Element;

    fn trusted(&self) -> bool {
        is_trusted(false)
    }

    fn session(&self) -> Option<SessionState> {
        session_state()
    }

    fn conductor_pid(&self) -> Option<i32> {
        NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(
            CONDUCTOR_BUNDLE_ID,
        ))
        .iter()
        .filter(|app| !app.isTerminated())
        .map(|app| app.processIdentifier())
        .find(|pid| *pid != -1)
    }

    fn application(&self, pid: i32) -> Element {
        bounded_application(pid)
    }

    fn open_url(&self, url: &str) -> bool {
        match NSURL::URLWithString(&NSString::from_str(url)) {
            Some(url) => NSWorkspace::sharedWorkspace().openURL(&url),
            None => false,
        }
    }

    fn frontmost_pid(&self) -> Option<i32> {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier())
            .filter(|pid| *pid != -1)
    }

    /// Accessibility, not AppKit activation: a background agent's AppKit activation request can be
    /// refused by macOS, and Accessibility is the one grant this app holds.
    fn activate(&self, pid: i32) -> bool {
        bounded_application(pid)
            .set_bool("AXFrontmost", true)
            .is_ok()
    }

    /// Keys go to Conductor's process only, never to the HID tap.
    fn post_key(&self, pid: i32, key: Key, modifiers: Modifiers) -> Result<(), String> {
        crate::ui::keys::post_key(Some(pid), key, modifiers).map_err(|e| e.to_string())
    }

    fn pause(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
