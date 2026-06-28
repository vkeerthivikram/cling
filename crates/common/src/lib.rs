//! Shared types for the cling clipboard manager.
//!
//! These types cross three boundaries: the in-process backend/store layer,
//! the D-Bus wire protocol (via `zvariant`), and the persistent SQLite schema.
//! Keep them dependency-light and serializable.

use serde::{Deserialize, Serialize};

/// Stable identifier for a history entry (SQLite rowid).
pub type EntryId = i64;

/// Unix timestamp, milliseconds.
pub type UnixMillis = i64;

/// A single `(mime, bytes)` clipboard target, stored and restored verbatim
/// to preserve full format fidelity (the Ditto/CopyQ model).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MimeBlob {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Coarse classification used to pick a preview renderer in the UI and to
/// summarise an entry cheaply in list views.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PreviewKind {
    Text,
    Image,
    Files,
    Rich,
    Other,
}

impl PreviewKind {
    /// Infer a preview kind from the set of offered MIME targets.
    pub fn infer(targets: &[MimeBlob]) -> PreviewKind {
        let has = |needle: &str| targets.iter().any(|t| t.mime.eq_ignore_ascii_case(needle));
        if has("text/uri-list") || has("x-special/gnome-copied-files") {
            PreviewKind::Files
        } else if targets.iter().any(|t| t.mime.starts_with("image/")) {
            PreviewKind::Image
        } else if has("text/html") || has("text/rtf") || has("text/markdown") {
            PreviewKind::Rich
        } else if has("text/plain") || targets.iter().any(|t| t.mime.starts_with("text/")) {
            PreviewKind::Text
        } else {
            PreviewKind::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PreviewKind::Text => "text",
            PreviewKind::Image => "image",
            PreviewKind::Files => "files",
            PreviewKind::Rich => "rich",
            PreviewKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> PreviewKind {
        match s {
            "text" => PreviewKind::Text,
            "image" => PreviewKind::Image,
            "files" => PreviewKind::Files,
            "rich" => PreviewKind::Rich,
            _ => PreviewKind::Other,
        }
    }
}

/// Best-effort identity of the application that produced a selection.
/// On X11 and the GNOME extension we may know this; on wlroots/KDE we do not
/// (the data-control protocol hides the source for privacy).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppId {
    /// A desktop-agnostic identifier such as a WM_CLASS instance or app-id,
    /// e.g. "keepassxc", "org.gnome.Nautilus".
    pub id: Option<String>,
    /// Human-readable label for display, when known.
    pub label: Option<String>,
}

/// Metadata for an entry as returned to list/search views. Does not include
/// the (potentially large) target blobs; fetch those with `get_entry`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntrySummary {
    pub id: EntryId,
    pub ts: UnixMillis,
    pub pinned: bool,
    pub group: Option<i64>,
    pub origin: Option<String>,
    pub use_count: u32,
    pub preview_kind: PreviewKind,
    pub size_bytes: u64,
    /// Short text preview (first ~160 chars of text/plain) for list display.
    pub preview_text: Option<String>,
}

/// A full entry including all of its MIME targets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: EntryId,
    pub ts: UnixMillis,
    pub pinned: bool,
    pub group: Option<i64>,
    pub origin: Option<String>,
    pub use_count: u32,
    pub preview_kind: PreviewKind,
    pub size_bytes: u64,
    pub targets: Vec<MimeBlob>,
}

impl Entry {
    /// Best-effort text/plain (utf-8) content, if present.
    pub fn text(&self) -> Option<&str> {
        self.targets
            .iter()
            .find(|t| {
                t.mime.eq_ignore_ascii_case("text/plain;charset=utf-8")
                    || t.mime.eq_ignore_ascii_case("text/plain")
            })
            .and_then(|t| std::str::from_utf8(&t.bytes).ok())
    }
}

/// What a backend advertises it can do. Drives UI affordances (e.g. hide the
/// "auto-paste" toggle on Wayland) and policy (e.g. whether exclude-by-app is
/// meaningful).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Caps {
    /// Can the backend capture history silently in the background?
    pub silent_history: bool,
    /// Can the backend synthesize a paste keystroke after offering a selection?
    pub auto_paste: bool,
    /// Can the backend identify the source application (for exclude-by-app)?
    pub source_id: bool,
}

/// A push event from a clipboard backend.
#[derive(Debug, Clone)]
pub enum ClipboardEvent {
    /// The current selection changed; targets are available to read now.
    SelectionChanged { source: AppId },
    /// The clipboard was cleared / emptied.
    Cleared,
}

/// Lightweight description of a clipboard capture that a producer (backend or
/// the GNOME extension) hands to the store/manager.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Capture {
    pub targets: Vec<MimeBlob>,
    pub source: AppId,
    /// Capture time; if None, the receiver stamps it.
    pub ts: Option<UnixMillis>,
}

impl Capture {
    pub fn total_size(&self) -> u64 {
        self.targets.iter().map(|t| t.bytes.len() as u64).sum()
    }
}

/// A named group (tab) for organising history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub icon: Option<String>,
    pub pos: i64,
}
