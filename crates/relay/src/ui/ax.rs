//! Hand-written bindings to the Accessibility API (HIServices, reached through
//! ApplicationServices) and a safe wrapper over `AXUIElementRef`.
//!
//! Ownership: every `Copy`/`Create` result is +1 and is owned exactly once, by an [`Element`] or
//! a `CFRetained`, which release it on drop. Attribute and action names are `CFSTR` macros in the
//! SDK, so they are built here as `CFString`s.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFGetTypeID, CFNumber, CFRetained, CFString, CFType, CFTypeID,
};

use super::snapshot::{NodeFields, SnapshotSource};

/// `AXUIElementRef`: `const struct __AXUIElement *`.
type AXUIElementRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// `extern CFStringRef kAXTrustedCheckOptionPrompt`, a real exported variable.
    static kAXTrustedCheckOptionPrompt: Option<&'static CFString>;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout_in_seconds: f32) -> i32;
    fn AXUIElementCopyAttributeNames(element: AXUIElementRef, names: *mut *const CFType) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: &CFString,
        value: *mut *const CFType,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: &CFString,
        value: &CFType,
    ) -> i32;
    fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut *const CFType) -> i32;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: &CFString) -> i32;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
}

// Private in `objc2-core-foundation`, so declared here for `Element`'s `Clone` and `Drop`.
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// A non-success `AXError` code (AXError.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AxError {
    /// kAXErrorFailure (-25200).
    #[error("a system error occurred")]
    Failure,
    /// kAXErrorIllegalArgument (-25201).
    #[error("an illegal argument was passed to the function")]
    IllegalArgument,
    /// kAXErrorInvalidUIElement (-25202).
    #[error("the element is no longer valid")]
    InvalidUiElement,
    /// kAXErrorInvalidUIElementObserver (-25203).
    #[error("the observer is not valid")]
    InvalidUiElementObserver,
    /// kAXErrorCannotComplete (-25204).
    #[error("messaging failed, or the application is busy or unresponsive")]
    CannotComplete,
    /// kAXErrorAttributeUnsupported (-25205).
    #[error("the attribute is not supported by the element")]
    AttributeUnsupported,
    /// kAXErrorActionUnsupported (-25206).
    #[error("the action is not supported by the element")]
    ActionUnsupported,
    /// kAXErrorNotificationUnsupported (-25207).
    #[error("the notification is not supported by the element")]
    NotificationUnsupported,
    /// kAXErrorNotImplemented (-25208).
    #[error("the application does not implement the accessibility function")]
    NotImplemented,
    /// kAXErrorNotificationAlreadyRegistered (-25209).
    #[error("the notification is already registered")]
    NotificationAlreadyRegistered,
    /// kAXErrorNotificationNotRegistered (-25210).
    #[error("the notification is not registered")]
    NotificationNotRegistered,
    /// kAXErrorAPIDisabled (-25211).
    #[error("the accessibility API is disabled")]
    ApiDisabled,
    /// kAXErrorNoValue (-25212).
    #[error("the attribute has no value")]
    NoValue,
    /// kAXErrorParameterizedAttributeUnsupported (-25213).
    #[error("the parameterized attribute is not supported by the element")]
    ParameterizedAttributeUnsupported,
    /// kAXErrorNotEnoughPrecision (-25214).
    #[error("not enough precision")]
    NotEnoughPrecision,
    /// A code AXError.h does not list.
    #[error("accessibility error {0}")]
    Other(i32),
}

impl AxError {
    /// `None` for 0 (success); `Other(code)` for an unknown code.
    pub fn from_code(code: i32) -> Option<AxError> {
        let error = match code {
            0 => return None,
            -25200 => AxError::Failure,
            -25201 => AxError::IllegalArgument,
            -25202 => AxError::InvalidUiElement,
            -25203 => AxError::InvalidUiElementObserver,
            -25204 => AxError::CannotComplete,
            -25205 => AxError::AttributeUnsupported,
            -25206 => AxError::ActionUnsupported,
            -25207 => AxError::NotificationUnsupported,
            -25208 => AxError::NotImplemented,
            -25209 => AxError::NotificationAlreadyRegistered,
            -25210 => AxError::NotificationNotRegistered,
            -25211 => AxError::ApiDisabled,
            -25212 => AxError::NoValue,
            -25213 => AxError::ParameterizedAttributeUnsupported,
            -25214 => AxError::NotEnoughPrecision,
            other => AxError::Other(other),
        };
        Some(error)
    }
}

