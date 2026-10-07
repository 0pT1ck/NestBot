use crate::{
    application::App,
    domain::{JobReport, JobSummary, JobWait, unix_time},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use teloxide::{prelude::*, types::MessageId};

#[derive(Serialize, Deserialize)]
struct Receipt {
    chat: i64,
    message: i32,
    header: String,
}

pub(crate) fn receipt_job(text: &str) -> Option<&str> {
    let line = text.lines().next()?;
    let (label, id) = line.split_once('：')?;
    if [
        "搜索已排队",
        "转存已排队",
        "批量转存已排队",
        "已排队",
        "媒体已接收",
        "已重新排队",
    ]
    .contains(&label)
        && uuid::Uuid::parse_str(id).is_ok()
    {
        Some(id)
    } else {
        None
    }
}

pub(crate) async fn receipt(app: &App, chat: i64, header: &str, id: &str) -> anyhow::Result<()> {
    let Some(bot) = &app.bot else { return Ok(()) };
    let Some(job) = app.store.job(id).await? else {
        return Ok(());
    };
    let report = app.store.report(id).await?;
    let wait = waiting(app, id).await?;
    let text = render(header, &job, &report, wait, unix_time());
    let message = tokio::select! {
        _ = app.shutdown.cancelled() => return Ok(()),
        result = tokio::time::timeout(Duration::from_secs(10), bot.send_message(ChatId(chat), text)) => result??,
    };
    let record = Receipt {
        chat,
        message: message.id.0,
        header: header.into(),
    };
    app.store
        .set_preference(&format!("job-ui:{id}"), &serde_json::to_string(&record)?)
        .await?;
    Ok(())
}

pub(crate) async fn waiting(app: &App, id: &str) -> anyhow::Result<Option<JobWait>> {
    app.store
        .preference(&format!("wait:{id}"))
        .await?
        .filter(|value| !value.is_empty())
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

fn active(job: &JobSummary) -> bool {
    matches!(
        job.status.as_str(),
        "queued" | "waiting" | "running" | "cancelling" | "interrupted"
    )
}

pub(crate) fn render(
    header: &str,
    job: &JobSummary,
    report: &JobReport,
    wait: Option<JobWait>,
    now: i64,
) -> String {
    let state = match job.status.as_str() {
        "queued" | "waiting" => "排队中",
        "running" => "进行中",
        "cancelling" => "正在停止",
        "cancelled" => "已停止",
        "completed" => "已完成",
        "partial" => "部分完成，结果已保留",
        "interrupted" => "已中断，进度已保留",
        "review" => "需核查发送结果",
        _ => "失败，已完成记录保留",
    };
    let mut lines = vec![header.to_owned(), format!("状态：{state}")];
    // A search-and-transfer job switches to key progress once extraction starts.
    if let Some(key) = report.transfer_key {
        lines.push(if active(job) {
            format!("正在转存第 {key} 个密钥")
        } else {
            format!("最后处理：第 {key} 个密钥")
        });
        lines.push(format!(
            "成功 {} 个文件，跳过 {} 个文件；失败 {} 个密钥",
            report.files, report.skipped_files, report.failed_keys
        ));
    } else if job.kind == "search" || report.search_page.is_some() {
        if let Some(page) = report.search_page {
            lines.push(format!(
                "当前搜索：第 {page} 页{}",
                report
                    .search_total_pages
                    .map(|total| format!(" / 共 {total} 页"))
                    .unwrap_or_default()
            ));
            lines.push(format!("本次已保存 {} 页", report.pages));
        } else {
            lines.push("当前搜索：等待页面回复".into());
        }
    } else if job.kind == "transfer" {
        lines.push("当前转存：等待开始处理密钥".into());
    }
    if let Some(wait) = wait.filter(|_| job.status == "running") {
        lines.push(format!(
            "Telegram 要求等待 {} 秒，额外等待 60 秒；剩余 {} 秒，之后自动继续。",
            wait.seconds,
            wait.retry_at.saturating_sub(now).max(0)
        ));
    } else if report.search_retry > 0 && job.status == "running" {
        lines.push(format!(
            "翻页重试：第 {}/20 轮；{}",
            report.search_retry,
            report
                .search_retry_at
                .map(|at| format!("{} 秒后重试", at.saturating_sub(now).max(0)))
                .unwrap_or_else(|| "正在重试原消息按钮".into())
        ));
    }
    if let Some(error) = &job.error_code {
        lines.push(format!("提示：{error}"));
    }
    lines.join("\n")
}

struct Delivery {
    text: String,
    next: tokio::time::Instant,
}

async fn update(
    app: &App,
    id: &str,
    deliveries: &mut HashMap<String, Delivery>,
) -> anyhow::Result<()> {
    let Some(bot) = &app.bot else { return Ok(()) };
    let Some(value) = app.store.preference(&format!("job-ui:{id}")).await? else {
        return Ok(());
    };
    let record: Receipt = serde_json::from_str(&value)?;
    let Some(job) = app.store.job(id).await? else {
        return Ok(());
    };
    let report = app.store.report(id).await?;
    let text = render(
        &record.header,
        &job,
        &report,
        waiting(app, id).await?,
        unix_time(),
    );
    let now = tokio::time::Instant::now();
    if deliveries
        .get(id)
        .is_some_and(|previous| previous.next > now || previous.text == text)
    {
        return Ok(());
    }
    let result = tokio::select! {
        _ = app.shutdown.cancelled() => return Ok(()),
        result = tokio::time::timeout(Duration::from_secs(10), bot.edit_message_text(ChatId(record.chat), MessageId(record.message), &text)) => result,
    };
    let (success, delay) = match result {
        Ok(Ok(_)) => (true, Duration::from_secs(2)),
        Ok(Err(teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified))) => {
            (true, Duration::from_secs(2))
        }
        Ok(Err(teloxide::RequestError::RetryAfter(seconds))) => {
            (false, seconds.duration() + Duration::from_secs(60))
        }
        _ => (false, Duration::from_secs(10)),
    };
    deliveries.insert(
        id.into(),
        Delivery {
            text: if success { text } else { String::new() },
            next: tokio::time::Instant::now() + delay,
        },
    );
    if success && !active(&job) {
        let name = format!("job-ui:{id}");
        app.store
            .call(move |c| {
                c.execute("DELETE FROM preferences WHERE name=?1", [name])?;
                Ok(())
            })
            .await?;
        deliveries.remove(id);
    }
    Ok(())
}

pub async fn run(app: Arc<App>) -> anyhow::Result<()> {
    let mut deliveries = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = tick.tick() => {},
        }
        let ids = app
            .store
            .call(|c| {
                let mut query =
                    c.prepare("SELECT substr(name,8) FROM preferences WHERE name LIKE 'job-ui:%'")?;
                Ok(query
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .await?;
        deliveries.retain(|id, _| ids.contains(id));
        for id in ids {
            update(&app, &id, &mut deliveries).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "progress_tests.rs"]
pub(crate) mod tests;
