//! Forward-only schema migrations for the history DB.

use cling_core::StoreError;
use rusqlite::{params, Connection, OptionalExtension};

pub const CURRENT_VERSION: i64 = 1;

/// Apply all migrations up to [`CURRENT_VERSION`].
pub fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let version = current_version(conn)?;
    if version < 1 {
        v1(conn)?;
    }
    set_version(conn, CURRENT_VERSION)?;
    Ok(())
}

fn current_version(conn: &Connection) -> Result<i64, StoreError> {
    // Ensure meta table exists before reading it.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(|e| StoreError::Db(e.to_string()))?;
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| StoreError::Db(e.to_string()))?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
}

fn set_version(conn: &Connection, v: i64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![v.to_string()],
    )
    .map_err(|e| StoreError::Db(e.to_string()))?;
    Ok(())
}

/// v1: the initial full schema.
fn v1(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS groups (
            id    INTEGER PRIMARY KEY,
            name  TEXT NOT NULL UNIQUE,
            icon  TEXT,
            pos   INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS entries (
            id           INTEGER PRIMARY KEY,
            ts           INTEGER NOT NULL,
            pinned       INTEGER NOT NULL DEFAULT 0,
            group_id     INTEGER REFERENCES groups(id),
            origin       TEXT,
            use_count    INTEGER NOT NULL DEFAULT 1,
            preview_kind TEXT NOT NULL DEFAULT 'text',
            deleted      INTEGER NOT NULL DEFAULT 0,
            size_bytes   INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_entries_ts ON entries(ts DESC);
        CREATE INDEX IF NOT EXISTS idx_entries_group ON entries(group_id);

        CREATE TABLE IF NOT EXISTS targets (
            entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
            mime     TEXT NOT NULL,
            blob     BLOB NOT NULL,
            PRIMARY KEY (entry_id, mime)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
            entry_id UNINDEXED,
            content,
            tokenize = 'unicode61 remove_diacritics 2'
        );

        CREATE TABLE IF NOT EXISTS entry_tags (
            entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
            tag      TEXT NOT NULL,
            PRIMARY KEY (entry_id, tag)
        );
        "#,
    )
    .map_err(|e| StoreError::Db(e.to_string()))?;
    Ok(())
}