fn check(code: i32) -> Result<(), AxError> {
    match AxError::from_code(code) {
        None => Ok(()),
        Some(error) => Err(error),
    }
}

/// The +1 result of a `Copy` call, or `None` for a read that found nothing: a NULL result,
/// `kAXErrorNoValue` or `kAXErrorAttributeUnsupported`.
fn copied(code: i32, value: *const CFType) -> Result<Option<CFRetained<CFType>>, AxError> {
    // SAFETY (both arms): a `Copy` function's out-value is +1 (CF_RETURNS_RETAINED) and owned by
    // the caller; `from_raw` takes that ownership and releases it once on drop.
    let owned = NonNull::new(value.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) });
    match AxError::from_code(code) {
        None => Ok(owned),
        Some(AxError::NoValue | AxError::AttributeUnsupported) => Ok(None),
        Some(error) => Err(error),
    }
}

fn is_element(value: &CFType) -> bool {
    // SAFETY: returns a constant type id; it does not message any process.
    CFGetTypeID(Some(value)) == unsafe { AXUIElementGetTypeID() }
}

/// The strings of a CFArray value; anything else, and any non-string item, is skipped.
fn strings_of(value: Option<CFRetained<CFType>>) -> Vec<String> {
    let Some(array) = value.and_then(|value| value.downcast::<CFArray>().ok()) else {
        return Vec::new();
    };
    // SAFETY: every item of a CFArray is a CF object, so reading it as `CFType` is valid.
    let array: &CFArray<CFType> = unsafe { array.cast_unchecked() };
    array
        .iter()
        .filter_map(|item| item.downcast::<CFString>().ok())
        .map(|string| string.to_string())
        .collect()
}

/// The items of a CFArray value that are elements, each owning the retain the iterator took;
/// anything else gives an empty list.
fn elements_of(value: Option<CFRetained<CFType>>) -> Vec<Element> {
    let Some(array) = value.and_then(|value| value.downcast::<CFArray>().ok()) else {
        return Vec::new();
    };
    // SAFETY: every item of a CFArray is a CF object, so reading it as `CFType` is valid.
    let array: &CFArray<CFType> = unsafe { array.cast_unchecked() };
    // `iter` retains each item, and that retain moves into the element.
    array
        .iter()
        .filter(|item| is_element(item))
        .map(Element::from_value)
        .collect()
}

/// A CFBoolean as itself, a CFNumber 0 or 1 as `false` or `true`; anything else is `None`.
fn boolean_of(value: &CFType) -> Option<bool> {
    if let Some(boolean) = value.downcast_ref::<CFBoolean>() {
        return Some(boolean.as_bool());
    }
    value
        .downcast_ref::<CFNumber>()
        .and_then(|number| number.as_i64())
        .and_then(|number| match number {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        })
}

/// An AXUIElementRef with its own retain; released on drop; cloning retains. Not Send and not
/// Sync: every element stays on the thread that made it.
pub struct Element {
    raw: NonNull<c_void>,
    // A raw-pointer marker keeps the type neither `Send` nor `Sync`.
    _thread_bound: PhantomData<*const c_void>,
}

impl Element {
    /// Takes ownership of a +1 reference.
    ///
    /// # Safety
    ///
    /// `raw` must be a valid `AXUIElementRef` the caller owns one retain of, which moves here.
    unsafe fn from_owned(raw: NonNull<c_void>) -> Element {
        Element {
            raw,
            _thread_bound: PhantomData,
        }
    }

    /// Takes ownership of a +1 reference that a `Create` function returned.
    fn created(raw: AXUIElementRef, function: &str) -> Element {
        let raw = NonNull::new(raw.cast_mut())
            .unwrap_or_else(|| panic!("{function} returned NULL, which it never does"));
        // SAFETY: a `Create` function returns a +1 reference, owned by the caller.
        unsafe { Element::from_owned(raw) }
    }

