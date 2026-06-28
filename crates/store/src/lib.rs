//! Persistent history store backed by SQLite + FTS5, encrypted at rest with
//! SQLCipher (whole-DB; passphrase supplied at open time).
//!
//! Deviation note: the plan specified an argon2id-derived raw key. For v1 we
//! use SQLCipher's built-in KDF (PBKDF2-HMAC-SHA512, default 256k iterations)
//! via `PRAGMA key`, which is the standard, audited path and lets SQLCipher
//! manage the per-DB salt automatically. Swapping in an argon2id raw key is a
//! localised future change (see open-followups in the plan).

use std::sync::Arc;

use cling_common::{AppId, Entry, EntryId, EntrySummary, MimeBlob, PreviewKind, UnixMillis};
use cling_core::{HistoryStore, StoreError};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;

pub mod handle;
pub mod schema;

pub use handle::StoreHandle;

/// Configuration for opening a store.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub max_blob_bytes: u64,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            max_blob_bytes: 50 * 1024 * 1024,
        }
    }
}

/// The history store. Cheap to clone (shares one connection behind a mutex).
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (and migrate) a store at `path`. If `passphrase` is `Some`, the DB
    /// is opened as a SQLCipher-encrypted database; if the DB already exists it
    /// must match, otherwise the key is set on a fresh DB.
    pub fn open(path: &str, passphrase: Option<&str>) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(db_err)?;
        if let Some(pw) = passphrase {
            // NB: bind the key via a parameterised pragma to avoid injection /
            // shell-escaping issues with arbitrary passphrases.
            conn.pragma_update(None, "key", pw).map_err(db_err)?;
        }
        // Tune for a single-writer local app.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        schema::migrate(&conn)?;
        // Verify we can actually read (catches a wrong passphrase immediately).
        let _check: i64 = conn
            .query_row("SELECT count(*) FROM meta", [], |r| r.get(0))
            .map_err(|e| {
                if is_decrypt_error(&e) {
                    StoreError::Locked
                } else {
                    StoreError::Db(e.to_string())
                }
            })?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory store (used by tests).
    pub fn open_memory(passphrase: Option<&str>) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        if let Some(pw) = passphrase {
            conn.pragma_update(None, "key", pw).map_err(db_err)?;
        }
        schema::migrate(&conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

fn db_err(e: rusqlite::Error) -> StoreError {
    if is_decrypt_error(&e) {
        StoreError::Locked
    } else {
        StoreError::Db(e.to_string())
    }
}

fn is_decrypt_error(e: &rusqlite::Error) -> bool {
    // SQLCipher surfaces a wrong key as "file is not a database" (SQLite_NotaDB).
    let s = e.to_string().to_lowercase();
    s.contains("not a database") || s.contains("file is not a database") || s.contains("decrypt")
}

// ---- sync implementation; the async trait below off-loads to these ----

fn add_capture_sync(
    conn: &Connection,
    targets: Vec<MimeBlob>,
    source: AppId,
    max_blob_bytes: u64,
) -> Result<EntryId, StoreError> {
    // Consecutive-dedup: if the most recent entry has identical target bytes,
    // bump its use_count instead of inserting a duplicate.
    let now = unix_millis();

    let tx = conn.unchecked_transaction().map_err(db_err)?;

    // Dedup check against the latest entry.
    if let Some(last_id) = latest_entry_id(&tx)? {
        if entries_identical(&tx, last_id, &targets)? {
            let rc = tx
                .execute(
                    "UPDATE entries SET use_count = use_count + 1 WHERE id = ?1",
                    params![last_id],
                )
                .map_err(db_err)?;
            debug_assert_eq!(rc, 1);
            tx.commit().map_err(db_err)?;
            return Ok(last_id);
        }
    }

    // Insert entry row.
    let preview_kind = PreviewKind::infer(&targets).as_str();
    let total_size: i64 = targets.iter().map(|t| t.bytes.len() as i64).sum();
    let _preview_text = preview_text_of(&targets);
    tx.execute(
        "INSERT INTO entries (ts, pinned, group_id, origin, use_count, preview_kind, deleted, size_bytes)
         VALUES (?1, 0, NULL, ?2, 1, ?3, 0, ?4)",
        params![now, source.id.as_deref(), preview_kind, total_size],
    )
    .map_err(db_err)?;
    let id = tx.last_insert_rowid();

    // Insert targets + index text content.
    let mut fts_content = String::new();
    for t in &targets {
        if (t.bytes.len() as u64) > max_blob_bytes {
            // Oversized target shouldn't have made it past policy, but guard.
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO targets (entry_id, mime, blob) VALUES (?1, ?2, ?3)",
            params![id, t.mime, t.bytes.as_slice()],
        )
        .map_err(db_err)?;
        if t.mime.eq_ignore_ascii_case("text/plain")
            || t.mime.eq_ignore_ascii_case("text/plain;charset=utf-8")
        {
            fts_content.push_str(std::str::from_utf8(&t.bytes).unwrap_or(""));
            fts_content.push('\n');
        } else if t.mime.eq_ignore_ascii_case("text/html") {
            fts_content.push_str(&strip_html(std::str::from_utf8(&t.bytes).unwrap_or("")));
            fts_content.push('\n');
        }
    }

    if !fts_content.trim().is_empty() {
        tx.execute(
            "INSERT INTO entries_fts (rowid, entry_id, content) VALUES (NULL, ?1, ?2)",
            params![id, fts_content],
        )
        .map_err(db_err)?;
    }

    // Stash preview_text in meta cache via a tiny table-free trick: we store it
    // nowhere extra; list view recomputes from targets join. Keep size small.

    tx.commit().map_err(db_err)?;
    Ok(id)
}

fn latest_entry_id(conn: &Connection) -> Result<Option<EntryId>, StoreError> {
    conn.query_row(
        "SELECT id FROM entries WHERE deleted = 0 ORDER BY ts DESC, id DESC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()
    .map_err(db_err)
}

fn entries_identical(
    conn: &Connection,
    id: EntryId,
    targets: &[MimeBlob],
) -> Result<bool, StoreError> {
    let mut stmt = conn
        .prepare("SELECT mime, blob FROM targets WHERE entry_id = ?1 ORDER BY mime")
        .map_err(db_err)?;
    let mut existing: Vec<(String, Vec<u8>)> = Vec::new();
    {
        let iter = stmt
            .query_map(params![id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(db_err)?;
        for row in iter {
            existing.push(row.map_err(db_err)?);
        }
    }
    if existing.len() != targets.len() {
        return Ok(false);
    }
    let mut want: Vec<(String, Vec<u8>)> = targets
        .iter()
        .map(|t| (t.mime.clone(), t.bytes.clone()))
        .collect();
    want.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(existing == want)
}

fn query_sync(
    conn: &Connection,
    offset: i64,
    limit: i64,
    group: Option<i64>,
) -> Result<Vec<EntrySummary>, StoreError> {
    let limit = limit.clamp(1, 500);
    let offset = offset.max(0);
    let sql = match group {
        Some(_) => "SELECT e.id, e.ts, e.pinned, e.group_id, e.origin, e.use_count, e.preview_kind, e.size_bytes, t.blob
                    FROM entries e LEFT JOIN targets t
                      ON t.entry_id = e.id AND (t.mime='text/plain' OR t.mime='text/plain;charset=utf-8')
                    WHERE e.deleted = 0 AND e.group_id = ?1
                    ORDER BY e.ts DESC, e.id DESC LIMIT ?2 OFFSET ?3",
        None => "SELECT e.id, e.ts, e.pinned, e.group_id, e.origin, e.use_count, e.preview_kind, e.size_bytes, t.blob
                 FROM entries e LEFT JOIN targets t
                   ON t.entry_id = e.id AND (t.mime='text/plain' OR t.mime='text/plain;charset=utf-8')
                 WHERE e.deleted = 0
                 ORDER BY e.ts DESC, e.id DESC LIMIT ?1 OFFSET ?2",
    };
    let mut out = Vec::new();
    {
        let mut stmt = conn.prepare(sql).map_err(db_err)?;
        let iter = match group {
            Some(g) => stmt
                .query_map(params![g, limit, offset], row_to_summary)
                .map_err(db_err)?,
            None => stmt
                .query_map(params![limit, offset], row_to_summary)
                .map_err(db_err)?,
        };
        for row in iter {
            out.push(row.map_err(db_err)?);
        }
    }
    Ok(out)
}

fn row_to_summary(r: &rusqlite::Row<'_>) -> rusqlite::Result<EntrySummary> {
    let origin: Option<String> = r.get(4)?;
    let kind_str: String = r.get(6)?;
    let preview_blob: Option<Vec<u8>> = r.get(8)?;
    let preview_text = preview_blob
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .map(|s| truncate(s, 160));
    Ok(EntrySummary {
        id: r.get(0)?,
        ts: r.get(1)?,
        pinned: r.get::<_, i64>(2)? != 0,
        group: r.get(3)?,
        origin,
        use_count: r.get::<_, i64>(5)? as u32,
        preview_kind: PreviewKind::parse(&kind_str),
        size_bytes: r.get::<_, i64>(7)? as u64,
        preview_text,
    })
}

fn search_sync(
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<EntrySummary>, StoreError> {
    let limit = limit.clamp(1, 500);
    let pattern = sanitize_fts(query);
    if pattern.trim().is_empty() {
        return query_sync(conn, 0, limit, None);
    }
    let sql = "SELECT e.id, e.ts, e.pinned, e.group_id, e.origin, e.use_count, e.preview_kind, e.size_bytes, t.blob
               FROM entries_fts f
               JOIN entries e ON e.id = f.entry_id AND e.deleted = 0
               LEFT JOIN targets t ON t.entry_id = e.id AND (t.mime='text/plain' OR t.mime='text/plain;charset=utf-8')
               WHERE entries_fts MATCH ?1
               ORDER BY bm25(entries_fts) ASC, e.ts DESC LIMIT ?2";
    let mut out = Vec::new();
    {
        let mut stmt = conn.prepare(sql).map_err(db_err)?;
        let iter = stmt
            .query_map(params![pattern, limit], row_to_summary)
            .map_err(db_err)?;
        for row in iter {
            out.push(row.map_err(db_err)?);
        }
    }
    Ok(out)
}

fn get_entry_sync(conn: &Connection, id: EntryId) -> Result<Option<Entry>, StoreError> {
    let meta = conn
        .query_row(
            "SELECT id, ts, pinned, group_id, origin, use_count, preview_kind, size_bytes
             FROM entries WHERE id = ?1 AND deleted = 0",
            params![id],
            |r| {
                let kind_str: String = r.get(6)?;
                Ok((
                    r.get::<_, EntryId>(0)?,
                    r.get::<_, UnixMillis>(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)? as u32,
                    PreviewKind::parse(&kind_str),
                    r.get::<_, i64>(7)? as u64,
                ))
            },
        )
        .optional()
        .map_err(db_err)?;
    let Some((id, ts, pinned, group, origin, use_count, preview_kind, size_bytes)) = meta else {
        return Ok(None);
    };

    let mut stmt = conn
        .prepare("SELECT mime, blob FROM targets WHERE entry_id = ?1")
        .map_err(db_err)?;
    let mut targets: Vec<MimeBlob> = Vec::new();
    {
        let iter = stmt
            .query_map(params![id], |r| {
                Ok(MimeBlob {
                    mime: r.get(0)?,
                    bytes: r.get(1)?,
                })
            })
            .map_err(db_err)?;
        for row in iter {
            targets.push(row.map_err(db_err)?);
        }
    }

    Ok(Some(Entry {
        id,
        ts,
        pinned,
        group,
        origin,
        use_count,
        preview_kind,
        size_bytes,
        targets,
    }))
}

fn delete_sync(conn: &Connection, ids: &[EntryId]) -> Result<(), StoreError> {
    if ids.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    for id in ids {
        tx.execute("DELETE FROM entries_fts WHERE entry_id = ?1", params![id])
            .map_err(db_err)?;
        tx.execute("DELETE FROM targets WHERE entry_id = ?1", params![id])
            .map_err(db_err)?;
        tx.execute("DELETE FROM entries WHERE id = ?1", params![id])
            .map_err(db_err)?;
    }
    tx.commit().map_err(db_err)?;
    Ok(())
}

fn set_pinned_sync(conn: &Connection, id: EntryId, pinned: bool) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE entries SET pinned = ?1 WHERE id = ?2 AND deleted = 0",
        params![pinned as i64, id],
    )
    .map_err(db_err)?;
    Ok(())
}

fn set_group_sync(conn: &Connection, id: EntryId, group: Option<i64>) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE entries SET group_id = ?1 WHERE id = ?2 AND deleted = 0",
        params![group, id],
    )
    .map_err(db_err)?;
    Ok(())
}

fn clear_sync(conn: &Connection) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    tx.execute("DELETE FROM entries_fts", []).map_err(db_err)?;
    tx.execute("DELETE FROM targets", []).map_err(db_err)?;
    tx.execute("DELETE FROM entries", []).map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(())
}

