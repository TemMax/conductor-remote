//! This process's identity, which the Accessibility grant is tied to.

use objc2_core_foundation::CFBundle;

/// The main bundle's CFBundleIdentifier (CFBundleGetMainBundle), when this process runs from a
/// bundle.
pub fn bundle_identifier() -> Option<String> {
    CFBundle::main_bundle()?
        .identifier()
        .map(|identifier| identifier.to_string())
}
