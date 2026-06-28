//! wlroots + KDE Plasma 6 Wayland clipboard backend.
//!
//! Uses the `wlr-data-control-unstable-v1` protocol (supported by wlroots
//! compositors — Sway, Hyprland, River, Labwc — and by KDE Plasma 6). Silent
//! history capture only; the protocol deliberately hides the source app and
//! cannot synthesize a paste, so `Caps` is [`wayland_caps()`].
//!
//! Status: scaffolded for P2; the data-control client wiring lands next. Until
//! then, the daemon falls back to the X11 backend (or the GNOME extension on
//! GNOME-Wayland).

#![allow(dead_code)]

use cling_core::BackendError;

use crate::wayland_caps;
use cling_common::Caps;

/// Placeholder backend constructor. The real implementation will establish a
/// `wayland-client` registry, bind `zwlr_data_control_manager_v1`, subscribe to
/// `zwlr_data_control_device_v1` selection events, and read each offered target.
pub struct WlrootsBackend {
    caps: Caps,
}

impl WlrootsBackend {
    pub fn new() -> Self {
        WlrootsBackend {
            caps: wayland_caps(),
        }
    }

    /// Probe whether the active compositor advertises data-control.
    /// (Real implementation walks the `wl_registry`.)
    pub fn probe_supported() -> bool {
        false
    }
}

// Suppress unused-import warning for the placeholder; `BackendError` is used by
// the future trait impl.
const _: fn() = || {
    let _ = std::marker::PhantomData::<BackendError>;
};
