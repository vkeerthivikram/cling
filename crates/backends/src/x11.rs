//! X11 clipboard backend.
//!
//! Full parity on X11: silent history capture (XFIXES selection notifications),
//! copy-from-history (owns the CLIPBOARD selection and answers SelectionRequest),
//! source-app identity (WM_CLASS / `_NET_WM_PID` of the selection owner), and
//! auto-paste (Ctrl+V synthesised via XTEST).
//!
//! X11 is a blocking event-loop model, so this backend runs a dedicated OS
//! thread owning the `x11rb` connection and bridges to the async world through
//! channels. Capture is eager: on a selection-change notification the thread
//! reads all targets + data immediately and caches them; `read_targets()`
//! returns that cache (the manager's event→read flow).
//!
//! Runtime note: needs a real X server (or Xephyr) to exercise; covered by
//! integration tests where a display is available.

use std::sync::mpsc as std_mpsc;

use cling_common::{AppId, Caps, ClipboardEvent, Entry, MimeBlob};
use futures::stream::BoxStream;
use futures::stream::StreamExt as _;
use tokio::sync::{oneshot, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;

use cling_core::{BackendError, ClipboardProvider};

/// Commands from the async side to the X11 event thread.
#[allow(dead_code)]
pub(super) enum XCmd {
    Offer(Vec<MimeBlob>, oneshot::Sender<Result<(), BackendError>>),
    ReadTargets(oneshot::Sender<Result<Vec<MimeBlob>, BackendError>>),
    SourceHint(oneshot::Sender<AppId>),
    AutoPaste(oneshot::Sender<Result<(), BackendError>>),
    Shutdown,
}

#[derive(Clone, Default)]
pub(super) struct Cache {
    targets: Option<Vec<MimeBlob>>,
    source: AppId,
}

/// The X11 backend handle (cheap to clone).
#[derive(Clone)]
pub struct X11Backend {
    caps: Caps,
    tx: std_mpsc::Sender<XCmd>,
    rx_slot: std::sync::Arc<
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<ClipboardEvent>>>,
    >,
    cache: std::sync::Arc<Mutex<Cache>>,
}

impl X11Backend {
    /// Connect to the X display in `$DISPLAY` and start the event thread.
    pub fn connect() -> Result<Self, BackendError> {
        let (display, screen_num) = x11rb::connect(None)
            .map_err(|e| BackendError::Unavailable(format!("X connect: {e}")))?;
        let caps = Caps {
            silent_history: true,
            auto_paste: true,
            source_id: true,
        };
        let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<ClipboardEvent>();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<XCmd>();
        let cache = std::sync::Arc::new(Mutex::new(Cache::default()));

        let cache_t = cache.clone();
        std::thread::Builder::new()
            .name("cling-x11".into())
            .spawn(move || {
                crate::x11::thread::run_loop(display, screen_num, cache_t, evt_tx, cmd_rx);
            })
            .map_err(|e| BackendError::Unavailable(format!("spawn x11 thread: {e}")))?;

        Ok(X11Backend {
            caps,
            tx: cmd_tx,
            rx_slot: std::sync::Arc::new(std::sync::Mutex::new(Some(evt_rx))),
            cache,
        })
    }

    async fn cmd<R>(&self, make: impl FnOnce(oneshot::Sender<R>) -> XCmd) -> Result<R, BackendError>
    where
        R: Send + 'static,
    {
        let (otx, orx) = oneshot::channel();
        self.tx
            .send(make(otx))
            .map_err(|_| BackendError::Unavailable("x11 thread gone".into()))?;
        orx.await
            .map_err(|_| BackendError::Unavailable("x11 thread dropped reply".into()))
    }
}

#[async_trait::async_trait]
impl ClipboardProvider for X11Backend {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn capabilities(&self) -> Caps {
        self.caps
    }

    fn subscribe(&self) -> BoxStream<'static, ClipboardEvent> {
        match self.rx_slot.lock().unwrap().take() {
            Some(r) => UnboundedReceiverStream::new(r).boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    async fn read_targets(&self) -> Result<Vec<MimeBlob>, BackendError> {
        if let Some(t) = self.cache.lock().await.targets.clone() {
            return Ok(t);
        }
        self.cmd(|otx| XCmd::ReadTargets(otx)).await?
    }

    async fn offer(&self, entry: &Entry) -> Result<(), BackendError> {
        let targets = entry.targets.clone();
        self.cache.lock().await.targets = Some(targets.clone());
        self.cmd(|otx| XCmd::Offer(targets, otx)).await?
    }

    async fn source_hint(&self) -> Result<AppId, BackendError> {
        Ok(self.cache.lock().await.source.clone())
    }
}

/// Synthesize Ctrl+V via XTEST (auto-paste). Only meaningful on X11.
pub async fn auto_paste(backend: &X11Backend) -> Result<(), BackendError> {
    backend.cmd(|otx| XCmd::AutoPaste(otx)).await?
}

pub mod thread;
