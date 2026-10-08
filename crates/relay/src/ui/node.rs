//! One element of another app's window, as the UI logic sees it.

use super::ax::{AxError, Element};

/// What the UI logic reads of and does to one element. `Element` implements it over the
/// Accessibility API; `fake::FakeNode` implements it for tests.
pub trait UiNode: Clone {
    /// `AXRole`.
    fn role(&self) -> Option<String>;
    /// `AXSubrole`.
    fn subrole(&self) -> Option<String>;
    /// The label: a non-empty `AXTitle`, else a non-empty `AXDescription`.
    fn label(&self) -> Option<String>;
    /// `AXValue` when it is a string.
    fn value(&self) -> Result<Option<String>, AxError>;
    /// `AXSelected`.
    fn selected(&self) -> Option<bool>;
    /// `AXHelp`, when it is not empty.
    fn help(&self) -> Option<String>;
    /// `AXValue` when it is a boolean.
    fn flag(&self) -> Option<bool>;
    /// `AXChildren`, in order.
    fn children(&self) -> Result<Vec<Self>, AxError>;
    /// `AXPress`.
    fn press(&self) -> Result<(), AxError>;
    /// `AXShowMenu`.
    fn show_menu(&self) -> Result<(), AxError>;
    /// Sets `AXValue` to a string.
    fn set_value(&self, text: &str) -> Result<(), AxError>;
    /// Sets `AXFocused`.
    fn set_focused(&self, focused: bool) -> Result<(), AxError>;
}

impl UiNode for Element {
    fn role(&self) -> Option<String> {
        self.string("AXRole").ok().flatten()
    }

    fn subrole(&self) -> Option<String> {
        self.string("AXSubrole").ok().flatten()
    }

    fn label(&self) -> Option<String> {
        ["AXTitle", "AXDescription"]
            .into_iter()
            .filter_map(|attribute| self.string(attribute).ok().flatten())
            .find(|label| !label.is_empty())
    }

    fn value(&self) -> Result<Option<String>, AxError> {
        self.string("AXValue")
    }

    fn selected(&self) -> Option<bool> {
        self.bool("AXSelected").ok().flatten()
    }

    fn help(&self) -> Option<String> {
        self.string("AXHelp")
            .ok()
            .flatten()
            .filter(|help| !help.is_empty())
    }

    fn flag(&self) -> Option<bool> {
        self.bool("AXValue").ok().flatten()
    }

    fn children(&self) -> Result<Vec<Element>, AxError> {
        self.elements("AXChildren")
    }

    fn press(&self) -> Result<(), AxError> {
        self.perform("AXPress")
    }

    fn show_menu(&self) -> Result<(), AxError> {
        self.perform("AXShowMenu")
    }

    fn set_value(&self, text: &str) -> Result<(), AxError> {
        self.set_string("AXValue", text)
    }

    fn set_focused(&self, focused: bool) -> Result<(), AxError> {
        self.set_bool("AXFocused", focused)
    }
}
