//! Tests for the re-openable StoreHandle (P4 unlock path): a pending handle
//! reports `Locked` for everything, reopens with the right passphrase, and
//! rejects a wrong one.

use cling_common::{AppId, MimeBlob};
use cling_core::HistoryStore;
use cling_store::StoreHandle;

fn t(mime: &str, b: &[u8]) -> MimeBlob {
    MimeBlob {
        mime: mime.into(),
        bytes: b.to_vec(),
    }
}

#[tokio::test]
async fn pending_handle_reports_locked() {
    let dir = std::env::temp_dir().join(format!("cling-handle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pending.clingdb").to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    // Create an encrypted DB first (so a wrong key is actually rejected).
    {
        let _ = cling_store::Store::open(&path, Some("correct")).unwrap();
    }

    let h = StoreHandle::pending(path.clone());
    assert!(!h.is_open().await);
    // Every op must report Locked while closed.
    assert!(matches!(
        h.add_capture(vec![t("text/plain", b"x")], AppId::default(), u64::MAX)
            .await
            .err(),
        Some(cling_core::StoreError::Locked)
    ));
    assert!(matches!(
        h.count().await.err(),
        Some(cling_core::StoreError::Locked)
    ));

    // Wrong passphrase → reopen fails (existing encrypted DB).
    let err = h.reopen("nope").await.unwrap_err();
    assert!(matches!(
        err,
        cling_core::StoreError::Locked | cling_core::StoreError::Db(_)
    ));

    // Right passphrase → opens.
    h.reopen("correct").await.unwrap();
    assert!(h.is_open().await);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[tokio::test]
async fn handle_reopen_round_trip() {
    let dir = std::env::temp_dir().join(format!("cling-handle2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("reopen.clingdb").to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    // Create the encrypted DB first.
    {
        let store = cling_store::Store::open(&path, Some("hunter2")).unwrap();
        let store_h = StoreHandle::opened(store, Some(path.clone()));
        let id = store_h
            .add_capture(vec![t("text/plain", b"secret")], AppId::default(), u64::MAX)
            .await
            .unwrap();
        assert!(store_h.get_entry(id).await.unwrap().is_some());
    }

    // Now start "locked": pending handle, reopen with the right passphrase.
    let h = StoreHandle::pending(path.clone());
    assert!(!h.is_open().await);
    h.reopen("hunter2").await.unwrap();
    assert!(h.is_open().await);

    let rows = h.query(0, 10, None).await.unwrap();
    assert_eq!(rows.len(), 1);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}
