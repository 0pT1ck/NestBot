use nestbot::{
    application::App,
    config::Config,
    domain::{Entry, JobPayload, TransferMode},
    storage::{Store, vault::Vault},
};
use std::sync::Arc;

fn fixture(max_queue: u32) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let vault =
        Arc::new(Vault::open(&dir.path().join("master.key"), "synthetic-vault-password").unwrap());
    let store = Store::open(&dir.path().join("test.sqlite"), vault, 1024, max_queue).unwrap();
    (dir, store)
}

fn search(keyword: &str) -> JobPayload {
    JobPayload::Search {
        keyword: keyword.into(),
        pages: Some(1),
        sort: None,
        resume: false,
    }
}

#[test]
fn authenticated_encryption_rejects_wrong_context_tampering_and_password() {
    let (dir, store) = fixture(5);
    let cipher = store
        .vault
        .encrypt("entry:one", b"synthetic-private-marker")
        .unwrap();
    assert_eq!(
        &*store.vault.decrypt("entry:one", &cipher).unwrap(),
        b"synthetic-private-marker"
    );
    assert!(store.vault.decrypt("entry:two", &cipher).is_err());
    let mut damaged = cipher.clone();
    damaged[25] ^= 1;
    assert!(store.vault.decrypt("entry:one", &damaged).is_err());
    assert!(Vault::open(&dir.path().join("master.key"), "wrong-password").is_err());
    let reopened = Vault::open(&dir.path().join("master.key"), "synthetic-vault-password").unwrap();
    assert_eq!(
        &*reopened.decrypt("entry:one", &cipher).unwrap(),
        b"synthetic-private-marker"
    );
}

