//! The D-Bus interface for `org.cling.ClipboardManager`.
//!
//! This is the single contract spoken by `cling-show` (UI), `cling-cli`, and
//! the GNOME Shell extension. The unlock passphrase never crosses the bus:
//! `RequestUnlock()` delegates to an injected [`UnlockHandler`] which, in the
//! real daemon, collects the passphrase over a private same-UID channel and
//! re-opens the store in-process.

use std::sync::Arc;

use cling_core::{ClipboardProvider, HistoryStore};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use zbus::fdo;
use zvariant::{Type, Value};

pub mod dto;
pub mod unlock;

pub use unlock::{NoUnlock, UnlockOutcome, UnlockRequest, UnlockResult};

/// Wire DTOs reused by the interface.
pub use dto::{EntryDto, SummaryDto, TargetDto};

/// D-Bus service name / path constants.
pub const BUS_NAME: &str = "org.cling.ClipboardManager";
pub const OBJECT_PATH: &str = "/org/cling/ClipboardManager";
const IFACE_NAME: &str = "org.cling.ClipboardManager";

/// Capabilities advertised as a property.
#[derive(Serialize, Deserialize, Type, Value, Clone, Copy, Debug, Default)]
pub struct CapsDto {
    pub silent_history: bool,
    pub auto_paste: bool,
    pub source_id: bool,
}

/// Snapshot of service state returned by the `State` method.
#[derive(Serialize, Deserialize, Type, Debug, Clone)]
pub struct StateDto {
    pub paused: bool,
    pub locked: bool,
    pub backend: String,
    pub entry_count: i64,
    pub excluded_apps: Vec<String>,
}

/// The D-Bus object. Holds the store, the provider (for `Pick`), the live
/// capture policy, an injected unlock handler, and the connection (for emitting
/// signals via raw `emit_signal`, sidestepping per-method SignalContext args).
pub struct ClipboardManagerService {
    store: Arc<dyn HistoryStore>,
    provider: Arc<dyn ClipboardProvider>,
    conn: zbus::Connection,
    state: Mutex<ServiceState>,
    auto_paste: Option<Arc<dyn Fn() + Send + Sync>>,
    unlock: Arc<dyn UnlockRequest>,
}

#[derive(Default)]
struct ServiceState {
    paused: bool,
    locked: bool,
    excluded_apps: Vec<String>,
    deny_regex: Vec<String>,
}

impl ClipboardManagerService {
    pub fn new(
        store: Arc<dyn HistoryStore>,
        provider: Arc<dyn ClipboardProvider>,
        unlock: Arc<dyn UnlockRequest>,
        conn: zbus::Connection,
    ) -> Self {
        ClipboardManagerService {
            store,
            provider,
            conn,
            state: Mutex::new(ServiceState::default()),
            auto_paste: None,
            unlock,
        }
    }

    pub fn with_auto_paste(mut self, auto_paste: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        self.auto_paste = auto_paste;
        self
    }

    async fn ensure_unlocked(&self, state: &ServiceState) -> fdo::Result<()> {
        if state.locked {
            return Err(fdo::Error::Failed(
                "database is locked; call RequestUnlock".into(),
            ));
        }
        Ok(())
    }

    // ---- raw signal emission (broadcast) ----
    async fn sig(&self, member: &str, body: &(impl serde::Serialize + Type)) {
        let res: zbus::Result<()> = self
            .conn
            .emit_signal(
                Option::<&str>::None,
                zvariant::ObjectPath::try_from(OBJECT_PATH).unwrap(),
                zbus::names::InterfaceName::try_from(IFACE_NAME).unwrap(),
                member,
                body,
            )
            .await;
        if let Err(e) = res {
            tracing::debug!(%member, error = %e, "signal emit failed");
        }
    }

    async fn emit_entry_added(&self, id: i64) {
        self.sig("EntryAdded", &(id)).await;
    }
    async fn emit_entry_removed(&self, id: i64) {
        self.sig("EntryRemoved", &(id)).await;
    }
    async fn emit_state_changed(&self, paused: bool, locked: bool) {
        self.sig("StateChanged", &(paused, locked)).await;
    }
    async fn emit_unlocked(&self) {
        self.sig("Unlocked", &()).await;
    }
}

