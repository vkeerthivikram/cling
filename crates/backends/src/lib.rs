//! Clipboard backends for cling.
//!
//! Backends implement [`cling_core::ClipboardProvider`]. At daemon startup the
//! active backend is auto-detected from the environment; the GNOME-extension
//! backend is passive (entries arrive over D-Bus).
//!
//! - [`mock`] — an in-process backend used for tests and the headless harness.
//! - [`x11`] — `cling_x11` feature (default). Full parity: silent history,
//!   auto-paste, source identity.
//! - [`wayland`] — wlroots/KDE data-control (feature-gated). Silent history
//!   only; no auto-paste, no source identity.

pub mod mock;

#[cfg(feature = "x11")]
pub mod x11;

#[cfg(feature = "wayland")]
pub mod wayland;

use cling_common::Caps;

/// Detect and construct the best backend for the current environment.
///
/// Returns the chosen backend name (no instance yet: each backend has its own
/// constructor). Kept as a pure decision function so it is trivially testable.
pub fn detect_backend_name(has_wayland: bool, has_x11: bool) -> Option<&'static str> {
    match (has_wayland, has_x11) {
        (true, _) => Some("wayland"),
        (false, true) => Some("x11"),
        (false, false) => None,
    }
}

/// Helper: build [`Caps`] for the common Wayland variants.
pub fn wayland_caps() -> Caps {
    Caps {
        silent_history: true,
        auto_paste: false,
        source_id: false,
    }
}
