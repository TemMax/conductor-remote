//! Driving another app's window, in three layers: the native layer (the Accessibility API,
//! keyboard events, the session lock, this process's identity, and a read-only snapshot of an
//! element tree), the UI logic over it (the node and desktop traits, the targets and errors, the
//! actions, the driver and the UI thread), and the fakes the tests use in place of the Mac.
//!
//! Nothing here runs on its own: every function waits for a caller.

pub mod actions;
pub mod actor;
pub mod ax;
pub mod desktop;
pub mod driver;
pub mod fake;
pub mod identity;
pub mod keys;
pub mod node;
pub mod screen;
pub mod snapshot;
pub mod system;
