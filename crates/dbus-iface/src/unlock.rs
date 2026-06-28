//! Unlock abstraction. The passphrase never crosses the D-Bus session bus; the
//! daemon injects a concrete [`UnlockRequest`] that collects it over a private
//! same-UID channel and re-opens the store in-process.

use async_trait::async_trait;

/// Outcome of an unlock attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockOutcome {
    Unlocked,
    Cancelled,
    Rejected,
}

pub type UnlockResult = anyhow::Result<UnlockOutcome>;

/// Implemented by the daemon's unlock mechanism and injected into the service.
#[async_trait]
pub trait UnlockRequest: Send + Sync {
    async fn request(&self) -> UnlockResult;
}

/// Default handler used when no unlock mechanism is wired (e.g. encryption off).
/// Reports "not configured" rather than silently unlocking.
pub struct NoUnlock;

#[async_trait]
impl UnlockRequest for NoUnlock {
    async fn request(&self) -> UnlockResult {
        anyhow::bail!("no unlock mechanism configured (encryption off or unavailable)")
    }
}
