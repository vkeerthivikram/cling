//! D-Bus data-transfer objects (derive `serde` + `zvariant::Type`).

use cling_common::{Entry, EntrySummary, PreviewKind};
use serde::{Deserialize, Serialize};
use zvariant::Type;

/// A `(mime, bytes)` target on the wire.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq, Eq)]
pub struct TargetDto {
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl From<TargetDto> for cling_common::MimeBlob {
    fn from(t: TargetDto) -> Self {
        cling_common::MimeBlob {
            mime: t.mime,
            bytes: t.bytes,
        }
    }
}

impl From<cling_common::MimeBlob> for TargetDto {
    fn from(t: cling_common::MimeBlob) -> Self {
        TargetDto {
            mime: t.mime,
            bytes: t.bytes,
        }
    }
}

/// List/search view of an entry (no blobs).
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
pub struct SummaryDto {
    pub id: i64,
    pub ts: i64,
    pub pinned: bool,
    pub group: Option<i64>,
    pub origin: Option<String>,
    pub use_count: u32,
    pub preview_kind: String,
    pub size_bytes: u64,
    pub preview_text: Option<String>,
}

impl From<EntrySummary> for SummaryDto {
    fn from(e: EntrySummary) -> Self {
        SummaryDto {
            id: e.id,
            ts: e.ts,
            pinned: e.pinned,
            group: e.group,
            origin: e.origin,
            use_count: e.use_count,
            preview_kind: e.preview_kind.as_str().to_string(),
            size_bytes: e.size_bytes,
            preview_text: e.preview_text,
        }
    }
}

/// Full entry including all targets.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
pub struct EntryDto {
    pub id: i64,
    pub ts: i64,
    pub pinned: bool,
    pub group: Option<i64>,
    pub origin: Option<String>,
    pub use_count: u32,
    pub preview_kind: String,
    pub size_bytes: u64,
    pub targets: Vec<TargetDto>,
}

impl From<Entry> for EntryDto {
    fn from(e: Entry) -> Self {
        EntryDto {
            id: e.id,
            ts: e.ts,
            pinned: e.pinned,
            group: e.group,
            origin: e.origin,
            use_count: e.use_count,
            preview_kind: match e.preview_kind {
                PreviewKind::Text => "text",
                PreviewKind::Image => "image",
                PreviewKind::Files => "files",
                PreviewKind::Rich => "rich",
                PreviewKind::Other => "other",
            }
            .to_string(),
            size_bytes: e.size_bytes,
            targets: e.targets.into_iter().map(Into::into).collect(),
        }
    }
}
