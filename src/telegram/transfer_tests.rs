use super::*;
use crate::domain::JobPayload;
use std::sync::Mutex;

#[tokio::test]
async fn bot_api_copy_retries_only_the_rejected_request_after_wait_plus_60_seconds() {
    use axum::{Json, Router};
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = requests.clone();
    let router=Router::new().fallback(move |Json(body):Json<serde_json::Value>| {
        let captured=captured.clone();
        async move {
            let mut requests=captured.lock().unwrap();
            requests.push(body);
            if requests.len()==1 {
                Json(serde_json::json!({"ok":false,"error_code":429,"description":"Too Many Requests: retry after 7","parameters":{"retry_after":7}}))
            } else {
                Json(serde_json::json!({"ok":true,"result":[{"message_id":101}]}))
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
    app.enqueue(
        JobPayload::Incoming {
            source_chat: 42,
            message_ids: vec![11],
            mode: TransferMode::Copy,
            target: "-100123456789".into(),
            caption: None,
            caption_message: None,
        },
        Some(42),
        None,
    )
    .await
    .unwrap();
    let job = app.store.next_job_lane(true).await.unwrap().unwrap();
    let id = job.summary.id.clone();
    let running = app.clone();
    let task = tokio::spawn(async move { running.execute(&job, &CancellationToken::new()).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if crate::interfaces::progress::waiting(&app, &id)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(66)).await;
    tokio::task::yield_now().await;
    assert_eq!(requests.lock().unwrap().len(), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    task.await.unwrap().unwrap();
    let copied = requests.lock().unwrap().clone();
    assert_eq!(copied.len(), 2);
    assert_eq!(copied[0], copied[1]);
    assert!(!app.store.has_uncertain(&id).await.unwrap());
    assert!(
        crate::interfaces::progress::waiting(&app, &id)
            .await
            .unwrap()
            .is_none()
    );
    server.abort();
}