#[tokio::test]
async fn queue_is_bounded_and_bot_updates_are_idempotent() {
    let (_dir, store) = fixture(1);
    let id = store
        .enqueue(search("first"), Some(42), Some(1))
        .await
        .unwrap();
    assert!(!id.is_empty());
    assert!(
        store
            .enqueue(search("same update"), Some(42), Some(1))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.enqueue(search("second"), None, None).await.is_err());
    assert_eq!(store.jobs(100, 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn concurrent_enqueue_cannot_exceed_capacity() {
    let (_dir, store) = fixture(1);
    let second = store.clone();
    let (one, two) = tokio::join!(
        store.enqueue(search("one"), None, None),
        second.enqueue(search("two"), None, None)
    );
    assert_ne!(one.is_ok(), two.is_ok());
    assert_eq!(store.jobs(100, 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn page_commit_deduplicates_and_keeps_sequence_numbers() {
    let (_dir, store) = fixture(4);
    let entry = |key: &str, page: u32| Entry {
        key: key.into(),
        page: Some(page),
        ..Default::default()
    };
    let batch = store
        .save_page(
            "synthetic-keyword",
            1,
            Some(101),
            vec![entry("a", 1), entry("b", 1)],
        )
        .await
        .unwrap();
    store
        .save_page(
            "synthetic-keyword",
            2,
            Some(102),
            vec![entry("b", 2), entry("c", 2)],
        )
        .await
        .unwrap();
    let rows = store.entries(&batch, 1, 100).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|(n, e)| (*n, e.key.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "a"), (2, "b"), (3, "c")]
    );
    assert_eq!(
        store.cursor("synthetic-keyword").await.unwrap(),
        Some((2, Some(102)))
    );
    assert_eq!(store.entries(&batch, 2, 1).await.unwrap()[0].1.key, "b");
}

#[tokio::test]
async fn database_and_wal_never_contain_sensitive_payloads() {
    let (dir, store) = fixture(4);
    let marker = "private-plaintext-leak-check-482019";
    store.enqueue(search(marker), None, None).await.unwrap();
    store
        .save_page(
            marker,
            1,
            None,
            vec![Entry {
                key: marker.into(),
                description: marker.into(),
                ..Default::default()
            }],
        )
        .await
        .unwrap();
    store.set_preference("target", marker).await.unwrap();
    for file in std::fs::read_dir(dir.path()).unwrap() {
        let bytes = std::fs::read(file.unwrap().path()).unwrap();
        assert!(!bytes.windows(marker.len()).any(|w| w == marker.as_bytes()));
    }
}

#[tokio::test]
async fn recovery_resumes_safe_jobs_but_quarantines_uncertain_sends() {
    let (_dir, store) = fixture(4);
    let first = store.enqueue(search("safe"), None, None).await.unwrap();
    assert_eq!(store.next_job().await.unwrap().unwrap().summary.id, first);
    store.recover().await.unwrap();
    assert_eq!(store.job(&first).await.unwrap().unwrap().status, "queued");
    let running = store.next_job().await.unwrap().unwrap();
    store
        .transfer_intent("scope", "d:123", &running.summary.id, false)
        .await
        .unwrap();
    store.recover().await.unwrap();
    assert_eq!(store.job(&first).await.unwrap().unwrap().status, "review");
    assert!(store.retry(&first, false).await.is_err());
    store.retry(&first, true).await.unwrap();
    assert_eq!(store.job(&first).await.unwrap().unwrap().status, "queued");
    assert!(
        store
            .transfer_status("scope", "d:123")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn transfer_scopes_separate_destinations_and_modes() {
    let (_dir, store) = fixture(4);
    let id = store.enqueue(search("fixture"), None, None).await.unwrap();
    let a = store.vault.index("scope", "account|target_a|copy|payload");
    let b = store.vault.index("scope", "account|target_b|copy|payload");
    let deep = store.vault.index("scope", "account|target_a|deep|payload");
    store
        .transfer_intent(&a, "d:123", &id, false)
        .await
        .unwrap();
    store.transfer_done(&a, "d:123", 9).await.unwrap();
    assert_eq!(
        store.transfer_status(&a, "d:123").await.unwrap(),
        Some(("done".into(), Some(9)))
    );
    assert!(store.transfer_status(&b, "d:123").await.unwrap().is_none());
    assert!(
        store
            .transfer_status(&deep, "d:123")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn album_fragments_are_persisted_as_one_bounded_job() {
    let (_dir, store) = fixture(4);
    let incoming = |id| JobPayload::Incoming {
        source_chat: 42,
        message_ids: vec![id],
        mode: TransferMode::Copy,
        target: "-1000000000001".into(),
        caption: None,
    };
    let id = store
        .enqueue_album(incoming(5), 42, "album", 1)
        .await
        .unwrap();
    assert_eq!(
        store
            .enqueue_album(incoming(6), 42, "album", 2)
            .await
            .unwrap(),
        id
    );
    assert_eq!(store.jobs(100, 0).await.unwrap().len(), 1);
    assert_eq!(store.job(&id).await.unwrap().unwrap().phase, "album");
}

#[test]
fn parser_handles_utf16_links_and_page_footer() {
    use grammers_tl_types as tl;
    use nestbot::telegram::parser;
    let text = "🔑密钥：sample\n📁描述：合成数据\n📦文件个数：12\n第 3 页 / 共 9 页";
    let offset = text.split('\n').next().unwrap().encode_utf16().count() + 1;
    let entity: tl::enums::MessageEntity = tl::types::MessageEntityTextUrl {
        offset: offset as i32,
        length: 2,
        url: "https://t.me/example?start=synthetic%2Bkey".into(),
    }
    .into();
    let page = parser::parse_search(text, &parser::line_links(text, &[entity]));
    assert_eq!(page.page, Some(3));
    assert_eq!(page.entries[0].file_count, Some(12));
    assert_eq!(page.entries[0].payload(), "synthetic+key");
    assert_eq!(parser::rate_wait("请稍后，等待 18 分钟"), Some(1080));
    assert_eq!(parser::rate_wait("普通内容"), None);
}

#[tokio::test]
async fn encrypted_sessions_persist_auth_keys_without_external_sqlite_runtime() {
    use grammers_session::Session;
    use nestbot::telegram::session::EncryptedSession;
    let (_dir, store) = fixture(4);
    let session = EncryptedSession::open(store.clone(), "main").await.unwrap();
    session.set_home_dc_id(5).await.unwrap();
    let mut dc = session.dc_option(5).unwrap().unwrap();
    dc.auth_key = Some([7; 256]);
    session.set_dc_option(&dc).await.unwrap();
    let reopened = EncryptedSession::open(store, "main").await.unwrap();
    assert_eq!(reopened.home_dc_id().unwrap(), 5);
    assert_eq!(
        reopened.dc_option(5).unwrap().unwrap().auth_key,
        Some([7; 256])
    );
}

#[tokio::test]
async fn web_requires_login_and_csrf_and_escapes_data_on_client() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (_dir, store) = fixture(4);
    let app = App::new(Config::default(), store).unwrap();
    let web = nestbot::interfaces::web::WebState::with_password(
        app,
        zeroize::Zeroizing::new("synthetic-admin-password".into()),
    )
    .await
    .unwrap();
    let router = nestbot::interfaces::web::router(web);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/login")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"password":"synthetic-admin-password"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let csrf = body["csrf"].as_str().unwrap();
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/queue/clear")
                .method("POST")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/queue/clear")
                .method("POST")
                .header("cookie", &cookie)
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/logout")
                .method("POST")
                .header("cookie", &cookie)
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn configuration_rejects_unbounded_or_exposed_defaults() {
    let mut config = Config::default();
    config.web.listen = "0.0.0.0:8787".parse().unwrap();
    assert!(config.validate().is_err());
    config.web.allow_remote = true;
    assert!(config.validate().is_err());
    config.web.secure_cookie = true;
    assert!(config.validate().is_ok());
    config.limits.max_queue = 0;
    assert!(config.validate().is_err());
}

#[test]
fn processing_notices_are_not_limits_and_empty_search_is_recognized() {
    use nestbot::telegram::parser;
    for text in [
        "请稍候",
        "正在搜索，请稍候……",
        "正在获取文件，请稍后",
        "正在处理，预计等待 10 秒",
    ] {
        assert_eq!(parser::rate_wait(text), None, "{text}");
    }
    assert_eq!(parser::rate_wait("请求过于频繁，请稍后再试"), Some(60));
    assert_eq!(parser::rate_wait("请等待 1048 秒后重试"), Some(1048));
    assert!(parser::no_search_results(
        "🔎 搜索词：synthetic\n🔍 未找到相关结果"
    ));
}

#[tokio::test]
async fn cancellation_survives_finish_races_and_restart_without_requeue() {
    let (_dir, store) = fixture(4);
    let id = store
        .enqueue(search("cancelled"), None, None)
        .await
        .unwrap();
    store.next_job().await.unwrap().unwrap();
    store.cancel(&id).await.unwrap();
    store
        .finish(&id, "waiting", Some("telegram_rate_limited"), Some(0))
        .await
        .unwrap();
    store.recover().await.unwrap();
    assert_eq!(store.job(&id).await.unwrap().unwrap().status, "cancelled");
    assert!(store.job(&id).await.unwrap().unwrap().retry_at.is_none());
    assert!(store.next_job().await.unwrap().is_none());
    let second = store
        .enqueue(search("cancelled-on-reboot"), None, None)
        .await
        .unwrap();
    store.next_job().await.unwrap().unwrap();
    store.cancel(&second).await.unwrap();
    store.recover().await.unwrap();
    assert_eq!(
        store.job(&second).await.unwrap().unwrap().status,
        "cancelled"
    );
    assert!(store.next_job().await.unwrap().is_none());
    let uncertain = store
        .enqueue(search("cancelled-but-uncertain"), None, None)
        .await
        .unwrap();
    store.next_job().await.unwrap().unwrap();
    store
        .transfer_intent("scope-cancel", "d:synthetic", &uncertain, false)
        .await
        .unwrap();
    store.cancel(&uncertain).await.unwrap();
    store.recover().await.unwrap();
    assert_eq!(
        store.job(&uncertain).await.unwrap().unwrap().status,
        "review"
    );
    assert!(store.next_job().await.unwrap().is_none());
}

#[tokio::test]
async fn manual_retry_resets_attempt_counter_but_waiting_keeps_it() {
    let (_dir, store) = fixture(4);
    let id = store.enqueue(search("limits"), None, None).await.unwrap();
    for attempt in 1..=6 {
        assert_eq!(store.next_job().await.unwrap().unwrap().summary.id, id);
        assert_eq!(store.attempts(&id).await.unwrap(), attempt);
        store
            .finish(&id, "waiting", Some("telegram_rate_limited"), Some(0))
            .await
            .unwrap();
    }
    store
        .finish(&id, "failed", Some("telegram_retries_exhausted"), None)
        .await
        .unwrap();
    store.retry(&id, false).await.unwrap();
    assert_eq!(store.attempts(&id).await.unwrap(), 0);
}

#[tokio::test]
async fn media_inbox_is_durable_deduplicated_and_paged() {
    let (dir, store) = fixture(5);
    let job = store
        .enqueue(search("synthetic"), None, None)
        .await
        .unwrap();
    for id in 1..=25 {
        assert!(
            store
                .inbox_push(&job, "claim", id, &format!("media-{id}"))
                .await
                .unwrap()
        );
    }
    assert!(
        !store
            .inbox_push(&job, "claim", 100, "media-1")
            .await
            .unwrap()
    );
    let first = store.inbox_pending(&job, "claim").await.unwrap();
    assert_eq!(first, (1..=10).collect::<Vec<_>>());
    for id in first {
        store.inbox_done(&job, "claim", id).await.unwrap();
    }
    let vault = store.vault.clone();
    drop(store);
    let reopened = Store::open(&dir.path().join("test.sqlite"), vault, 1024, 5).unwrap();
    assert_eq!(
        reopened.inbox_pending(&job, "claim").await.unwrap(),
        (11..=20).collect::<Vec<_>>()
    );
    reopened.reset_inbox(&job, "claim").await.unwrap();
    assert!(
        reopened
            .inbox_pending(&job, "claim")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn fast_copy_lane_can_run_while_main_job_is_running() {
    let (_dir, store) = fixture(5);
    let main = store
        .enqueue(search("synthetic"), None, None)
        .await
        .unwrap();
    let fast = store
        .enqueue(
            JobPayload::Incoming {
                source_chat: 42,
                message_ids: vec![1],
                mode: TransferMode::Copy,
                target: "-1001234567890".into(),
                caption: None,
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .next_job_lane(false)
            .await
            .unwrap()
            .unwrap()
            .summary
            .id,
        main
    );
    assert_eq!(
        store.next_job_lane(true).await.unwrap().unwrap().summary.id,
        fast
    );
    assert!(store.next_job_lane(false).await.unwrap().is_none());
}

#[tokio::test]
async fn imports_original_python_vault_without_modifying_source_or_duplicate_entries() {
    let (_dir, store) = fixture(5);
    let old = tempfile::tempdir().unwrap();
    std::fs::create_dir(old.path().join("keys")).unwrap();
    let blob = include_bytes!("fixtures/legacy-hv1.bin");
    let path = old.path().join("keys/fixture.bin");
    std::fs::write(&path, blob).unwrap();
    assert!(nestbot::storage::vault::decode_legacy(blob, "wrong").is_err());
    for _ in 0..2 {
        assert_eq!(
            nestbot::application::migration::import_legacy(
                &store,
                old.path(),
                "synthetic-legacy-password"
            )
            .await
            .unwrap(),
            (1, 2)
        );
    }
    let batches = store.batches(10, 0).await.unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].entries, 2);
    assert_eq!(
        store.batch_keyword(&batches[0].id).await.unwrap(),
        "synthetic-legacy"
    );
    assert_eq!(std::fs::read(path).unwrap(), blob);
}
