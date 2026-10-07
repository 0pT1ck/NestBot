use nestbot::{
    domain::{ClaimRecord, Entry, JobPayload},
    storage::{Store, vault::Vault},
};
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let vault =
        Arc::new(Vault::open(&dir.path().join("master.key"), "synthetic-password").unwrap());
    let store = Store::open(&dir.path().join("test.sqlite"), vault, 1024, 20).unwrap();
    (dir, store)
}

#[tokio::test]
async fn repeated_search_merges_entries_but_keeps_page_and_message_from_same_run() {
    let (_dir, store) = fixture();
    let shared = Entry {
        key: "synthetic-shared".into(),
        ..Default::default()
    };
    let a = store
        .save_page("synthetic", 12, Some(120), vec![shared.clone()])
        .await
        .unwrap();
    let b = store
        .save_page(
            "synthetic",
            6,
            Some(260),
            vec![
                shared,
                Entry {
                    key: "synthetic-new".into(),
                    ..Default::default()
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(store.batches(20, 0).await.unwrap().len(), 1);
    assert_eq!(store.entries(&a, 1, 20).await.unwrap().len(), 2);
    assert_eq!(
        store.cursor("synthetic").await.unwrap(),
        Some((6, Some(260)))
    );
    store
        .save_page(
            "synthetic",
            7,
            Some(260),
            vec![Entry {
                key: "synthetic-next".into(),
                ..Default::default()
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        store.cursor("synthetic").await.unwrap(),
        Some((7, Some(260)))
    );
    assert_eq!(store.entries(&a, 1, 20).await.unwrap()[2].0, 3);
    // Offline imports have no result message and must preserve a live cursor.
    store
        .save_page(
            "synthetic",
            0,
            None,
            vec![Entry {
                key: "synthetic-import".into(),
                ..Default::default()
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        store.cursor("synthetic").await.unwrap(),
        Some((7, Some(260)))
    );
}

#[tokio::test]
async fn new_keyword_inherits_only_matching_completed_keys() {
    let (_dir, store) = fixture();
    let shared = Entry {
        key: "synthetic-shared".into(),
        file_count: Some(1),
        ..Default::default()
    };
    let new = Entry {
        key: "synthetic-new".into(),
        ..Default::default()
    };
    store
        .save_page("old-keyword", 1, Some(10), vec![shared.clone()])
        .await
        .unwrap();
    let batch = store
        .save_page(
            "new-keyword",
            1,
            Some(20),
            vec![shared.clone(), new.clone()],
        )
        .await
        .unwrap();
    let mut done = ClaimRecord {
        status: "done".into(),
        ..Default::default()
    };
    done.add("document:42");
    store.save_claim(&shared.payload(), &done).await.unwrap();
    assert!(
        store
            .batch_entry_complete(&batch, 1, &shared)
            .await
            .unwrap()
    );
    assert!(!store.batch_entry_complete(&batch, 2, &new).await.unwrap());
    assert_eq!(store.batch_next_pending(&batch).await.unwrap(), Some(2));
}

#[tokio::test]
async fn partial_search_retains_report_and_can_be_retried() {
    let (_dir, store) = fixture();
    let payload = JobPayload::Search {
        keyword: "synthetic".into(),
        pages: None,
        sort: None,
        resume: false,
    };
    let id = store.enqueue(payload, None, None).await.unwrap();
    store.warning(&id, "search_page_stalled").await.unwrap();
    store.finish(&id, "partial", None, None).await.unwrap();
    assert_eq!(store.job(&id).await.unwrap().unwrap().status, "partial");
    assert_eq!(
        store.report(&id).await.unwrap().warnings,
        vec!["search_page_stalled"]
    );
    store.retry(&id, false).await.unwrap();
    assert_eq!(store.job(&id).await.unwrap().unwrap().status, "queued");
}
