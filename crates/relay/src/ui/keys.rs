//! Hand-written bindings to CoreGraphics keyboard events.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFRetained, CFType};

/// `kCGHIDEventTap`: events enter the system where the hardware's do.
const HID_EVENT_TAP: u32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// Returns a +1 `CGEventRef`, or NULL.
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> Option<NonNull<CFType>>;
    fn CGEventSetFlags(event: &CFType, flags: u64);
    fn CGEventPost(tap: u32, event: &CFType);
    fn CGEventPostToPid(pid: i32, event: &CFType);
}

/// A key the relay presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Return,
    Escape,
    Space,
    Delete,
    L,
    T,
    V,
    K,
    W,
    A,
}

/// The modifier keys held with a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub command: bool,
    pub shift: bool,
    pub option: bool,
    pub control: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// CGEventCreateKeyboardEvent returned NULL.
    #[error("the keyboard event could not be created")]
    EventNotCreated,
}

/// The virtual key code of `key`, from HIToolbox Events.h: Return 0x24, Escape 0x35, Space 0x31,
/// Delete 0x33, L 0x25, T 0x11, V 0x09, K 0x28, W 0x0D, A 0x00.
pub fn key_code(key: Key) -> u16 {
    match key {
        Key::Return => 0x24,
        Key::Escape => 0x35,
        Key::Space => 0x31,
        Key::Delete => 0x33,
        Key::L => 0x25,
        Key::T => 0x11,
        Key::V => 0x09,
        Key::K => 0x28,
        Key::W => 0x0D,
        Key::A => 0x00,
    }
}

/// The CGEventFlags of the modifiers (IOLLEvent.h): Shift 0x20000, Control 0x40000,
/// Option 0x80000, Command 0x100000.
pub fn flags(modifiers: Modifiers) -> u64 {
    let mut flags = 0;
    if modifiers.shift {
        flags |= 0x20000;
    }
    if modifiers.control {
        flags |= 0x40000;
    }
    if modifiers.option {
        flags |= 0x80000;
    }
    if modifiers.command {
        flags |= 0x100000;
    }
    flags
}

/// A keyboard event with exactly these flags, owned and released on drop.
fn keyboard_event(code: u16, down: bool, flags: u64) -> Result<CFRetained<CFType>, KeyError> {
    // SAFETY: a NULL source is allowed; the result is +1 or NULL.
    let raw = unsafe { CGEventCreateKeyboardEvent(ptr::null(), code, down) }
        .ok_or(KeyError::EventNotCreated)?;
    // SAFETY: a `Create` result is owned by the caller; `from_raw` releases it once on drop.
    let event = unsafe { CFRetained::from_raw(raw) };
    // SAFETY: a valid event.
    unsafe { CGEventSetFlags(&event, flags) };
    Ok(event)
}

/// Posts key down and key up with the modifiers, to `pid` when given (CGEventPostToPid), else
/// to the HID tap.
pub fn post_key(pid: Option<i32>, key: Key, modifiers: Modifiers) -> Result<(), KeyError> {
    let code = key_code(key);
    let flags = flags(modifiers);
    // Both events exist before either is posted, so a failure never leaves a key held down.
    let down = keyboard_event(code, true, flags)?;
    let up = keyboard_event(code, false, flags)?;
    for event in [&down, &up] {
        match pid {
            // SAFETY: a valid event; posting does not take ownership of it.
            Some(pid) => unsafe { CGEventPostToPid(pid, event) },
            // SAFETY: as above.
            None => unsafe { CGEventPost(HID_EVENT_TAP, event) },
        }
    }
    Ok(())
}