    /// Takes over a +1 CF value known to be an AXUIElement.
    fn from_value(value: CFRetained<CFType>) -> Element {
        let raw = CFRetained::into_raw(value).cast::<c_void>();
        // SAFETY: `into_raw` hands over the retain `value` held, and the caller checked the type.
        unsafe { Element::from_owned(raw) }
    }

    /// The borrowed reference, valid while `self` lives.
    fn as_raw(&self) -> AXUIElementRef {
        self.raw.as_ptr()
    }

    /// The application element of the process `pid`. Creating it messages no process.
    pub fn application(pid: i32) -> Element {
        // SAFETY: takes a plain pid and returns a +1 reference.
        let raw = unsafe { AXUIElementCreateApplication(pid) };
        Element::created(raw, "AXUIElementCreateApplication")
    }

    /// The system-wide element. Creating it messages no process.
    pub fn system_wide() -> Element {
        // SAFETY: takes no argument and returns a +1 reference.
        let raw = unsafe { AXUIElementCreateSystemWide() };
        Element::created(raw, "AXUIElementCreateSystemWide")
    }

    /// AXUIElementSetMessagingTimeout, in seconds.
    pub fn set_messaging_timeout(&self, seconds: f32) -> Result<(), AxError> {
        // SAFETY: `self` holds a valid element for the duration of the call.
        check(unsafe { AXUIElementSetMessagingTimeout(self.as_raw(), seconds) })
    }

    /// The names of the element's attributes.
    pub fn attribute_names(&self) -> Result<Vec<String>, AxError> {
        let mut names = ptr::null();
        // SAFETY: a valid element and a valid out-pointer; the result is +1.
        let code = unsafe { AXUIElementCopyAttributeNames(self.as_raw(), &mut names) };
        copied(code, names).map(strings_of)
    }

    /// The names of the element's actions.
    pub fn action_names(&self) -> Result<Vec<String>, AxError> {
        let mut names = ptr::null();
        // SAFETY: a valid element and a valid out-pointer; the result is +1.
        let code = unsafe { AXUIElementCopyActionNames(self.as_raw(), &mut names) };
        copied(code, names).map(strings_of)
    }

    /// The raw value of an attribute; `Ok(None)` when it is absent or has no value.
    fn value(&self, attribute: &str) -> Result<Option<CFRetained<CFType>>, AxError> {
        let attribute = CFString::from_str(attribute);
        let mut value = ptr::null();
        // SAFETY: a valid element, a valid string and a valid out-pointer; the result is +1.
        let code = unsafe { AXUIElementCopyAttributeValue(self.as_raw(), &attribute, &mut value) };
        copied(code, value)
    }

    /// A string attribute; `Ok(None)` when the attribute is absent, has no value or is not a
    /// string.
    pub fn string(&self, attribute: &str) -> Result<Option<String>, AxError> {
        Ok(self
            .value(attribute)?
            .and_then(|value| value.downcast::<CFString>().ok())
            .map(|string| string.to_string()))
    }

    /// A boolean attribute, given as a CFBoolean or as a CFNumber 0 or 1; `Ok(None)` when the
    /// attribute is absent, has no value or is neither.
    pub fn bool(&self, attribute: &str) -> Result<Option<bool>, AxError> {
        Ok(self.value(attribute)?.and_then(|value| boolean_of(&value)))
    }

    /// An element attribute; `Ok(None)` when the attribute is absent, has no value or is not an
    /// element.
    pub fn element(&self, attribute: &str) -> Result<Option<Element>, AxError> {
        Ok(self
            .value(attribute)?
            .filter(|value| is_element(value))
            .map(Element::from_value))
    }

    /// The elements of an array attribute; empty when the attribute is absent, has no value or
    /// is not an array. Items that are not elements are skipped.
    pub fn elements(&self, attribute: &str) -> Result<Vec<Element>, AxError> {
        Ok(elements_of(self.value(attribute)?))
    }

    fn set(&self, attribute: &str, value: &CFType) -> Result<(), AxError> {
        let attribute = CFString::from_str(attribute);
        // SAFETY: a valid element, string and value; the call does not take ownership of any.
        check(unsafe { AXUIElementSetAttributeValue(self.as_raw(), &attribute, value) })
    }

    /// Sets a string attribute.
    pub fn set_string(&self, attribute: &str, value: &str) -> Result<(), AxError> {
        self.set(attribute, &CFString::from_str(value))
    }

