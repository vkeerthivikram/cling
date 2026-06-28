//! Tests for the capture manager + mock backend: capture→store flow, pause,
//! and exclude-by-app.

use std::sync::Arc;

use cling_backends::mock::MockBackend;
use cling_common::{AppId, Caps, MimeBlob};
use cling_core::{ClipboardManager, ClipboardProvider, ContentRules, HistoryStore};
use cling_store::Store;

fn t(mime: &str, b: &[u8]) -> MimeBlob {
    MimeBlob {
        mime: mime.into(),
        bytes: b.to_vec(),
    }
}
fn src(id: &str) -> AppId {
    AppId {
        id: Some(id.into()),
        label: Some(id.into()),
    }
}

async fn harness() -> (MockBackend, Arc<Store>) {
    let store = Arc::new(Store::open_memory(None).unwrap());
    let backend = MockBackend::new(
        "mock",
        Caps {
            silent_history: true,
            auto_paste: false,
            source_id: true,
        },
    );
    (backend, store)
}

#[tokio::test]
async fn manager_captures_selection() {
    let (backend, store) = harness().await;
    let manager = ClipboardManager::new();
    let provider: Arc<dyn ClipboardProvider> = Arc::new(backend.clone());
    let store_h: Arc<dyn HistoryStore> = store.clone();
    let h = manager.clone();
    tokio::spawn(async move { h.run(provider, store_h).await });

    backend
        .push_selection(vec![t("text/plain", b"hello")], src("gedit"))
        .await;
    // Let the loop process.
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if store.count().await.unwrap() >= 1 {
            break;
        }
    }
    assert_eq!(store.count().await.unwrap(), 1);
}

#[tokio::test]
async fn manager_drops_when_paused() {
    let (backend, store) = harness().await;
    let manager = ClipboardManager::new();
    manager.set_paused(true).await;
    let provider: Arc<dyn ClipboardProvider> = Arc::new(backend.clone());
    let store_h: Arc<dyn HistoryStore> = store.clone();
    let h = manager.clone();
    tokio::spawn(async move { h.run(provider, store_h).await });

    backend
        .push_selection(vec![t("text/plain", b"paused-capture")], src("a"))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(store.count().await.unwrap(), 0);

    // Unpause and try again.
    manager.set_paused(false).await;
    backend
        .push_selection(vec![t("text/plain", b"after-unpause")], src("a"))
        .await;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if store.count().await.unwrap() >= 1 {
            break;
        }
    }
    assert_eq!(store.count().await.unwrap(), 1);
}

#[tokio::test]
async fn manager_excludes_app() {
    let (backend, store) = harness().await;
    let manager = ClipboardManager::new();
    manager.set_excluded_apps(vec!["keepassxc".into()]).await;
    let provider: Arc<dyn ClipboardProvider> = Arc::new(backend.clone());
    let store_h: Arc<dyn HistoryStore> = store.clone();
    let h = manager.clone();
    tokio::spawn(async move { h.run(provider, store_h).await });

    backend
        .push_selection(vec![t("text/plain", b"password")], src("keepassxc"))
        .await;
    backend
        .push_selection(vec![t("text/plain", b"normal")], src("firefox"))
        .await;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if store.count().await.unwrap() >= 1 {
            break;
        }
    }
    let rows = store.query(0, 10, None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].origin.as_deref(), Some("firefox"));
}

#[tokio::test]
async fn manager_content_denylist() {
    let (backend, store) = harness().await;
    let manager = ClipboardManager::new();
    manager
        .set_content_rules(ContentRules {
            deny_regex: vec!["SECRET-TOKEN-\\w+".into()],
        })
        .await;
    let provider: Arc<dyn ClipboardProvider> = Arc::new(backend.clone());
    let store_h: Arc<dyn HistoryStore> = store.clone();
    let h = manager.clone();
    tokio::spawn(async move { h.run(provider, store_h).await });

    backend
        .push_selection(vec![t("text/plain", b"SECRET-TOKEN-1234")], src("a"))
        .await;
    backend
        .push_selection(vec![t("text/plain", b"innocuous text")], src("a"))
        .await;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if store.count().await.unwrap() >= 1 {
            break;
        }
    }
    assert_eq!(store.count().await.unwrap(), 1);
}
