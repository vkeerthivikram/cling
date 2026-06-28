//! A re-openable handle around a [`Store`].
//!
//! The daemon holds a `StoreHandle` so it can start *locked* (the encrypted DB
//! cannot be read without a passphrase) and re-open the store in-process once
//! the user supplies a passphrase — which is collected over a private same-UID
//! channel (the unlock dialog), never over the D-Bus session bus.
//!
//! While closed (no passphrase yet), every [`HistoryStore`] method returns
//! [`StoreError::Locked`].

use std::sync::Arc;

use cling_common::{AppId, Entry, EntryId, EntrySummary, MimeBlob};
use cling_core::{HistoryStore, StoreError};
use tokio::sync::Mutex;

use crate::Store;

/// Re-openable store handle, cheap to clone (shares inner state).
#[derive(Clone)]
pub struct StoreHandle {
    path: Option<String>,
    store: Arc<Mutex<Option<Store>>>,
}

impl StoreHandle {
    /// Wrap an already-open store. `path` lets the handle be re-opened later.
    pub fn opened(store: Store, path: Option<String>) -> Self {
        StoreHandle {
            path,
            store: Arc::new(Mutex::new(Some(store))),
        }
    }

    /// Start closed, awaiting an unlock that re-opens `path`.
    pub fn pending(path: String) -> Self {
        StoreHandle {
            path: Some(path),
            store: Arc::new(Mutex::new(None)),
        }
    }

    /// Re-open the store at the stored path with `passphrase`. Returns
    /// `Locked` if the passphrase is wrong (SQLCipher "not a database").
    pub async fn reopen(&self, passphrase: &str) -> Result<(), StoreError> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| StoreError::Db("in-memory store cannot be reopened".into()))?;
        let store = Store::open(&path, Some(passphrase))?;
        *self.store.lock().await = Some(store);
        Ok(())
    }

    pub async fn is_open(&self) -> bool {
        self.store.lock().await.is_some()
    }
}

/// All methods delegate to the open store, or return [`StoreError::Locked`].
#[async_trait::async_trait]
impl HistoryStore for StoreHandle {
    async fn add_capture(
        &self,
        targets: Vec<MimeBlob>,
        source: AppId,
        max_blob_bytes: u64,
    ) -> Result<EntryId, StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.add_capture(targets, source, max_blob_bytes).await,
            None => Err(StoreError::Locked),
        }
    }

    async fn query(
        &self,
        offset: i64,
        limit: i64,
        group: Option<i64>,
    ) -> Result<Vec<EntrySummary>, StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.query(offset, limit, group).await,
            None => Err(StoreError::Locked),
        }
    }

    async fn search(&self, query: &str, limit: i64) -> Result<Vec<EntrySummary>, StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.search(query, limit).await,
            None => Err(StoreError::Locked),
        }
    }

    async fn get_entry(&self, id: EntryId) -> Result<Option<Entry>, StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.get_entry(id).await,
            None => Err(StoreError::Locked),
        }
    }

    async fn delete(&self, ids: &[EntryId]) -> Result<(), StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.delete(ids).await,
            None => Err(StoreError::Locked),
        }
    }

    async fn set_pinned(&self, id: EntryId, pinned: bool) -> Result<(), StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.set_pinned(id, pinned).await,
            None => Err(StoreError::Locked),
        }
    }

    async fn set_group(&self, id: EntryId, group: Option<i64>) -> Result<(), StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.set_group(id, group).await,
            None => Err(StoreError::Locked),
        }
    }

    async fn clear(&self) -> Result<(), StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.clear().await,
            None => Err(StoreError::Locked),
        }
    }

    async fn bump_use_count(&self, id: EntryId) -> Result<(), StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.bump_use_count(id).await,
            None => Err(StoreError::Locked),
        }
    }

    async fn count(&self) -> Result<i64, StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.count().await,
            None => Err(StoreError::Locked),
        }
    }

    async fn prune(
        &self,
        max_entries: i64,
        retention_days: Option<i64>,
    ) -> Result<i64, StoreError> {
        match self.store.lock().await.as_ref() {
            Some(s) => s.prune(max_entries, retention_days).await,
            None => Err(StoreError::Locked),
        }
    }
}