fn bump_use_count_sync(conn: &Connection, id: EntryId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE entries SET use_count = use_count + 1 WHERE id = ?1",
        params![id],
    )
    .map_err(db_err)?;
    Ok(())
}

fn count_sync(conn: &Connection) -> Result<i64, StoreError> {
    conn.query_row("SELECT count(*) FROM entries WHERE deleted = 0", [], |r| {
        r.get(0)
    })
    .map_err(db_err)
}

fn prune_sync(
    conn: &Connection,
    max_entries: i64,
    retention_days: Option<i64>,
) -> Result<i64, StoreError> {
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    let mut removed = 0i64;

    // Retention window (age-based), never pinned.
    if let Some(days) = retention_days {
        let cutoff = unix_millis() - days * 86_400_000;
        removed += tx
            .execute(
                "DELETE FROM entries WHERE pinned = 0 AND ts < ?1",
                params![cutoff],
            )
            .map_err(db_err)? as i64;
    }

    // Count-based: keep newest `max_entries` non-deleted; delete older non-pinned.
    let total: i64 = tx
        .query_row("SELECT count(*) FROM entries WHERE deleted = 0", [], |r| {
            r.get(0)
        })
        .map_err(db_err)?;
    if total > max_entries {
        let excess = total - max_entries;
        // ids of the oldest non-pinned entries to drop.
        let mut ids: Vec<EntryId> = Vec::new();
        {
            let mut stmt = tx
                .prepare(
                    "SELECT id FROM entries WHERE deleted = 0 AND pinned = 0
                     ORDER BY ts ASC, id ASC LIMIT ?1",
                )
                .map_err(db_err)?;
            let iter = stmt
                .query_map(params![excess], |r| r.get::<_, EntryId>(0))
                .map_err(db_err)?;
            for row in iter {
                ids.push(row.map_err(db_err)?);
            }
        }
        for id in &ids {
            tx.execute("DELETE FROM entries_fts WHERE entry_id = ?1", params![id])
                .map_err(db_err)?;
            tx.execute("DELETE FROM targets WHERE entry_id = ?1", params![id])
                .map_err(db_err)?;
            tx.execute("DELETE FROM entries WHERE id = ?1", params![id])
                .map_err(db_err)?;
        }
        removed += ids.len() as i64;
    }

    tx.commit().map_err(db_err)?;
    Ok(removed)
}