/// The `org.cling.ClipboardManager` interface. Method/signature names map 1:1
/// to the plan's D-Bus contract. Signals are emitted via raw `emit_signal`.
#[zbus::interface(name = "org.cling.ClipboardManager")]
impl ClipboardManagerService {
    // ---- properties ----

    #[zbus(property)]
    pub async fn locked(&self) -> fdo::Result<bool> {
        Ok(self.state.lock().await.locked)
    }

    #[zbus(property)]
    pub async fn paused(&self) -> fdo::Result<bool> {
        Ok(self.state.lock().await.paused)
    }

    #[zbus(property)]
    pub fn backend_name(&self) -> fdo::Result<String> {
        Ok(self.provider.name().to_string())
    }

    #[zbus(property)]
    pub fn caps(&self) -> fdo::Result<CapsDto> {
        let c = self.provider.capabilities();
        Ok(CapsDto {
            silent_history: c.silent_history,
            auto_paste: c.auto_paste,
            source_id: c.source_id,
        })
    }

    #[zbus(property)]
    pub async fn entry_count(&self) -> fdo::Result<i64> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        self.store.count().await.map_err(dbus_err)
    }

    // ---- signals (declarations; emitted via raw emit_signal above) ----

    #[zbus(signal)]
    pub async fn entry_added(
        &self,
        #[zbus(signal_context)] ctx: &zbus::object_server::SignalContext<'_>,
        id: i64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn entry_removed(
        &self,
        #[zbus(signal_context)] ctx: &zbus::object_server::SignalContext<'_>,
        id: i64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn state_changed(
        &self,
        #[zbus(signal_context)] ctx: &zbus::object_server::SignalContext<'_>,
        paused: bool,
        locked: bool,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn unlocked(
        &self,
        #[zbus(signal_context)] ctx: &zbus::object_server::SignalContext<'_>,
    ) -> zbus::Result<()>;

    // ---- methods ----

    pub async fn query(
        &self,
        offset: i64,
        limit: i64,
        group: Option<i64>,
    ) -> fdo::Result<Vec<SummaryDto>> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        let rows = self
            .store
            .query(offset, limit, group)
            .await
            .map_err(dbus_err)?;
        Ok(rows.into_iter().map(SummaryDto::from).collect())
    }

    pub async fn search(&self, query: &str, limit: i64) -> fdo::Result<Vec<SummaryDto>> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        let rows = self.store.search(query, limit).await.map_err(dbus_err)?;
        Ok(rows.into_iter().map(SummaryDto::from).collect())
    }

    pub async fn get_entry(&self, id: i64) -> fdo::Result<Option<EntryDto>> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        Ok(self
            .store
            .get_entry(id)
            .await
            .map_err(dbus_err)?
            .map(EntryDto::from))
    }

    /// Offer an entry back onto the clipboard; optionally auto-paste (X11 only).
    pub async fn pick(&self, id: i64, auto_paste: bool) -> fdo::Result<()> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        let entry = self
            .store
            .get_entry(id)
            .await
            .map_err(dbus_err)?
            .ok_or_else(|| fdo::Error::Failed(format!("no entry {id}")))?;
        self.provider
            .offer(&entry)
            .await
            .map_err(|e| fdo::Error::Failed(format!("offer: {e}")))?;
        self.store.bump_use_count(id).await.map_err(dbus_err)?;
        if auto_paste && self.provider.capabilities().auto_paste {
            if let Some(ap) = &self.auto_paste {
                ap();
            }
        }
        Ok(())
    }

    /// Used by the GNOME Shell extension (and external producers) to push a
    /// capture. The manager policy (pause/exclude/size) is applied server-side.
    pub async fn add_entry(&self, targets: Vec<TargetDto>) -> fdo::Result<i64> {
        let state = self.state.lock().await;
        if state.locked {
            return Err(fdo::Error::Failed("database is locked".into()));
        }
        if state.paused {
            return Err(fdo::Error::Failed("capture is paused".into()));
        }
        let max_blob = 50 * 1024 * 1024u64;
        drop(state);

        let mimes: Vec<cling_common::MimeBlob> = targets.into_iter().map(Into::into).collect();
        let source = cling_common::AppId::default();
        let id = self
            .store
            .add_capture(mimes, source, max_blob)
            .await
            .map_err(dbus_err)?;
        self.emit_entry_added(id).await;
        Ok(id)
    }

    pub async fn delete(&self, ids: Vec<i64>) -> fdo::Result<()> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        self.store.delete(&ids).await.map_err(dbus_err)?;
        for id in ids {
            self.emit_entry_removed(id).await;
        }
        Ok(())
    }

    pub async fn set_pinned(&self, id: i64, pinned: bool) -> fdo::Result<()> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        self.store.set_pinned(id, pinned).await.map_err(dbus_err)
    }

    pub async fn set_group(&self, id: i64, group: Option<i64>) -> fdo::Result<()> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        self.store.set_group(id, group).await.map_err(dbus_err)
    }

    pub async fn clear(&self) -> fdo::Result<()> {
        let state = self.state.lock().await;
        self.ensure_unlocked(&state).await?;
        drop(state);
        self.store.clear().await.map_err(dbus_err)
    }

    pub async fn pause(&self, paused: bool) -> fdo::Result<()> {
        let p;
        let l;
        {
            let mut state = self.state.lock().await;
            state.paused = paused;
            p = state.paused;
            l = state.locked;
        }
        self.emit_state_changed(p, l).await;
        Ok(())
    }

    pub async fn exclude_app_add(&self, app: &str) -> fdo::Result<()> {
        self.state.lock().await.excluded_apps.push(app.to_string());
        Ok(())
    }

    pub async fn exclude_app_remove(&self, app: &str) -> fdo::Result<()> {
        let mut state = self.state.lock().await;
        state.excluded_apps.retain(|a| a != app);
        Ok(())
    }

    pub async fn exclude_content_regex_add(&self, pattern: &str) -> fdo::Result<()> {
        regex::Regex::new(pattern).map_err(|e| fdo::Error::Failed(format!("bad regex: {e}")))?;
        self.state.lock().await.deny_regex.push(pattern.to_string());
        Ok(())
    }

    pub async fn exclude_content_regex_remove(&self, pattern: &str) -> fdo::Result<()> {
        let mut state = self.state.lock().await;
        state.deny_regex.retain(|p| p != pattern);
        Ok(())
    }

    pub async fn lock(&self) -> fdo::Result<()> {
        let p;
        {
            let mut state = self.state.lock().await;
            state.locked = true;
            p = state.paused;
        }
        self.emit_state_changed(p, true).await;
        Ok(())
    }

    /// Triggers unlock via the injected handler (passphrase stays off the bus).
    pub async fn request_unlock(&self) -> fdo::Result<bool> {
        let outcome = self
            .unlock
            .request()
            .await
            .map_err(|e| fdo::Error::Failed(format!("unlock: {e}")))?;
        match outcome {
            UnlockOutcome::Unlocked => {
                let p;
                {
                    let mut state = self.state.lock().await;
                    state.locked = false;
                    p = state.paused;
                }
                self.emit_unlocked().await;
                self.emit_state_changed(p, false).await;
                Ok(true)
            }
            UnlockOutcome::Cancelled => Ok(false),
            UnlockOutcome::Rejected => Err(fdo::Error::Failed("incorrect passphrase".into())),
        }
    }

    pub async fn state(&self) -> fdo::Result<StateDto> {
        let state = self.state.lock().await;
        Ok(StateDto {
            paused: state.paused,
            locked: state.locked,
            backend: self.provider.name().to_string(),
            entry_count: if state.locked {
                -1
            } else {
                self.store.count().await.unwrap_or(-1)
            },
            excluded_apps: state.excluded_apps.clone(),
        })
    }
}

fn dbus_err(e: cling_core::StoreError) -> fdo::Error {
    match e {
        cling_core::StoreError::Locked => fdo::Error::Failed("database is locked".into()),
        cling_core::StoreError::Db(m) => fdo::Error::Failed(m),
    }
}
