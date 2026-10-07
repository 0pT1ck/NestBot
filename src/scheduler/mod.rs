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
            let report = app.store.report(&job.summary.id).await?;
            let partial_search = job.summary.kind == "search"
                && report.warnings.iter().any(|warning| {
                    matches!(
                        warning.as_str(),
                        "search_page_stalled" | "search_retries_exhausted"
                    )
                });
            let rate_wait = result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<RetryLater>())
                .map(|wait| wait.seconds);
            let (status, error, retry) = match result {
                Ok(()) if partial_search => ("partial", None, None),
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
                let mut progress = String::new();
                if let Some(seconds) = rate_wait {
                    progress.push_str(&format!("\nTelegram 要求等待 {seconds} 秒；短期限流已自动等待，长等待或连续限流暂停本次任务。已完成部分保留，等待后可重新执行原命令续传。"));
                }
                if report.pages > 0 {
                    progress.push_str(&format!("\n已保存 {} 页搜索结果。", report.pages));
                }
                if partial_search
                    && let crate::domain::JobPayload::Search { keyword, .. } = &job.payload
                {
                    progress.push_str(&format!(
                        "\n搜索未翻完，已收集结果保留。可用 /search {keyword} continue 续搜。"
                    ));
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
                if matches!(status, "completed" | "partial")
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
