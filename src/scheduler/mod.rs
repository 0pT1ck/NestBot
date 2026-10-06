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
                    ("failed", Some("telegram_rate_limited"), None)
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
                let report = app.store.report(&job.summary.id).await?;
                let mut progress = String::new();
                if report.pages > 0 {
                    progress.push_str(&format!("\n已保存 {} 页搜索结果。", report.pages));
                }
                if job.summary.kind == "transfer" {
                    progress.push_str(&format!("\n成功 {} 个文件，续传/重复跳过 {} 个文件，已完成跳过 {} 个密钥；失败 {} 个密钥、{} 个文件。",report.files,report.skipped_files,report.skipped_keys,report.failed_keys,report.failed_files));
                }
                if !report.warnings.is_empty() {
                    progress.push_str(&format!(
                        "\n提示：{}（已完成部分保留）",
                        report.warnings.join(", ")
                    ));
                }
                let _ = bot
                    .send_message(
                        ChatId(chat),
                        format!(
                            "任务 {}：{}{}{}",
                            job.summary.id,
                            status,
                            error.map(|e| format!("（{e}）")).unwrap_or_default(),
                            progress
                        ),
                    )
                    .await;
                if status == "completed"
                    && let crate::domain::JobPayload::Search { keyword, .. } = &job.payload
                {
                    crate::interfaces::bot::search_result(&app, &job, keyword).await?;
                }
            }
        } else {
            tokio::select! {_=app.shutdown.cancelled()=>break,_=notified=>{},_=tokio::time::sleep(Duration::from_secs(1))=>{}}
        }
    }
    Ok(())
}
