use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use tokio::sync::mpsc;

use crate::{
    bot::{ApiError, Bot, Sender},
    commands::{self, Command},
    config::Config,
    paths::Paths,
    pipeline::{Seen, is_live, message_link, now_unix},
    rules::Rules,
    telegram::{Event, Telegram},
};

fn control_reserve(capacity: usize) -> usize {
    (capacity / 8).clamp(1, 8).min(capacity.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reservation_never_starves_message_lane() {
        for capacity in 1..=1024 {
            assert!(super::control_reserve(capacity) < capacity);
        }
        assert_eq!(super::control_reserve(1), 0);
    }
}

#[derive(Default)]
struct Stats {
    matched: AtomicU64,
    dropped: AtomicU64,
    delivered: AtomicU64,
    failed: AtomicU64,
}

enum Outbound {
    Match(Event, Instant),
    Text(String, Instant),
}

pub async fn run(config: &Config, paths: &Paths) -> Result<()> {
    let rules = Arc::new(Mutex::new(Rules::load(paths, config.max_rules)?));
    let bot = Bot::new(&config.bot_token, config.owner_id, config.relay_chat_id)?;
    tokio::select! {
        result = bot.check() => result?,
        result = crate::control::wait_for_stop(paths) => return result,
    }
    let stats = Arc::new(Stats::default());
    loop {
        let connection = tokio::select! {
            result = Telegram::connect(config, paths) => result,
            result = crate::control::wait_for_stop(paths) => return result,
        };
        let telegram = match connection {
            Ok(t) => t,
            Err(_) => {
                // Includes authorization failures: do not repeatedly attempt login.
                bail!("Telegram connection/authorization failed; check connectivity or run login")
            }
        };
        let groups: std::collections::BTreeSet<_> = rules
            .lock()
            .expect("rules lock")
            .entries()
            .into_iter()
            .map(|(chat, _)| chat)
            .collect();
        for chat in groups {
            let validated = tokio::select! {
                result = telegram.validate_group(chat) => result,
                result = crate::control::wait_for_stop(paths) => { telegram.close().await; return result; }
            };
            if validated.is_err() {
                telegram.close().await;
                bail!("a watched supergroup is unavailable; check membership and rules");
            }
        }
        let boundary = now_unix();
        let result = tokio::select! {
            result = session(config, &bot, &telegram, rules.clone(), stats.clone(), boundary) => result,
            result = crate::control::wait_for_stop(paths) => {
                telegram.close().await;
                return result;
            }
        };
        telegram.close().await;
        match result {
            Ok(()) => return Ok(()),
            Err(_) => {
                tracing::warn!("Telegram session ended; discarding pending tasks before reconnect");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                    result = crate::control::wait_for_stop(paths) => return result,
                }
            }
        }
    }
}

async fn session(
    config: &Config,
    bot: &Bot,
    telegram: &Telegram,
    rules: Arc<Mutex<Rules>>,
    stats: Arc<Stats>,
    boundary: i64,
) -> Result<()> {
    // Raw Telegram updates have their own bounded queue; both application queues are bounded.
    let (events_tx, mut events_rx) = mpsc::channel(config.queue_capacity);
    let (out_tx, mut out_rx) = mpsc::channel(config.queue_capacity);
    let receive = async {
        let mut seen = Seen::new(2048, Duration::from_secs(600));
        while let Some(event) = events_rx.recv().await {
            let event: Event = event;
            if !is_live(event.date, boundary, now_unix(), config.task_ttl_secs)
                || !rules
                    .lock()
                    .expect("rules lock")
                    .contains(event.chat_id, event.user_id)
            {
                continue;
            }
            if !seen.insert((event.chat_id, event.message_id), Instant::now()) {
                continue;
            }
            stats.matched.fetch_add(1, Ordering::Relaxed);
            let remaining = config
                .task_ttl_secs
                .saturating_sub(now_unix().saturating_sub(event.date) as u64);
            let deadline = Instant::now() + Duration::from_secs(remaining);
            // Reserve a few slots for owner replies without creating an unbounded control lane.
            let reserve = control_reserve(config.queue_capacity);
            if out_tx.capacity() <= reserve
                || out_tx.try_send(Outbound::Match(event, deadline)).is_err()
            {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        bail!("Telegram event stream closed")
    };
    let deliver = async {
        let mut sender = Sender::new(bot.clone());
        while let Some(task) = out_rx.recv().await {
            let result = match task {
                Outbound::Text(text, deadline) => sender.text(&text, None, deadline).await,
                Outbound::Match(event, deadline) => {
                    deliver_match(telegram, &mut sender, event, deadline).await
                }
            };
            if result.is_ok() {
                stats.delivered.fetch_add(1, Ordering::Relaxed);
            } else {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("outbound task failed or expired");
            }
        }
        bail!("outbound queue closed")
    };
    tokio::select! {
        result = telegram.pump(events_tx) => result,
        result = receive => result,
        result = deliver => result,
        result = manage(bot, telegram, rules.clone(), stats.clone(), out_tx.clone()) => result,
    }
}

async fn deliver_match(
    telegram: &Telegram,
    sender: &mut Sender,
    event: Event,
    deadline: Instant,
) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("expired");
    }
    let link = message_link(event.chat_id, event.message_id)?;
    if event.protected {
        return sender
            .text(
                "关注的用户发送了消息，但原群禁止转发。",
                Some(&link),
                deadline,
            )
            .await;
    }
    let relay = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        telegram.forward(event.peer, event.message_id),
    )
    .await??;
    // Do not retry an unknown forward outcome; successful stages are never repeated.
    sender.forward(relay, deadline).await?;
    sender
        .text(
            &format!(
                "关注提醒：用户 {} 在群 {} 发了消息。",
                event.user_id, event.chat_id
            ),
            Some(&link),
            deadline,
        )
        .await?;
    // Best effort only; relay auto-delete is the crash/failure cleanup fallback.
    if !matches!(
        tokio::time::timeout(Duration::from_secs(10), telegram.delete_relay(relay)).await,
        Ok(Ok(()))
    ) {
        tracing::warn!("relay cleanup failed; use relay auto-delete as fallback");
    }
    Ok(())
}

