use super::*;
use crate::{
    config::Config,
    domain::JobPayload,
    storage::{Store, vault::Vault},
};
use std::sync::Mutex;

pub(crate) fn fixture() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Arc::new(
        Vault::open(
            &dir.path().join("master.key"),
            "synthetic-progress-password",
        )
        .unwrap(),
    );
    let store = Store::open(&dir.path().join("test.sqlite"), vault, 1024, 20).unwrap();
    let mut config = Config::default();
    config.telegram.bot_token_env = "NESTBOT_PROGRESS_TEST_DISABLED_BOT".into();
    (dir, App::new(config, store).unwrap())
}

pub(crate) async fn job(app: &App) -> crate::domain::Job {
    app.enqueue(
        JobPayload::Search {
            keyword: "synthetic".into(),
            pages: None,
            sort: None,
            resume: false,
        },
        Some(42),
        None,
    )
    .await
    .unwrap();
    app.store.next_job().await.unwrap().unwrap()
}

#[tokio::test]
async fn pages_and_keys_edit_the_original_receipt_and_unchanged_progress_is_not_sent() {
    use axum::{Json, Router, extract::OriginalUri};
    let requests = Arc::new(Mutex::new(Vec::<(String, serde_json::Value)>::new()));
    let captured = requests.clone();
    let router = Router::new().fallback(move |OriginalUri(uri):OriginalUri, Json(body):Json<serde_json::Value>| {
        let captured = captured.clone();
        async move {
            captured.lock().unwrap().push((uri.path().into(), body.clone()));
            Json(serde_json::json!({"ok":true,"result":{"message_id":321,"date":0,"chat":{"id":42,"type":"private","first_name":"Synthetic"},"text":body["text"]}}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, mut app) = fixture();
    Arc::get_mut(&mut app).unwrap().bot = Some(
        teloxide::Bot::new("000000:synthetic")
            .set_api_url(format!("http://{address}").parse().unwrap()),
    );
    let job = job(&app).await;
    let id = &job.summary.id;
    let header = format!("搜索已排队：{id}");
    crate::interfaces::bot::send(&app, 42, &header).await;
    let mut deliveries = HashMap::new();
    for page in [4, 5] {
        let report = JobReport {
            pages: page,
            search_page: Some(page),
            search_total_pages: Some(125),
            ..Default::default()
        };
        app.store.save_report(id, &report).await.unwrap();
        deliveries.clear();
        update(&app, id, &mut deliveries).await.unwrap();
        update(&app, id, &mut deliveries).await.unwrap();
    }
    let mut report = app.store.report(id).await.unwrap();
    report.transfer_key = Some(25);
    app.store.save_report(id, &report).await.unwrap();
    let wait = JobWait {
        seconds: 763,
        retry_at: unix_time() + 823,
    };
    app.store
        .set_preference(
            &format!("wait:{id}"),
            &serde_json::to_string(&wait).unwrap(),
        )
        .await
        .unwrap();
    deliveries.clear();
    update(&app, id, &mut deliveries).await.unwrap();
    let captured = requests.lock().unwrap().clone();
    assert_eq!(captured.len(), 4);
    assert!(captured[0].0.to_lowercase().ends_with("sendmessage"));
    for request in &captured[1..] {
        assert!(request.0.to_lowercase().ends_with("editmessagetext"));
        assert_eq!(request.1["message_id"], 321);
    }
    assert!(
        captured[1].1["text"]
            .as_str()
            .unwrap()
            .contains("第 4 页 / 共 125 页")
    );
    assert!(
        captured[2].1["text"]
            .as_str()
            .unwrap()
            .contains("第 5 页 / 共 125 页")
    );
    let current = captured[3].1["text"].as_str().unwrap();
    assert!(current.contains("第 25 个密钥"));
    assert!(current.contains("763 秒"));
    assert!(current.contains("额外等待 60 秒"));
    app.store.finish(id, "cancelled", None, None).await.unwrap();
    deliveries.clear();
    update(&app, id, &mut deliveries).await.unwrap();
    assert!(
        app.store
            .preference(&format!("job-ui:{id}"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        requests.lock().unwrap().last().unwrap().1["text"]
            .as_str()
            .unwrap()
            .contains("已停止")
    );
    server.abort();
}

#[tokio::test]
async fn progress_edit_rate_limit_defers_updates_for_requested_seconds_plus_a_minute() {
    use axum::{Json, Router};
    let count = Arc::new(Mutex::new(0));
    let captured = count.clone();
    let router = Router::new().fallback(move || {
        let captured = captured.clone();
        async move {
            *captured.lock().unwrap() += 1;
            Json(serde_json::json!({"ok":false,"error_code":429,"description":"Too Many Requests: retry after 7","parameters":{"retry_after":7}}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, mut app) = fixture();
    Arc::get_mut(&mut app).unwrap().bot = Some(
        teloxide::Bot::new("000000:synthetic")
            .set_api_url(format!("http://{address}").parse().unwrap()),
    );
    let job = job(&app).await;
    let id = &job.summary.id;
    app.store
        .set_preference(
            &format!("job-ui:{id}"),
            &serde_json::to_string(&Receipt {
                chat: 42,
                message: 321,
                header: format!("搜索已排队：{id}"),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let mut deliveries = HashMap::new();
    let start = tokio::time::Instant::now();
    update(&app, id, &mut deliveries).await.unwrap();
    assert!(deliveries[id].next - start >= Duration::from_secs(67));
    assert!(deliveries[id].next - start < Duration::from_secs(72));
    update(&app, id, &mut deliveries).await.unwrap();
    assert_eq!(*count.lock().unwrap(), 1);
    server.abort();
}
