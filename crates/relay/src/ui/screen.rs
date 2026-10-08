//! The window server session: whether the screen is locked and whether the session is in front.

use std::ptr::NonNull;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// Returns a +1 dictionary, or NULL outside a GUI session.
    fn CGSessionCopyCurrentDictionary() -> Option<NonNull<CFDictionary>>;
}

/// The state of the session this process runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionState {
    pub locked: bool,
    pub on_console: bool,
}

/// From CGSessionCopyCurrentDictionary: `CGSSessionScreenIsLocked` (absent → false) and
/// `kCGSSessionOnConsoleKey` (both keys are CFSTR macros, not symbols: build the CFStrings by
/// hand). Any failure is `None`. `None` without a session (a process outside the GUI session).
pub fn session_state() -> Option<SessionState> {
    // SAFETY: takes no argument; the result is +1 or NULL.
    let raw = unsafe { CGSessionCopyCurrentDictionary() }?;
    // SAFETY: a `Copy` result is owned by the caller; `from_raw` releases it once on drop.
    let owned = unsafe { CFRetained::from_raw(raw) };
    // SAFETY: the session dictionary's keys are strings and its values CF objects.
    let dictionary: &CFDictionary<CFString, CFType> = unsafe { owned.cast_unchecked() };
    // Undocumented: the key is present while the lock screen is up and absent otherwise.
    let locked = match dictionary.get(&CFString::from_static_str("CGSSessionScreenIsLocked")) {
        None => false,
        Some(value) => truthy(&value)?,
    };
    // kCGSessionOnConsoleKey, CGSession.h.
    let on_console = dictionary.get(&CFString::from_static_str("kCGSSessionOnConsoleKey"))?;
    let on_console = truthy(&on_console)?;
    Some(SessionState { locked, on_console })
}

/// A CFBoolean, or a CFNumber read as zero / non-zero; `None` for anything else.
fn truthy(value: &CFType) -> Option<bool> {
    if let Some(boolean) = value.downcast_ref::<CFBoolean>() {
        return Some(boolean.as_bool());
    }
    value
        .downcast_ref::<CFNumber>()
        .and_then(|number| number.as_i64())
        .map(|number| number != 0)
}
