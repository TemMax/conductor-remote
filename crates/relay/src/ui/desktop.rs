//! Everything the UI logic needs from the Mac besides the element tree.

use std::time::Duration;

use super::keys::{Key, Modifiers};
use super::node::UiNode;
use super::screen::SessionState;

/// The Mac as the UI logic sees it. `system::SystemDesktop` is the real one; `fake::FakeDesktop`
/// is the one tests use.
pub trait Desktop {
    type Node: UiNode;
    /// Whether this process holds the Accessibility grant. Never prompts.
    fn trusted(&self) -> bool;
    /// The window server session; `None` when unknown.
    fn session(&self) -> Option<SessionState>;
    /// The process id of a running Conductor.
    fn conductor_pid(&self) -> Option<i32>;
    /// The application element of `pid`, with a bounded messaging timeout.
    fn application(&self, pid: i32) -> Self::Node;
    /// Opens a URL with the app registered for its scheme; false when it could not be handed over.
    fn open_url(&self, url: &str) -> bool;
    /// The process id of the frontmost app.
    fn frontmost_pid(&self) -> Option<i32>;
    /// Asks for `pid`'s app to come to the front; false when the request was refused.
    fn activate(&self, pid: i32) -> bool;
    /// Posts key down and key up with the modifiers to `pid` only.
    fn post_key(&self, pid: i32, key: Key, modifiers: Modifiers) -> Result<(), String>;
    /// Waits. The fake records the duration and returns at once.
    fn pause(&self, duration: Duration);
}
