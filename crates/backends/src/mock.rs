//! An in-process `ClipboardProvider` used by tests and the headless harness.
//!
//! Fully controllable: push selections and read back what was last offered, so
//! the manager logic can be exercised without any display server.

use std::sync::Arc;

use cling_common::{AppId, Caps, ClipboardEvent, Entry, MimeBlob};
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;

use cling_core::{BackendError, ClipboardProvider};

#[derive(Default, Clone)]
struct State {
    current: Option<Vec<MimeBlob>>,
    source: AppId,
    offered: Option<Vec<MimeBlob>>,
}

/// A controllable mock backend.
#[derive(Clone)]
pub struct MockBackend {
    caps: Caps,
    name: &'static str,
    tx: mpsc::UnboundedSender<ClipboardEvent>,
    /// Held in a std mutex so the sync `subscribe()` can take it without
    /// blocking the async runtime.
    rx_slot: Arc<std::sync::Mutex<Option<mpsc::UnboundedReceiver<ClipboardEvent>>>>,
    state: Arc<Mutex<State>>,
}

impl MockBackend {
    pub fn new(name: &'static str, caps: Caps) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        MockBackend {
            caps,
            name,
            tx,
            rx_slot: Arc::new(std::sync::Mutex::new(Some(rx))),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Push a new selection (simulates another app copying).
    pub async fn push_selection(&self, targets: Vec<MimeBlob>, source: AppId) {
        {
            let mut s = self.state.lock().await;
            s.current = Some(targets);
            s.source = source.clone();
        }
        let _ = self.tx.send(ClipboardEvent::SelectionChanged { source });
    }

    pub async fn clear(&self) {
        self.state.lock().await.current = None;
        let _ = self.tx.send(ClipboardEvent::Cleared);
    }

    /// What was last offered onto the clipboard by `offer()`.
    pub async fn last_offered(&self) -> Option<Vec<MimeBlob>> {
        self.state.lock().await.offered.clone()
    }
}

#[async_trait::async_trait]
impl ClipboardProvider for MockBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> Caps {
        self.caps
    }

    fn subscribe(&self) -> BoxStream<'static, ClipboardEvent> {
        let rx = self.rx_slot.lock().unwrap().take();
        match rx {
            Some(r) => UnboundedReceiverStream::new(r).boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    async fn read_targets(&self) -> Result<Vec<MimeBlob>, BackendError> {
        match &self.state.lock().await.current {
            Some(t) => Ok(t.clone()),
            None => Err(BackendError::Unavailable("no current selection".into())),
        }
    }

    async fn offer(&self, entry: &Entry) -> Result<(), BackendError> {
        let mut s = self.state.lock().await;
        s.offered = Some(entry.targets.clone());
        s.current = Some(entry.targets.clone());
        Ok(())
    }

    async fn source_hint(&self) -> Result<AppId, BackendError> {
        Ok(self.state.lock().await.source.clone())
    }
}
