use crate::{application::App, domain::JobReport, telegram::RateLimited, telemetry};
use std::{sync::Arc, time::Duration};

fn final_status(
    result: &anyhow::Result<()>,
    kind: &str,
    report: &JobReport,
    uncertain: bool,
    shutdown: bool,
    cancelled: bool,
) -> (&'static str, Option<&'static str>) {
    let partial = (kind == "search"
        && report
            .warnings
            .iter()
            .any(|warning| warning == "search_page_stalled"))
        || (kind == "transfer" && (report.failed_keys > 0 || report.failed_files > 0))
        || report
            .warnings
            .iter()
            .any(|warning| warning == "claim_incomplete");
    match result {
        Ok(()) if partial => ("partial", None),
        Ok(()) => ("completed", None),
        Err(_) if uncertain => ("review", Some("transfer_uncertain")),
        Err(_) if shutdown => ("interrupted", None),
        Err(_) if cancelled => ("cancelled", None),
        Err(error) if error.is::<RateLimited>() => ("failed", Some("telegram_rate_limited")),
        Err(error) => ("failed", Some(telemetry::safe_error(error))),
    }
}

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
                && report
                    .warnings
                    .iter()
                    .any(|warning| warning == "search_page_stalled");
            let (status, error) = final_status(
                &result,
                &job.summary.kind,
                &report,
                uncertain,
                app.shutdown.is_cancelled(),
                token.is_cancelled(),
            );
            app.store
                .finish(&job.summary.id, status, error, None)
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
                if report
                    .warnings
                    .iter()
                    .any(|warning| warning == "search_page_stalled")
                {
                    progress.push_str("\n翻页等待超时，已停止本次搜索，已保存结果保留。");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn status(kind: &str, report: &JobReport) -> (&'static str, Option<&'static str>) {
        final_status(&Ok(()), kind, report, false, false, false)
    }

    #[test]
    fn successful_transfer_with_failed_keys_or_files_finishes_partial() {
        for (failed_keys, failed_files) in [(1, 0), (0, 1), (2, 3)] {
            let report = JobReport {
                failed_keys,
                failed_files,
                ..Default::default()
            };
            assert_eq!(status("transfer", &report), ("partial", None));
        }
        let report = JobReport {
            files: 2,
            skipped_keys: 3,
            skipped_files: 4,
            warnings: vec!["optional_thumbnail_missing".into()],
            ..Default::default()
        };
        assert_eq!(status("transfer", &report), ("completed", None));
    }

    #[test]
    fn claim_incomplete_finishes_partial_and_search_partial_is_preserved() {
        let report = JobReport {
            warnings: vec!["claim_incomplete".into()],
            ..Default::default()
        };
        assert_eq!(status("transfer", &report), ("partial", None));
        assert_eq!(status("claim", &report), ("partial", None));
        let report = JobReport {
            pages: 2,
            warnings: vec!["search_page_stalled".into()],
            ..Default::default()
        };
        assert_eq!(status("search", &report), ("partial", None));
        assert_eq!(status("transfer", &report), ("completed", None));
        assert_eq!(status("search", &JobReport::default()), ("completed", None));
    }

    #[test]
    fn successful_retry_finishes_completed_with_previous_success_counts() {
        let previous = JobReport {
            files: 2,
            skipped_keys: 3,
            skipped_files: 4,
            failed_keys: 1,
            failed_files: 1,
            warnings: vec!["claim_incomplete".into()],
            ..Default::default()
        };
        assert_eq!(status("transfer", &previous), ("partial", None));
        // Execution resets attempt failures; cumulative successes remain in the report.
        let retried = JobReport {
            files: previous.files + 1,
            failed_keys: 0,
            failed_files: 0,
            warnings: vec![],
            ..previous
        };
        assert_eq!(status("transfer", &retried), ("completed", None));
    }

    #[test]
    fn execution_errors_keep_existing_status_precedence_even_with_partial_report() {
        let report = JobReport {
            failed_files: 1,
            warnings: vec!["claim_incomplete".into()],
            ..Default::default()
        };
        let result = Err(anyhow::Error::new(RateLimited));
        for (uncertain, shutdown, cancelled, expected) in [
            (true, true, true, ("review", Some("transfer_uncertain"))),
            (false, true, true, ("interrupted", None)),
            (false, false, true, ("cancelled", None)),
            (
                false,
                false,
                false,
                ("failed", Some("telegram_rate_limited")),
            ),
        ] {
            assert_eq!(
                final_status(&result, "transfer", &report, uncertain, shutdown, cancelled),
                expected
            );
        }
        assert_eq!(
            final_status(
                &Err(anyhow::anyhow!("upload_failed")),
                "transfer",
                &report,
                false,
                false,
                false
            ),
            ("failed", Some("upload_failed"))
        );
    }
}