    /// Sets a boolean attribute.
    pub fn set_bool(&self, attribute: &str, value: bool) -> Result<(), AxError> {
        self.set(attribute, CFBoolean::new(value))
    }

    /// Performs an action.
    pub fn perform(&self, action: &str) -> Result<(), AxError> {
        let action = CFString::from_str(action);
        // SAFETY: a valid element and a valid string.
        check(unsafe { AXUIElementPerformAction(self.as_raw(), &action) })
    }

    /// The process id of the element's application.
    pub fn pid(&self) -> Result<i32, AxError> {
        let mut pid = 0;
        // SAFETY: a valid element and a valid out-pointer.
        check(unsafe { AXUIElementGetPid(self.as_raw(), &mut pid) })?;
        Ok(pid)
    }
}

impl Clone for Element {
    fn clone(&self) -> Element {
        // SAFETY: `self.raw` is a valid CF object; the new retain belongs to the clone.
        unsafe { CFRetain(self.as_raw()) };
        Element {
            raw: self.raw,
            _thread_bound: PhantomData,
        }
    }
}

impl Drop for Element {
    fn drop(&mut self) {
        // SAFETY: this element owns exactly one retain, released here once.
        unsafe { CFRelease(self.as_raw()) };
    }
}

impl SnapshotSource for Element {
    fn read(&self) -> NodeFields {
        let string = |attribute| self.string(attribute).ok().flatten();
        let boolean = |attribute| self.bool(attribute).ok().flatten();
        NodeFields {
            role: string("AXRole"),
            subrole: string("AXSubrole"),
            title: string("AXTitle"),
            description: string("AXDescription"),
            identifier: string("AXIdentifier"),
            value: string("AXValue"),
            help: string("AXHelp"),
            placeholder: string("AXPlaceholderValue"),
            enabled: boolean("AXEnabled"),
            focused: boolean("AXFocused"),
            selected: boolean("AXSelected"),
            actions: self.action_names().unwrap_or_default(),
        }
    }

    fn children(&self) -> Vec<Element> {
        self.elements("AXChildren").unwrap_or_default()
    }
}

