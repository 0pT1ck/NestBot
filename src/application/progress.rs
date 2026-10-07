use super::App;
use crate::{domain::Job, telegram};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(crate) async fn observe<T>(
    app: &App,
    job: &Job,
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let name = format!("wait:{}", job.summary.id);
    app.store.set_preference(&name, "").await?;
    let (events, mut notices) = watch::channel(None);
    // One enclosing budget also protects transfer stall timers and the collector.
    let future = telegram::flood_scope(events, telegram::bounded(cancel, u32::MAX as u64, future));
    tokio::pin!(future);
    let mut until = None;
    let result = loop {
        let wake = until
            .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(3600));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break Err(anyhow::anyhow!("cancelled")),
            changed = notices.changed() => {
                if changed.is_ok() {
                    let notice = *notices.borrow_and_update();
                    if let Some(notice) = notice {
                        until = Some(notice.until);
                        app.store.set_preference(&name, &serde_json::to_string(&notice.wait)?).await?;
                        app.publish(&job.summary.id).await;
                    }
                }
            }
            _ = tokio::time::sleep_until(wake), if until.is_some() => {
                until = None;
                app.store.set_preference(&name, "").await?;
                app.publish(&job.summary.id).await;
            }
            result = &mut future => break result,
        }
    };
    app.store.set_preference(&name, "").await?;
    app.publish(&job.summary.id).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::progress;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn long_flood_wait_is_visible_and_resumes_same_request_after_823_seconds() {
        let (_dir, app) = progress::tests::fixture();
        let job = progress::tests::job(&app).await;
        let cancel = CancellationToken::new();
        let start = tokio::time::Instant::now();
        observe(&app, &job, &cancel, async {
            let collector = tokio::spawn(telegram::inherit_flood_context(async {
                telegram::wait_flood(&CancellationToken::new(), 763).await
            }));
            tokio::time::sleep(Duration::from_secs(1)).await;
            let wait = progress::waiting(&app, &job.summary.id).await?.unwrap();
            assert_eq!(wait.seconds, 763);
            let status = crate::interfaces::bot::command(&app, 42, "/status", None).await?;
            assert!(status.contains("763 秒"));
            collector.await??;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(823));
        assert!(
            progress::waiting(&app, &job.summary.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_interrupts_long_wait_and_clears_countdown() {
        let (_dir, app) = progress::tests::fixture();
        let job = progress::tests::job(&app).await;
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            trigger.cancel();
        });
        let start = tokio::time::Instant::now();
        let error = observe(&app, &job, &cancel, telegram::wait_flood(&cancel, 763))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        assert!(
            progress::waiting(&app, &job.summary.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
