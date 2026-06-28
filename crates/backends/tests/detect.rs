//! Tests for the (pure) backend-selection decision function and capability maps.

use cling_backends::{detect_backend_name, wayland_caps};

#[test]
fn detects_wayland_when_present() {
    assert_eq!(detect_backend_name(true, true), Some("wayland"));
    assert_eq!(detect_backend_name(true, false), Some("wayland"));
}

#[test]
fn falls_back_to_x11() {
    assert_eq!(detect_backend_name(false, true), Some("x11"));
}

#[test]
fn none_when_no_display() {
    assert_eq!(detect_backend_name(false, false), None);
}

#[test]
fn wayland_caps_advertise_no_paste_no_source() {
    let c = wayland_caps();
    assert!(c.silent_history);
    assert!(!c.auto_paste);
    assert!(!c.source_id);
}
