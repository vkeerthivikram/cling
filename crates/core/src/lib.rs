//! The clipboard provider abstraction and the manager that orchestrates a
//! backend together with the history store.
//!
//! The daemon is display-server-agnostic: it talks to a `ClipboardProvider`
//! (X11 / wlroots / KDE-Wayland, or the GNOME-extension push backend) and a
//! `HistoryStore`, neither of which knows about the other.

use cling_common::{AppId, Caps, ClipboardEvent, Entry, EntryId, EntrySummary, MimeBlob};
use futures::stream::BoxStream;

pub mod manager;

pub use manager::{CapturePolicy, ClipboardManager, ContentRules};

/// The unifying interface every clipboard backend implements.
///
/// Backends are selected at daemon startup based on the active display server
/// (`WAYLAND_DISPLAY` + data-control probe, else X11; GNOME-extension pushes
/// arrive over D-Bus as a passive backend). All variants are compiled into a
/// single binary; there is no plugin loader.
#[async_trait::async_trait]
pub trait ClipboardProvider: Send + Sync {
    /// Human-readable backend name, e.g. "x11", "wlroots", "gnome-ext".
    fn name(&self) -> &'static str;

    /// What this backend can do. Drives UI affordances and policy.
    fn capabilities(&self) -> Caps;

    /// A stream of clipboard events. The manager reacts to
    /// `SelectionChanged` by reading targets and persisting them.
    fn subscribe(&self) -> BoxStream<'static, ClipboardEvent>;

    /// Read every offered MIME target of the current selection, full-fidelity.
    async fn read_targets(&self) -> std::result::Result<Vec<MimeBlob>, BackendError>;

    /// Offer an entry back onto the clipboard selection (copy-from-history).
    async fn offer(&self, entry: &Entry) -> std::result::Result<(), BackendError>;

    /// Best-effort identity of the application that owns the current selection.
    /// Returns `None` on backends that hide the source (wlroots/KDE).
    async fn source_hint(&self) -> std::result::Result<AppId, BackendError>;
}

/// Sink the manager uses to persist captures. `cling-store` implements this;
/// a fake can implement it in tests.
#[async_trait::async_trait]
pub trait HistoryStore: Send + Sync {
    async fn add_capture(
        &self,
        targets: Vec<MimeBlob>,
        source: AppId,
        max_blob_bytes: u64,
    ) -> std::result::Result<EntryId, StoreError>;

    async fn query(
        &self,
        offset: i64,
        limit: i64,
        group: Option<i64>,
    ) -> std::result::Result<Vec<EntrySummary>, StoreError>;

    async fn search(
        &self,
        query: &str,
        limit: i64,
    ) -> std::result::Result<Vec<EntrySummary>, StoreError>;

    async fn get_entry(&self, id: EntryId) -> std::result::Result<Option<Entry>, StoreError>;

    async fn delete(&self, ids: &[EntryId]) -> std::result::Result<(), StoreError>;

    async fn set_pinned(&self, id: EntryId, pinned: bool) -> std::result::Result<(), StoreError>;

    async fn set_group(
        &self,
        id: EntryId,
        group: Option<i64>,
    ) -> std::result::Result<(), StoreError>;

    async fn clear(&self) -> std::result::Result<(), StoreError>;

    async fn bump_use_count(&self, id: EntryId) -> std::result::Result<(), StoreError>;

    async fn count(&self) -> std::result::Result<i64, StoreError>;

    async fn prune(
        &self,
        max_entries: i64,
        retention_days: Option<i64>,
    ) -> std::result::Result<i64, StoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("backend io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("backend protocol error: {0}")]
    Protocol(String),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(String),
    #[error("database is locked; unlock required")]
    Locked,
}