async fn manage(
    bot: &Bot,
    telegram: &Telegram,
    rules: Arc<Mutex<Rules>>,
    stats: Arc<Stats>,
    out: mpsc::Sender<Outbound>,
) -> Result<()> {
    // Negative offset asks for only the newest pending update and forgets older commands.
    // Discard that last update too; commands from downtime must never execute.
    let mut offset = match bot.updates(-1, 0, 1).await {
        Ok(batch) => batch.last().map_or(0, |u| u.update_id + 1),
        Err(_) => bail!("cannot discard old Bot updates"),
    };
    loop {
        let batch = match bot.updates(offset, 25, 20).await {
            Ok(batch) => batch,
            Err(ApiError::RateLimited(wait)) => {
                tokio::time::sleep(wait).await;
                continue;
            }
            Err(ApiError::Rejected(401 | 409)) => {
                bail!("Bot authorization or competing poller error")
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        for update in batch {
            offset = update.update_id + 1;
            let Some(text) = bot.authorized_text(&update) else {
                continue;
            };
            if out.capacity() == 0 {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let reply = match commands::parse(text) {
                Ok(command) => match execute(command, telegram, &rules, &stats, out.capacity()).await {
                    Ok(text) => text,
                    Err(_) => "操作失败：请检查群类型、用户 ID、权限或限流情况。配置和名单未确认保存前不会报告成功。".into(),
                },
                Err(_) => "命令格式错误，请发送 /help".into(),
            };
            if out
                .try_send(Outbound::Text(
                    reply,
                    Instant::now() + Duration::from_secs(60),
                ))
                .is_err()
            {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn execute(
    command: Command,
    telegram: &Telegram,
    rules: &Mutex<Rules>,
    stats: &Stats,
    available: usize,
) -> Result<String> {
    match command {
        Command::Help => Ok(commands::HELP.into()),
        Command::Groups(page) => telegram.groups(page).await,
        Command::Watch(chat, user) => {
            let user = telegram.resolve_user(&user).await?;
            let was_watched = rules
                .lock()
                .expect("rules lock")
                .entries()
                .iter()
                .any(|(id, _)| *id == chat);
            telegram.validate_group(chat).await?;
            let saved = rules.lock().expect("rules lock").add(chat, user);
            if saved.is_err() && !was_watched {
                telegram.forget_group(chat);
            }
            let added = saved?;
            Ok(if added {
                format!("已关注：群 {chat}，用户 {user}")
            } else {
                "该关注规则已存在。".into()
            })
        }
        Command::Unwatch(chat, user) => {
            let removed = rules.lock().expect("rules lock").remove(chat, user)?;
            if !rules
                .lock()
                .expect("rules lock")
                .entries()
                .iter()
                .any(|(id, _)| *id == chat)
            {
                telegram.forget_group(chat);
            }
            Ok(if removed {
                "已取消关注。"
            } else {
                "规则不存在。"
            }
            .into())
        }
        Command::List(page) => {
            let entries = rules.lock().expect("rules lock").entries();
            let lines: Vec<_> = entries
                .iter()
                .skip((page - 1) * 20)
                .take(20)
                .map(|(c, u)| format!("群 {c} / 用户 {u}"))
                .collect();
            Ok(format!(
                "关注名单（共 {} 条，第 {page} 页）\n{}",
                entries.len(),
                lines.join("\n")
            ))
        }
        Command::Status => Ok(format!(
            "运行中（仅实时，无历史补发）\n关注规则：{}\n发送队列空位：{available}\n命中：{}\n完成发送任务：{}\n丢弃：{}\n失败：{}",
            rules.lock().expect("rules lock").entries().len(),
            stats.matched.load(Ordering::Relaxed),
            stats.delivered.load(Ordering::Relaxed),
            stats.dropped.load(Ordering::Relaxed),
            stats.failed.load(Ordering::Relaxed)
        )),
    }
}
