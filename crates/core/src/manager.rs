//! The clipboard manager: wires a `ClipboardProvider` to a `HistoryStore` and
//! applies capture policy (size limits, exclude rules, pause).

use cling_common::{AppId, Capture, ClipboardEvent, MimeBlob};
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{BackendError, ClipboardProvider, HistoryStore};

/// Policy applied to every captured selection.
#[derive(Debug, Clone)]
pub struct CapturePolicy {
    /// Skip the whole capture if any single target blob exceeds this size.
    pub max_blob_bytes: u64,
    /// App ids whose captures are dropped entirely (password managers, etc.).
    pub excluded_apps: Vec<String>,
    /// If true, drop all captures (global pause).
    pub paused: bool,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        CapturePolicy {
            max_blob_bytes: 50 * 1024 * 1024, // 50 MiB
            excluded_apps: Vec::new(),
            paused: false,
        }
    }
}

/// Rules for matching content (used where source-app identity is unavailable,
/// i.e. wlroots/KDE Wayland, and as an additional denylist everywhere).
#[derive(Debug, Clone, Default)]
pub struct ContentRules {
    /// Substrings/regexes that, if found in text/plain, drop the capture.
    pub deny_regex: Vec<String>,
}

/// Owns the live capture loop. Cloned handles share the same policy.
#[derive(Clone)]
pub struct ClipboardManager {
    policy: Arc<Mutex<CapturePolicy>>,
    content_rules: Arc<Mutex<ContentRules>>,
}

impl ClipboardManager {
    pub fn new() -> Self {
        ClipboardManager {
            policy: Arc::new(Mutex::new(CapturePolicy::default())),
            content_rules: Arc::new(Mutex::new(ContentRules::default())),
        }
    }

    pub async fn policy(&self) -> CapturePolicy {
        self.policy.lock().await.clone()
    }

    pub async fn set_paused(&self, paused: bool) {
        self.policy.lock().await.paused = paused;
    }

    pub async fn set_excluded_apps(&self, apps: Vec<String>) {
        self.policy.lock().await.excluded_apps = apps;
    }

    pub async fn set_content_rules(&self, rules: ContentRules) {
        *self.content_rules.lock().await = rules;
    }

    /// Decide whether a capture should be persisted, given current policy.
    /// Returns the (possibly filtered) targets to store, or `None` to drop.
    pub async fn filter_capture(
        &self,
        mut targets: Vec<MimeBlob>,
        source: &AppId,
    ) -> Option<Vec<MimeBlob>> {
        let policy = self.policy.lock().await.clone();
        if policy.paused {
            tracing::debug!("capture dropped: paused");
            return None;
        }
        // Exclude by app id (X11 / GNOME only).
        if let Some(id) = &source.id {
            let idl = id.to_ascii_lowercase();
            if policy
                .excluded_apps
                .iter()
                .any(|a| a.to_ascii_lowercase() == idl)
            {
                tracing::debug!(app = %id, "capture dropped: excluded app");
                return None;
            }
        }
        // Size guard: drop any oversized single target, and if that empties the
        // set, drop the capture.
        targets.retain(|t| (t.bytes.len() as u64) <= policy.max_blob_bytes);
        if targets.is_empty() {
            return None;
        }
        // Content denylist (text only).
        let rules = self.content_rules.lock().await.clone();
        if !rules.deny_regex.is_empty() {
            if let Some(text) = text_plain(&targets) {
                if rules.deny_regex.iter().any(|p| {
                    regex::Regex::new(p)
                        .map(|re| re.is_match(&text))
                        .unwrap_or(false)
                }) {
                    tracing::debug!("capture dropped: content denylist");
                    return None;
                }
            }
        }
        Some(targets)
    }

    /// Run the capture loop until the event stream ends. This is what the
    /// daemon spawns at startup.
    pub async fn run(self, provider: Arc<dyn ClipboardProvider>, store: Arc<dyn HistoryStore>) {
        tracing::info!(backend = provider.name(), "clipboard manager loop started");
        let mut events = provider.subscribe();
        while let Some(ev) = events.next().await {
            match ev {
                ClipboardEvent::SelectionChanged { source } => {
                    let targets = match provider.read_targets().await {
                        Ok(t) => t,
                        Err(BackendError::Unavailable(msg)) => {
                            tracing::debug!(error = %msg, "no targets available");
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to read targets");
                            continue;
                        }
                    };
                    let Some(targets) = self.filter_capture(targets, &source).await else {
                        continue;
                    };
                    match store
                        .add_capture(
                            targets,
                            source.clone(),
                            self.policy.lock().await.max_blob_bytes,
                        )
                        .await
                    {
                        Ok(id) => tracing::debug!(entry = id, "captured"),
                        Err(e) => tracing::warn!(error = %e, "store add failed"),
                    }
                }
                ClipboardEvent::Cleared => {
                    tracing::debug!("clipboard cleared");
                }
            }
        }
        tracing::info!("clipboard manager loop ended");
    }
}

impl Default for ClipboardManager {
    fn default() -> Self {
        Self::new()
    }
}

fn text_plain(targets: &[MimeBlob]) -> Option<String> {
    targets
        .iter()
        .find(|t| {
            t.mime.eq_ignore_ascii_case("text/plain;charset=utf-8")
                || t.mime.eq_ignore_ascii_case("text/plain")
        })
        .and_then(|t| String::from_utf8(t.bytes.clone()).ok())
}

/// Convenience for producers that want to build a [`Capture`] inline.
pub fn capture(targets: Vec<MimeBlob>, source: AppId) -> Capture {
    Capture {
        targets,
        source,
        ts: None,
    }
}
