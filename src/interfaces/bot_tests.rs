use super::*;

fn update(owner: i64, text: &str, private: bool) -> Value {
    serde_json::json!({
        "update_id": 100,
        "message": {"message_id": 1, "from": {"id": owner},
            "chat": {"id": if private { owner } else { -1001234567890i64 },
                "type": if private { "private" } else { "supergroup" }}, "text": text}
    })
}

#[tokio::test]
async fn restart_requires_whitelist_and_persists_offset_before_triggering() {
    let (_dir, app) = crate::restart_tests::fixture();
    handle(&app, &update(7, "/restart", true)).await.unwrap();
    assert!(!app.restart.is_cancelled());
    assert!(app.store.preference("bot_offset").await.unwrap().is_none());
    handle(&app, &update(42, "/restart@synthetic_bot", true))
        .await
        .unwrap();
    assert!(app.restart.is_cancelled());
    assert!(!app.shutdown.is_cancelled());
    assert_eq!(
        app.store.preference("bot_offset").await.unwrap().as_deref(),
        Some("101")
    );
}

#[tokio::test]
async fn restart_works_in_groups_and_rejects_arguments() {
    let (_dir, app) = crate::restart_tests::fixture();
    handle(&app, &update(42, "/restart extra", false))
        .await
        .unwrap();
    assert!(!app.restart.is_cancelled());
    handle(&app, &update(42, "/restart", false)).await.unwrap();
    assert!(app.restart.is_cancelled());
}

#[tokio::test]
async fn restart_recovers_running_jobs_and_keeps_uncertain_sends_for_review() {
    let (_dir, app) = crate::restart_tests::fixture();
    let payload = || JobPayload::Search {
        keyword: "synthetic".into(),
        pages: Some(1),
        sort: None,
        resume: false,
    };
    let interrupted = app.enqueue(payload(), None, None).await.unwrap();
    let uncertain = app.enqueue(payload(), None, None).await.unwrap();
    app.store.next_job().await.unwrap().unwrap();
    app.store.next_job().await.unwrap().unwrap();
    app.store
        .transfer_intent("synthetic-scope", "photo:1", &uncertain, false)
        .await
        .unwrap();
    app.store.recover().await.unwrap();
    assert_eq!(
        app.store.job(&interrupted).await.unwrap().unwrap().status,
        "queued"
    );
    assert_eq!(
        app.store.job(&uncertain).await.unwrap().unwrap().status,
        "review"
    );
    assert!(app.store.retry(&uncertain, false).await.is_err());
}
