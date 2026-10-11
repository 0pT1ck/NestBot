use super::*;
use crate::domain::JobPayload;
use tokio::sync::Mutex;

#[tokio::test]
async fn api_rate_limit_stops_without_retry_and_preserves_previous_completed_transfers() {
    use axum::{Json, Router};
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = requests.clone();
    let router = Router::new().fallback(move |Json(body): Json<serde_json::Value>| {
        let captured = captured.clone();
        async move {
            let mut requests = captured.lock().await;
            requests.push(body);
            if requests.len() == 1 {
                Json(serde_json::json!({"ok":true,"result":[{"message_id":101}]}))
            } else {
                Json(serde_json::json!({"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":8}}))
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, mut app) = crate::interfaces::progress::tests::fixture();
    Arc::get_mut(&mut app).unwrap().bot = Some(
        teloxide::Bot::new("000000:synthetic")
            .set_api_url(format!("http://{address}").parse().unwrap()),
    );
    let mut completed_scope = None;
    for (message, succeeds) in [(11, true), (12, false)] {
        app.enqueue(
            JobPayload::Incoming {
                source_chat: 42,
                message_ids: vec![message],
                mode: TransferMode::Copy,
                target: "-100123456789".into(),
                caption: None,
                caption_message: None,
            },
            None,
            None,
        )
        .await
        .unwrap();
        let job = app.store.next_job_lane(true).await.unwrap().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            app.execute(&job, &CancellationToken::new()),
        )
        .await
        .unwrap();
        if succeeds {
            result.unwrap();
            completed_scope = Some(
                app.store
                    .vault
                    .index("incoming_scope", "42|-100123456789|copy"),
            );
        } else {
            assert!(result.unwrap_err().is::<crate::telegram::RateLimited>());
            assert!(!app.store.has_uncertain(&job.summary.id).await.unwrap());
        }
    }
    assert_eq!(requests.lock().await.len(), 2);
    assert_eq!(
        app.store
            .transfer_status(&completed_scope.unwrap(), "11")
            .await
            .unwrap(),
        Some(("done".into(), Some(101)))
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(120)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        requests.lock().await.len(),
        2,
        "rate-limited request was automatically retried"
    );
    tokio::time::resume();
    server.abort();
}
