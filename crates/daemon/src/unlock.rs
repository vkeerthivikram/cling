//! Secure unlock wiring.
//!
//! `RequestUnlock()` on D-Bus is routed to a [`GuiUnlock`] which:
//!   1. Binds a private `UnixListener` under `$XDG_RUNTIME_DIR` (0700, same-UID).
//!   2. Spawns `cling-show --socket <path>` (a passphrase dialog).
//!   3. Reads the passphrase the dialog writes back over that socket.
//!   4. Re-opens the [`StoreHandle`] in-process with it.
//!
//! The passphrase therefore never crosses the D-Bus session bus. The socket
//! path lives in the runtime dir, which is mode-0700 per-user, so only the same
//! UID can connect.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use cling_core::StoreError;
use cling_dbus_iface::{UnlockOutcome, UnlockRequest, UnlockResult};
use cling_store::StoreHandle;

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(120);

/// GUI-based unlock: spawns `cling-show --socket <path>` and re-opens the store.
pub struct GuiUnlock {
    handle: StoreHandle,
    /// Path to the `cling-show` binary (defaults to `cling-show` on PATH).
    show_binary: String,
}

impl GuiUnlock {
    pub fn new(handle: StoreHandle, show_binary: String) -> Self {
        GuiUnlock {
            handle,
            show_binary,
        }
    }

    fn socket_path() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Ok(base.join(format!("cling-unlock-{}.sock", std::process::id())))
    }
}

#[async_trait]
impl UnlockRequest for GuiUnlock {
    async fn request(&self) -> UnlockResult {
        let path = Self::socket_path()?;
        let _ = std::fs::remove_file(&path);

        let listener = tokio::net::UnixListener::bind(&path)
            .map_err(|e| anyhow::anyhow!("bind unlock socket: {e}"))?;
        // Best-effort restrictive perms (XDG_RUNTIME_DIR is already 0700).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        // Spawn the unlock dialog.
        let mut cmd = tokio::process::Command::new(&self.show_binary);
        cmd.arg("--socket").arg(&path);
        let _child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn cling-show --unlock: {e} (is it on PATH?)"))?;

        // Accept one connection (with a timeout), then read the passphrase.
        let accept = tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept()).await;
        let _ = std::fs::remove_file(&path);
        let (mut stream, _addr) = match accept {
            Ok(Ok(s)) => s,
            _ => return Ok(UnlockOutcome::Cancelled),
        };

        use tokio::io::AsyncReadExt;
        let mut buf = Vec::with_capacity(256);
        stream.read_to_end(&mut buf).await.ok();
        // The dialog writes the passphrase (no trailing newline) then closes.
        let passphrase = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        if passphrase.is_empty() {
            return Ok(UnlockOutcome::Cancelled);
        }

        match self.handle.reopen(&passphrase).await {
            Ok(()) => Ok(UnlockOutcome::Unlocked),
            Err(StoreError::Locked) => Ok(UnlockOutcome::Rejected),
            Err(e) => Err(anyhow::anyhow!("reopen: {e}")),
        }
    }
}
