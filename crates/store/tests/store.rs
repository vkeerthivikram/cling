//! Tests for the history store: schema/migration, full-fidelity round-trip,
//! consecutive dedup, FTS5 search, pinning, pruning, and SQLCipher locking.

use cling_common::{AppId, MimeBlob};
use cling_core::HistoryStore;
use cling_store::Store;

fn t(mime: &str, bytes: &[u8]) -> MimeBlob {
    MimeBlob {
        mime: mime.to_string(),
        bytes: bytes.to_vec(),
    }
}

fn src(id: &str) -> AppId {
    AppId {
        id: Some(id.into()),
        label: Some(id.into()),
    }
}

#[tokio::test]
async fn roundtrip_text_and_rich_full_fidelity() {
    let s = Store::open_memory(None).unwrap();
    let id = s
        .add_capture(
            vec![
                t("text/plain;charset=utf-8", b"hello world"),
                t("text/html", b"<b>hello</b> world"),
            ],
            src("gedit"),
            u64::MAX,
        )
        .await
        .unwrap();

    let entry = s.get_entry(id).await.unwrap().expect("entry exists");
    assert_eq!(entry.targets.len(), 2);
    // Kinds: both text targets present and verbatim.
    let plain = entry.text().unwrap();
    assert_eq!(plain, "hello world");
    assert!(entry.targets.iter().any(|x| x.mime == "text/html"));
    assert_eq!(entry.origin.as_deref(), Some("gedit"));
}

#[tokio::test]
async fn consecutive_identical_capture_dedups() {
    let s = Store::open_memory(None).unwrap();
    let id1 = s
        .add_capture(vec![t("text/plain", b"same")], src("a"), u64::MAX)
        .await
        .unwrap();
    let id2 = s
        .add_capture(vec![t("text/plain", b"same")], src("a"), u64::MAX)
        .await
        .unwrap();
    // Same content back-to-back should bump, not create a new row.
    assert_eq!(id1, id2);
    assert_eq!(s.count().await.unwrap(), 1);
    let entry = s.get_entry(id1).await.unwrap().unwrap();
    assert_eq!(entry.use_count, 2);
}

#[tokio::test]
async fn different_capture_does_not_dedup() {
    let s = Store::open_memory(None).unwrap();
    let _ = s
        .add_capture(vec![t("text/plain", b"aaa")], src("a"), u64::MAX)
        .await;
    let _ = s
        .add_capture(vec![t("text/plain", b"bbb")], src("a"), u64::MAX)
        .await;
    assert_eq!(s.count().await.unwrap(), 2);
}

#[tokio::test]
async fn fts_search_ranks_and_matches() {
    let s = Store::open_memory(None).unwrap();
    for w in ["alpha bravo", "charlie delta", "bravo zulu"] {
        let _ = s
            .add_capture(vec![t("text/plain", w.as_bytes())], src("a"), u64::MAX)
            .await;
    }
    let res = s.search("bravo", 10).await.unwrap();
    let previews: Vec<String> = res.into_iter().filter_map(|r| r.preview_text).collect();
    assert!(previews.iter().any(|p| p.contains("alpha bravo")));
    assert!(previews.iter().any(|p| p.contains("bravo zulu")));
    assert!(!previews.iter().any(|p| p.contains("charlie")));
}

#[tokio::test]
async fn pinned_survives_prune() {
    let s = Store::open_memory(None).unwrap();
    let pinned = s
        .add_capture(vec![t("text/plain", b"keep me")], src("a"), u64::MAX)
        .await
        .unwrap();
    s.set_pinned(pinned, true).await.unwrap();
    for i in 0..20 {
        let _ = s
            .add_capture(
                vec![t("text/plain", format!("x{i}").as_bytes())],
                src("a"),
                u64::MAX,
            )
            .await;
    }
    let removed = s.prune(5, None).await.unwrap();
    assert!(removed > 0);
    assert!(s.get_entry(pinned).await.unwrap().is_some());
}

#[tokio::test]
async fn encryption_round_trip_and_wrong_key_locked() {
    // SQLCipher: a fresh DB accepts any key (it just creates a new encrypted DB),
    // so we must persist to a file to test wrong-key rejection.
    let dir = std::env::temp_dir().join(format!("cling-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("enc.clingdb");
    let _ = std::fs::remove_file(&path);

    // Create + write with the correct passphrase.
    {
        let s = Store::open(path.to_str().unwrap(), Some("correct horse battery staple")).unwrap();
        let id = s
            .add_capture(vec![t("text/plain", b"secret")], src("a"), u64::MAX)
            .await
            .unwrap();
        assert!(s.get_entry(id).await.unwrap().is_some());
    }

    // Reopen with the WRONG passphrase must fail (cannot decrypt).
    let bad = Store::open(path.to_str().unwrap(), Some("wrong key"));
    assert!(
        bad.is_err(),
        "wrong passphrase must fail to open an existing encrypted DB"
    );

    // Reopen with the correct passphrase still works.
    let s = Store::open(path.to_str().unwrap(), Some("correct horse battery staple")).unwrap();
    let rows = s.query(0, 10, None).await.unwrap();
    assert_eq!(rows.len(), 1);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[tokio::test]
async fn delete_and_clear() {
    let s = Store::open_memory(None).unwrap();
    let a = s
        .add_capture(vec![t("text/plain", b"a")], src("x"), u64::MAX)
        .await
        .unwrap();
    let b = s
        .add_capture(vec![t("text/plain", b"b")], src("x"), u64::MAX)
        .await
        .unwrap();
    s.delete(&[a]).await.unwrap();
    assert!(s.get_entry(a).await.unwrap().is_none());
    assert!(s.get_entry(b).await.unwrap().is_some());
    s.clear().await.unwrap();
    assert_eq!(s.count().await.unwrap(), 0);
}

#[tokio::test]
async fn query_lists_newest_first() {
    let s = Store::open_memory(None).unwrap();
    for w in ["first", "second", "third"] {
        let _ = s
            .add_capture(vec![t("text/plain", w.as_bytes())], src("a"), u64::MAX)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let rows = s.query(0, 10, None).await.unwrap();
    let previews: Vec<String> = rows.into_iter().filter_map(|r| r.preview_text).collect();
    assert_eq!(previews[0], "third");
    assert_eq!(previews.last().unwrap(), &"first");
}
