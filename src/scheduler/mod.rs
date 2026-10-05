use crate::{application::App, telegram::RetryLater, telemetry};
use std::{sync::Arc, time::Duration};

pub async fn worker(app: Arc<App>, fast: bool) -> anyhow::Result<()> {
    loop {
        if app.shutdown.is_cancelled() {
            break;
        }
        let notified = app.notify.notified();
        if let Some(job) = app.store.next_job_lane(fast).await? {
            let token = app.shutdown.child_token();
            app.active
                .lock()
                .map_err(|_| anyhow::anyhow!("service_unavailable"))?
                .insert(job.summary.id.clone(), token.clone());
            if app
                .store
                .job(&job.summary.id)
                .await?
                .is_some_and(|s| s.status == "cancelling")
            {
                token.cancel();
            }
            tracing::info!(event="job_started",job_id=%job.summary.id,kind=%job.summary.kind);
            let result = app.execute(&job, &token).await;
            let uncertain = app.store.has_uncertain(&job.summary.id).await?;
            let (status, error, retry) = match result {
                Ok(()) => ("completed", None, None),
                Err(_) if uncertain => ("review", Some("transfer_uncertain"), None),
                Err(_) if app.shutdown.is_cancelled() => ("interrupted", None, None),
                Err(_) if token.is_cancelled() => ("cancelled", None, None),
                Err(ref e) if e.downcast_ref::<RetryLater>().is_some() => {
                    let seconds = e
                        .downcast_ref::<RetryLater>()
                        .unwrap()
                        .seconds
                        .clamp(1, 86400);
                    (
                        "waiting",
                        Some("telegram_rate_limited"),
                        Some(crate::domain::unix_time() + seconds as i64),
                    )
                }
                Err(ref e) => ("failed", Some(telemetry::safe_error(e)), None),
            };
            app.store
                .finish(&job.summary.id, status, error, retry)
                .await?;
            app.active
                .lock()
                .map_err(|_| anyhow::anyhow!("service_unavailable"))?
                .remove(&job.summary.id);
            app.publish(&job.summary.id).await;
            tracing::info!(event="job_finished",job_id=%job.summary.id,status,error_code=error);
            if let (Some(bot), Some(chat)) = (&app.bot, job.reply_chat) {
                use teloxide::prelude::*;
                let _ = bot
                    .send_message(
                        ChatId(chat),
                        format!(
                            "任务 {}：{}{}",
                            job.summary.id,
                            status,
                            error.map(|e| format!("（{e}）")).unwrap_or_default()
                        ),
                    )
                    .await;
            }
        } else {
            tokio::select! {_=app.shutdown.cancelled()=>break,_=notified=>{},_=tokio::time::sleep(Duration::from_secs(1))=>{}}
        }
    }
    Ok(())
}