/// AXIsProcessTrustedWithOptions; `prompt` asks macOS to show its grant dialog.
/// `prompt == false` passes NULL options (no dictionary, no prompt key).
pub fn is_trusted(prompt: bool) -> bool {
    if !prompt {
        // SAFETY: NULL options are allowed; this reads only this process's own grant.
        return unsafe { AXIsProcessTrustedWithOptions(ptr::null()) } != 0;
    }
    // SAFETY: an immutable constant exported by HIServices, initialised when it is loaded. The
    // header never leaves it NULL.
    let key = unsafe { kAXTrustedCheckOptionPrompt }
        .expect("kAXTrustedCheckOptionPrompt is exported by HIServices");
    let dictionary =
        CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);
    let options = CFRetained::as_ptr(&dictionary).as_ptr().cast::<c_void>();
    // SAFETY: `dictionary` is valid and outlives the call, which does not take ownership of it.
    unsafe { AXIsProcessTrustedWithOptions(options) != 0 }
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CFGetRetainCount, Type};

    use super::*;

    /// The retain count of a CF object.
    fn count(value: &CFType) -> isize {
        CFGetRetainCount(Some(value))
    }

    /// Views an element as the `CFType` it is.
    fn cf_type(element: &Element) -> &CFType {
        // SAFETY: `raw` is a valid CF object for as long as `element` lives, and every
        // AXUIElementRef is one.
        unsafe { element.raw.cast::<CFType>().as_ref() }
    }

    /// An array as the plain `CFType` value an attribute read hands out.
    fn as_value(array: CFRetained<CFArray<CFType>>) -> CFRetained<CFType> {
        // SAFETY: every CFArray is a CF object, so reading it as `CFType` is valid; the retain moves.
        unsafe { CFRetained::cast_unchecked::<CFType>(array) }
    }

    /// A fresh +1 retain of the system-wide element, as a `Copy` call would hand it out.
    fn retained_system_wide(element: &Element) -> CFRetained<CFType> {
        cf_type(element).retain()
    }

    #[test]
    fn clone_and_drop_balance_the_retain_count() {
        let element = Element::system_wide();
        let before = count(cf_type(&element));
        let clone = element.clone();
        assert_eq!(count(cf_type(&element)), before + 1);
        drop(clone);
        assert_eq!(count(cf_type(&element)), before);
    }

    #[test]
    fn from_value_moves_the_retain_into_the_element() {
        let original = Element::system_wide();
        let before = count(cf_type(&original));
        let value = retained_system_wide(&original);
        assert_eq!(count(cf_type(&original)), before + 1);
        let moved = Element::from_value(value);
        // The retain moved; there is no further one.
        assert_eq!(count(cf_type(&original)), before + 1);
        drop(moved);
        assert_eq!(count(cf_type(&original)), before);
    }

    #[test]
    fn elements_of_keeps_only_elements_and_balances_retains() {
        let original = Element::system_wide();
        let before = count(cf_type(&original));
        let string = CFString::from_str("not an element");
        let number = CFNumber::new_i32(7);
        let array = CFArray::<CFType>::from_objects(&[cf_type(&original), &string, &number]);
        let with_array = count(cf_type(&original));
        assert_eq!(with_array, before + 1);

        let found = elements_of(Some(as_value(array.clone())));
        assert_eq!(found.len(), 1);
        assert_eq!(count(cf_type(&found[0])), with_array + 1);
        assert_eq!(count(cf_type(&original)), with_array + 1);

        drop(found);
        assert_eq!(count(cf_type(&original)), with_array);
        drop(array);
        assert_eq!(count(cf_type(&original)), before);
    }

    #[test]
    fn elements_of_gives_nothing_for_none_or_a_non_array() {
        assert!(elements_of(None).is_empty());
        let string = CFString::from_str("not an array");
        assert!(elements_of(Some(string.into())).is_empty());
    }

    #[test]
    fn copied_takes_ownership_without_another_retain() {
        let original = Element::system_wide();
        let before = count(cf_type(&original));
        let raw = CFRetained::into_raw(retained_system_wide(&original)).as_ptr();
        assert_eq!(count(cf_type(&original)), before + 1);
        let owned = copied(0, raw).expect("success").expect("a value");
        assert_eq!(count(cf_type(&original)), before + 1);
        drop(owned);
        assert_eq!(count(cf_type(&original)), before);
    }

    #[test]
    fn copied_reads_nothing_as_none() {
        assert!(matches!(copied(0, ptr::null()), Ok(None)));
        assert!(matches!(copied(-25212, ptr::null()), Ok(None)));
        assert!(matches!(copied(-25205, ptr::null()), Ok(None)));
    }

    #[test]
    fn copied_reports_other_errors() {
        assert!(matches!(
            copied(-25204, ptr::null()),
            Err(AxError::CannotComplete)
        ));
    }

    #[test]
    fn strings_of_keeps_only_strings() {
        let a = CFString::from_str("a");
        let number = CFNumber::new_i32(1);
        let b = CFString::from_str("b");
        let array = CFArray::<CFType>::from_objects(&[&a, &number, &b]);
        assert_eq!(strings_of(Some(as_value(array))), ["a", "b"]);
    }

    #[test]
    fn strings_of_gives_nothing_for_none_or_a_non_array() {
        assert!(strings_of(None).is_empty());
        let string = CFString::from_str("not an array");
        assert!(strings_of(Some(string.into())).is_empty());
    }

    #[test]
    fn boolean_of_reads_booleans_and_zero_or_one() {
        assert_eq!(boolean_of(CFBoolean::new(true)), Some(true));
        assert_eq!(boolean_of(CFBoolean::new(false)), Some(false));
        assert_eq!(boolean_of(&CFNumber::new_i32(0)), Some(false));
        assert_eq!(boolean_of(&CFNumber::new_i32(1)), Some(true));
    }

    #[test]
    fn boolean_of_rejects_other_numbers_and_types() {
        assert_eq!(boolean_of(&CFNumber::new_i32(2)), None);
        assert_eq!(boolean_of(&CFString::from_str("1")), None);
    }

    #[test]
    fn is_element_tells_elements_from_other_values() {
        let element = Element::system_wide();
        assert!(is_element(cf_type(&element)));
        assert!(!is_element(&CFString::from_str("not an element")));
    }
}