// ---- helpers ----

fn unix_millis() -> UnixMillis {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn preview_text_of(targets: &[MimeBlob]) -> Option<String> {
    targets
        .iter()
        .find(|t| {
            t.mime.eq_ignore_ascii_case("text/plain")
                || t.mime.eq_ignore_ascii_case("text/plain;charset=utf-8")
        })
        .and_then(|t| std::str::from_utf8(&t.bytes).ok())
        .map(|s| truncate(s, 160))
}

/// Naive HTML tag stripper for indexing. Good enough for search relevance; not
/// a security boundary.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Escape a user query into an FTS5 MATCH pattern: wrap each whitespace-split
/// token in double quotes so punctuation can't break the query.
fn sanitize_fts(query: &str) -> String {
    query
        .split_whitespace()
        .map(|tok| {
            let clean: String = tok
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
                .collect();
            if clean.is_empty() {
                String::new()
            } else {
                format!("\"{}\"*", clean)
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- async HistoryStore impl (off-loads blocking SQLite work) ----

#[async_trait::async_trait]
impl HistoryStore for Store {
    async fn add_capture(
        &self,
        targets: Vec<MimeBlob>,
        source: AppId,
        max_blob_bytes: u64,
    ) -> Result<EntryId, StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            add_capture_sync(&g, targets, source, max_blob_bytes)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn query(
        &self,
        offset: i64,
        limit: i64,
        group: Option<i64>,
    ) -> Result<Vec<EntrySummary>, StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            query_sync(&g, offset, limit, group)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn search(&self, query: &str, limit: i64) -> Result<Vec<EntrySummary>, StoreError> {
        let conn = self.conn.clone();
        let query = query.to_string();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            search_sync(&g, &query, limit)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn get_entry(&self, id: EntryId) -> Result<Option<Entry>, StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            get_entry_sync(&g, id)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn delete(&self, ids: &[EntryId]) -> Result<(), StoreError> {
        let conn = self.conn.clone();
        let ids = ids.to_vec();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            delete_sync(&g, &ids)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn set_pinned(&self, id: EntryId, pinned: bool) -> Result<(), StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            set_pinned_sync(&g, id, pinned)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn set_group(&self, id: EntryId, group: Option<i64>) -> Result<(), StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            set_group_sync(&g, id, group)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn clear(&self) -> Result<(), StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            clear_sync(&g)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn bump_use_count(&self, id: EntryId) -> Result<(), StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            bump_use_count_sync(&g, id)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn count(&self) -> Result<i64, StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            count_sync(&g)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }

    async fn prune(
        &self,
        max_entries: i64,
        retention_days: Option<i64>,
    ) -> Result<i64, StoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = conn.blocking_lock();
            prune_sync(&g, max_entries, retention_days)
        })
        .await
        .map_err(|e| StoreError::Db(format!("join: {e}")))?
    }
}
