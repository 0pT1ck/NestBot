use super::commands;
use crate::{
    application::App,
    domain::{JobPayload, TransferMode, unix_time},
    telemetry,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use teloxide::{
    prelude::*,
    types::{AllowedUpdate, MessageId, Recipient},
};

pub fn recipient(target: &str) -> Recipient {
    target
        .parse::<i64>()
        .map(|id| Recipient::Id(ChatId(id)))
        .unwrap_or_else(|_| Recipient::ChannelUsername(target.into()))
}

pub async fn send(app: &App, chat: i64, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(id) = super::progress::receipt_job(text) {
        if super::progress::receipt(app, chat, text, id).await.is_err() {
            tracing::warn!(event = "bot_receipt_failed");
        }
        return;
    }
    if let Some(bot) = &app.bot {
        let truncated = text.chars().take(3800).collect::<String>();
        let _ = bot.send_message(ChatId(chat), truncated).await;
    }
}

async fn send_buttons(
    app: &App,
    chat: i64,
    text: &str,
    rows: Vec<Vec<teloxide::types::InlineKeyboardButton>>,
) {
    if let Some(bot) = &app.bot {
        let _ = bot
            .send_message(ChatId(chat), text.chars().take(3800).collect::<String>())
            .reply_markup(teloxide::types::InlineKeyboardMarkup::new(rows))
            .await;
    }
}

pub async fn search_result(
    app: &App,
    job: &crate::domain::Job,
    keyword: &str,
) -> anyhow::Result<()> {
    let Some(owner) = job.reply_chat else {
        return Ok(());
    };
    let batch = app.store.vault.index("keyword", keyword);
    let entries = app
        .store
        .selected_entries(&job.summary.id, &batch, 1, 20)
        .await?;
    let count = app.store.selection_count(&job.summary.id).await?;
    if count == 0 {
        send(
            app,
            owner,
            "没有新增可领取的密钥；结果可能为空，或已经到最后一页。",
        )
        .await;
        return Ok(());
    }
    let display_keyword = if app.config.telegram.mask_output {
        "text"
    } else {
        keyword
    };
    let mut lines = vec![format!(
        "🔎 关键词：{display_keyword}，本次收集 {count} 条\n数据库批次：{batch}"
    )];
    let mut buttons = vec![];
    for (index, (seq, entry)) in entries.iter().enumerate() {
        let description = if app.config.telegram.mask_output {
            "text"
        } else {
            entry.description.as_str()
        };
        lines.push(format!(
            "{}. {}{}",
            index + 1,
            description,
            entry
                .file_count
                .map(|n| format!("｜📦 {n} 个"))
                .unwrap_or_default()
        ));
        let payload = JobPayload::Transfer {
            options: Default::default(),
            keys: vec![],
            batch: Some(batch.clone()),
            start: *seq,
            end: Some(*seq),
            mode: app.mode().await?,
            target: app.target().await?,
            redo: false,
            dry_run: false,
        };
        let data = app
            .store
            .callback(owner, &serde_json::json!({"payload":payload}))
            .await?;
        buttons.push(teloxide::types::InlineKeyboardButton::callback(
            format!("⬆ {}", index + 1),
            data,
        ));
    }
    let mut rows = buttons.chunks(5).map(<[_]>::to_vec).collect::<Vec<_>>();
    let options = crate::domain::TransferOptions {
        selection: Some(job.summary.id.clone()),
        ..Default::default()
    };
    let payload = JobPayload::Transfer {
        options,
        keys: vec![],
        batch: Some(batch),
        start: 1,
        end: None,
        mode: app.mode().await?,
        target: app.target().await?,
        redo: false,
        dry_run: false,
    };
    let data = app
        .store
        .callback(owner, &serde_json::json!({"payload":payload}))
        .await?;
    rows.push(vec![teloxide::types::InlineKeyboardButton::callback(
        format!("⬆ 全部 {count} 条"),
        data,
    )]);
    send_buttons(app, owner, &lines.join("\n"), rows).await;
    Ok(())
}

async fn batch_command(
    app: &App,
    owner: i64,
    args: &[&str],
    update: Option<i32>,
) -> anyhow::Result<String> {
    let reference = args[0];
    let continuing = reference.eq_ignore_ascii_case("continue");
    let password = crate::config::Config::env_secret("LEGACY_VAULT_PASSWORD")
        .or_else(|| crate::config::Config::env_secret(&app.config.web.vault_password_env));
    let batch = if continuing {
        app.store
            .preference("last_batch")
            .await?
            .ok_or_else(|| anyhow::anyhow!("no_previous_batch"))?
    } else {
        match app.store.resolve_batch(reference,password.as_ref().map(|s|s.as_str())).await {
            Ok(batch)=>batch,
            Err(error) if error.to_string()=="batch_not_found" => return Ok("找不到该密钥夹；发送 /batch 查看名称和批次 ID。旧 .bin 文件名需已导入，或使用对应旧密码。".into()),
            Err(error)=>return Err(error),
        }
    };
    if args
        .get(1)
        .is_some_and(|s| ["progress", "set", "进度", "已转"].contains(s))
    {
        let Some(completed) = args
            .get(2)
            .filter(|_| args.len() == 3)
            .and_then(|s| s.parse::<u32>().ok())
        else {
            return Ok(
                "用法：/batch 名称 progress 已转条数，例如 /batch test progress 11；0 表示重置。"
                    .into(),
            );
        };
        match app.store.set_batch_progress(&batch, completed).await {
            Ok(()) => {}
            Err(error) if error.to_string() == "invalid_batch_progress" => {
                return Ok("已转条数不能超过该密钥夹的总条数。".into());
            }
            Err(error) if error.to_string() == "batch_busy" => {
                return Ok("该批次正在转存，请先 /stop，等待任务停止后再修改进度。".into());
            }
            Err(error) => return Err(error),
        }
        app.store.set_preference("last_batch", &batch).await?;
        app.store
            .set_preference("last_batch_start", &(completed + 1).to_string())
            .await?;
        let keyword = app.store.batch_keyword(&batch).await?;
        let next = app.store.batch_next_pending(&batch).await?;
        return Ok(format!(
            "{keyword}：已转条数设为 {completed}。前 {completed} 条视为已转，其余重设为未转。{}",
            next.map(|n| format!("下次从第 {n} 条开始。"))
                .unwrap_or_else(|| "该密钥夹已全部完成。".into())
        ));
    }
    let numbers = args[1..]
        .iter()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect::<Vec<_>>();
    let redo = args[1..].contains(&"redo");
    let start = if let Some(start) = numbers.first() {
        *start
    } else if redo {
        1
    } else if continuing {
        app.store
            .preference("last_batch_start")
            .await?
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    } else if let Some(next) = app.store.batch_next_pending(&batch).await? {
        next
    } else {
        return Ok("该密钥夹已全部转存；加 redo 可强制重转，或用 progress 修改已转条数。".into());
    };
    let end = if continuing {
        None
    } else {
        numbers.get(1).copied()
    };
    if start == 0
        || end.is_some_and(|n| n < start)
        || app.store.entries(&batch, start, 1).await?.is_empty()
    {
        return Ok("序号范围无效；起止序号从 1 开始，且起始序号不能超过总条数。".into());
    }
    let target = app.target().await?;
    if target.trim().is_empty() {
        return Ok("尚未设置转存目标。请先 /bind 群ID，或填写 config/nestbot.toml 的 default_target，再重启服务。".into());
    }
    let options = crate::domain::TransferOptions {
        confirmed: continuing,
        ..Default::default()
    };
    let id = app
        .enqueue(
            JobPayload::Transfer {
                options,
                keys: vec![],
                batch: Some(batch.clone()),
                start,
                end,
                mode: app.mode().await?,
                target,
                redo,
                dry_run: args[1..].contains(&"--dry"),
            },
            Some(owner),
            update,
        )
        .await?;
    app.store.set_preference("last_batch", &batch).await?;
    app.store
        .set_preference("last_batch_start", &start.to_string())
        .await?;
    Ok(format!(
        "批量转存已排队：{id}\n范围：第 {start} 条到{}。",
        end.map(|n| format!("第 {n} 条"))
            .unwrap_or_else(|| "末条".into())
    ))
}

async fn batch_list(app: &App, owner: i64) -> anyhow::Result<String> {
    let mut lines = vec![];
    let mut rows = vec![];
    for batch in app.store.batches(30, 0).await? {
        let keyword = app.store.batch_keyword(&batch.id).await?;
        let mut offset = 1;
        let mut done = 0;
        let mut next = None;
        loop {
            let entries = app.store.entries(&batch.id, offset, 64).await?;
            if entries.is_empty() {
                break;
            }
            for (seq, entry) in entries {
                if app
                    .store
                    .batch_entry_complete(&batch.id, seq, &entry)
                    .await?
                {
                    done += 1;
                } else if next.is_none() {
                    next = Some(seq);
                }
                offset = seq + 1;
            }
        }
        let line = format!(
            "{}：{} 条，已转 {done}\n{}",
            keyword, batch.entries, batch.id
        );
        lines.push(line.clone());
        let payload = JobPayload::Transfer {
            options: Default::default(),
            keys: vec![],
            batch: Some(batch.id),
            start: next.unwrap_or(1),
            end: None,
            mode: app.mode().await?,
            target: app.target().await?,
            redo: false,
            dry_run: false,
        };
        rows.push(vec![teloxide::types::InlineKeyboardButton::callback(
            format!("▶ {keyword}（{done}/{}）", batch.entries),
            app.store
                .callback(owner, &serde_json::json!({"payload":payload}))
                .await?,
        )]);
    }
    if lines.is_empty() {
        return Ok("密钥夹为空，请先 /search 收集。".into());
    }
    if app.bot.is_some() {
        send_buttons(app, owner, &lines.join("\n\n"), rows).await;
        Ok(String::new())
    } else {
        Ok(lines.join("\n\n"))
    }
}

pub async fn batch_confirmation(
    app: &App,
    job: &crate::domain::Job,
    batch: &str,
    start: u32,
    end: Option<u32>,
) -> anyhow::Result<bool> {
    let Some(owner) = job.reply_chat else {
        return Ok(true);
    };
    let (mut done, mut pending, mut offset) = (0, 0, start);
    loop {
        let entries = app.store.entries(batch, offset, 64).await?;
        if entries.is_empty() {
            break;
        }
        for (seq, entry) in entries {
            if end.is_some_and(|n| seq > n) {
                break;
            }
            if app.store.batch_entry_complete(batch, seq, &entry).await? {
                done += 1;
            } else {
                pending += 1;
            }
            offset = seq + 1;
        }
        if end.is_some_and(|n| offset > n) {
            break;
        }
    }
    if pending == 0 {
        send(
            app,
            owner,
            "选定序号段已全部转存，无需处理；加 redo 可强制重转。",
        )
        .await;
        return Ok(false);
    }
    if done == 0 {
        return Ok(true);
    }
    let mut rows = vec![];
    for redo in [false, true] {
        let mut payload = job.payload.clone();
        if let JobPayload::Transfer {
            options,
            redo: flag,
            ..
        } = &mut payload
        {
            options.confirmed = true;
            *flag = redo;
        }
        let data = app
            .store
            .callback(owner, &serde_json::json!({"payload":payload}))
            .await?;
        rows.push(vec![teloxide::types::InlineKeyboardButton::callback(
            if redo {
                "🔄 从头重转"
            } else {
                "▶ 继承进度"
            },
            data,
        )]);
    }
    send_buttons(
        app,
        owner,
        &format!("上次已有进度：已完成 {done} 个，待转 {pending} 个。继承进度继续，还是从头重转？"),
        rows,
    )
    .await;
    app.store
        .warning(&job.summary.id, "batch_confirmation_required")
        .await?;
    Ok(false)
}

#[derive(Serialize, Deserialize)]
struct CaptionRecord {
    id: i32,
    caption: String,
    #[serde(default = "carrier_default")]
    carrier: bool,
}
fn carrier_default() -> bool {
    true
}
#[derive(Serialize, Deserialize)]
struct RecentMedia {
    target: String,
    messages: Vec<CaptionRecord>,
    tags: Vec<String>,
    updated_at: i64,
}

pub async fn remember_media(
    app: &App,
    owner: i64,
    target: &str,
    ids: &[(i32, i32)],
    caption: &str,
) -> anyhow::Result<()> {
    remember_media_carrier(app, owner, target, ids, caption, None).await
}

pub async fn remember_media_carrier(
    app: &App,
    owner: i64,
    target: &str,
    ids: &[(i32, i32)],
    caption: &str,
    carrier: Option<i32>,
) -> anyhow::Result<()> {
    let key = format!("media:{owner}");
    let previous = app
        .store
        .preference(&key)
        .await?
        .and_then(|s| serde_json::from_str::<RecentMedia>(&s).ok());
    let mut recent = previous
        .filter(|r| r.target == target && unix_time() - r.updated_at <= 10)
        .unwrap_or_else(|| RecentMedia {
            target: target.into(),
            messages: vec![],
            tags: vec![],
            updated_at: unix_time(),
        });
    for (index, (source, id)) in ids.iter().enumerate() {
        let is_carrier = carrier.map_or(index == 0, |source_id| source_id == *source);
        recent.messages.push(CaptionRecord {
            id: *id,
            caption: if is_carrier {
                caption.into()
            } else {
                String::new()
            },
            carrier: is_carrier,
        });
    }
    recent.updated_at = unix_time();
    app.store
        .set_preference(&key, &serde_json::to_string(&recent)?)
        .await
}

async fn tags(app: &App, owner: i64, text: &str) -> anyhow::Result<String> {
    let key = format!("media:{owner}");
    let Some(serialized) = app.store.preference(&key).await? else {
        return Ok("还没有可补标的媒体批次。".into());
    };
    let mut recent: RecentMedia = serde_json::from_str(&serialized)?;
    let mut changed = false;
    for tag in commands::tags(text) {
        if !recent.tags.contains(&tag) {
            recent.tags.push(tag);
            changed = true;
        }
    }
    if !changed {
        return Ok("标签已经存在。".into());
    }
    let bot = app
        .bot
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("bot_not_configured"))?;
    let (mut ok, mut failed) = (0, 0);
    for message in recent.messages.iter().filter(|m| m.carrier) {
        let caption = commands::caption(&message.caption, &recent.tags);
        anyhow::ensure!(caption.chars().count() <= 1024, "caption_too_long");
        match bot
            .edit_message_caption(recipient(&recent.target), MessageId(message.id))
            .caption(caption)
            .await
        {
            Ok(_) | Err(teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)) => {
                ok += 1
            }
            Err(_) => failed += 1,
        }
    }
    app.store
        .set_preference(&key, &serde_json::to_string(&recent)?)
        .await?;
    Ok(format!("已补标 {ok} 条媒体，{failed} 条编辑失败。"))
}

async fn remember_chat(app: &App, chat: &Value) -> anyhow::Result<()> {
    let Some(id) = chat.get("id").and_then(Value::as_i64).filter(|n| *n < 0) else {
        return Ok(());
    };
    let mut seen: Vec<(i64, String)> = app
        .store
        .preference("seen_chats")
        .await?
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default();
    seen.retain(|(old, _)| *old != id);
    seen.push((
        id,
        chat.get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
    ));
    if seen.len() > 30 {
        seen.remove(0);
    }
    app.store
        .set_preference("seen_chats", &serde_json::to_string(&seen)?)
        .await
}

fn number(value: &Value, path: &str) -> Option<i64> {
    value.pointer(path).and_then(Value::as_i64)
}

pub async fn handle(app: &App, value: &Value) -> anyhow::Result<()> {
    let update = value.get("update_id").and_then(Value::as_i64).unwrap_or(0) as i32;
    if let Some(chat) = value.pointer("/my_chat_member/chat") {
        remember_chat(app, chat).await?;
        return Ok(());
    }
    if let Some(callback) = value.get("callback_query") {
        let owner = number(callback, "/from/id").unwrap_or(0);
        if !app.config.telegram.allowed_users.contains(&owner) {
            return Ok(());
        }
        if let (Some(bot), Some(id)) = (&app.bot, callback.get("id").and_then(Value::as_str)) {
            let _ = bot
                .answer_callback_query(teloxide::types::CallbackQueryId(id.into()))
                .await;
        }
        if let Some(token) = callback
            .get("data")
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix("C:"))
        {
            if let Some(action) = app.store.callback_value(owner, token).await? {
                if let Some(target) = action.get("bind").and_then(Value::as_str) {
                    send(
                        app,
                        owner,
                        &command(app, owner, &format!("/bind {target}"), Some(update)).await?,
                    )
                    .await;
                } else if let Some(payload) = action.get("payload") {
                    let mut payload: JobPayload = serde_json::from_value(payload.clone())?;
                    if let JobPayload::Transfer {
                        mode,
                        target,
                        batch,
                        start,
                        ..
                    } = &mut payload
                    {
                        *mode = app.mode().await?;
                        *target = app.target().await?;
                        if let Some(batch) = batch {
                            app.store.set_preference("last_batch", batch).await?;
                            app.store
                                .set_preference("last_batch_start", &start.to_string())
                                .await?;
                        }
                    }
                    let id = app.enqueue(payload, Some(owner), Some(update)).await?;
                    send(app, owner, &format!("已排队：{id}")).await;
                }
            } else {
                send(app, owner, "按钮已过期，请重新打开批次列表或搜索结果。").await;
            }
            return Ok(());
        }
        if let Some(data) = callback
            .get("data")
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix("batch:"))
        {
            let target = app.target().await?;
            anyhow::ensure!(!target.is_empty(), "missing_target");
            let payload = JobPayload::Transfer {
                options: Default::default(),
                keys: vec![],
                batch: Some(data.into()),
                start: 1,
                end: None,
                mode: app.mode().await?,
                target,
                redo: false,
                dry_run: false,
            };
            let id = app.enqueue(payload, Some(owner), Some(update)).await?;
            if !id.is_empty() {
                send(app, owner, &format!("已排队：{id}")).await;
            }
        }
        return Ok(());
    }
    let Some(message) = value.get("message") else {
        return Ok(());
    };
    let owner = number(message, "/from/id").unwrap_or(0);
    let chat = number(message, "/chat/id").unwrap_or(0);
    if let Some(chat) = message.get("chat") {
        remember_chat(app, chat).await?;
    }
    let private = message.pointer("/chat/type").and_then(Value::as_str) == Some("private");
    if !app.config.telegram.allowed_users.contains(&owner) {
        if app.config.telegram.allowed_users.is_empty()
            && message.get("text").and_then(Value::as_str) == Some("/start")
        {
            send(
                app,
                chat,
                &format!(
                    "未配置白名单。请把你的用户 ID {owner} 加入 telegram.allowed_users 后重启。"
                ),
            )
            .await;
        }
        return Ok(());
    }
    if !private {
        let text = message.get("text").and_then(Value::as_str).unwrap_or("");
        let cmd = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("");
        if ![
            "/bind", "/target", "/status", "/stop", "/clear", "/log", "/start", "/help",
        ]
        .contains(&cmd)
        {
            return Ok(());
        }
    }
    let media = [
        "photo",
        "video",
        "document",
        "audio",
        "voice",
        "animation",
        "video_note",
        "sticker",
    ]
    .iter()
    .any(|k| message.get(k).is_some());
    if media {
        let target = app.target().await?;
        anyhow::ensure!(!target.is_empty(), "missing_target");
        let payload = JobPayload::Incoming {
            source_chat: chat,
            message_ids: vec![
                message
                    .get("message_id")
                    .and_then(Value::as_i64)
                    .unwrap_or(0) as i32,
            ],
            mode: app.mode().await?,
            target,
            caption_message: message
                .get("caption")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .and_then(|_| message.get("message_id").and_then(Value::as_i64))
                .map(|n| n as i32),
            caption: message
                .get("caption")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        let id = if let Some(group) = message.get("media_group_id").and_then(Value::as_str) {
            let id = app
                .store
                .enqueue_album(payload, chat, group, update)
                .await?;
            app.notify.notify_one();
            id
        } else {
            app.enqueue(payload, Some(chat), Some(update)).await?
        };
        if !id.is_empty() {
            send(app, chat, &format!("媒体已接收：{id}")).await;
        }
        return Ok(());
    }
    let Some(text) = message.get("text").and_then(Value::as_str) else {
        return Ok(());
    };
    if text.trim_start().starts_with('#') {
        send(app, chat, &tags(app, chat, text).await?).await;
        return Ok(());
    }
    let binding = if text.trim() == "/bind" {
        number(message, "/reply_to_message/forward_origin/chat/id")
            .filter(|id| *id < 0)
            .or(if !private { Some(chat) } else { None })
            .map(|id| format!("/bind {id}"))
    } else {
        None
    };
    let response = command(app, chat, binding.as_deref().unwrap_or(text), Some(update)).await?;
    send(app, chat, &response).await;
    Ok(())
}

pub async fn command(
    app: &App,
    chat: i64,
    text: &str,
    update: Option<i32>,
) -> anyhow::Result<String> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.is_empty() {
        return Ok(String::new());
    }
    let command = words[0].split('@').next().unwrap_or(words[0]);
    match command {
        "/start"|"/help"=>Ok("归巢 Rust\n转发媒体自动转存；#标签 补标\n/search 关键词 [页数|continue]\n/grab 密钥…：copy\n/fetch 密钥…：deep\n/batch：批次列表\n/batch 名称或ID [起] [止] [redo]\n/batch 名称 progress 已转条数\n/copy /deep /mode\n/bind 群ID；/target\n/status /stop [任务ID] /clear\n/retry 任务ID\n/chats：账号群组\n/log：最近运行事件\nWeb 管理通过本机 8787 端口访问。".into()),
        "/copy"|"/deep"=>{let mode=&command[1..];app.store.set_preference("mode",mode).await?;Ok(format!("已切换为 {mode}。"))},
        "/mode"=>Ok(format!("当前模式：{}",app.mode().await?.as_str())),
        "/target"=>Ok(format!("目标：{}",app.target().await?)),
        "/bind"=>{
            let target=words.get(1).copied().unwrap_or("");
            if target=="reset" {app.store.set_preference("target","").await?;return Ok("已清除绑定，改用配置文件中的默认目标。".into());}
            if target.is_empty() {
                let seen:Vec<(i64,String)>=app.store.preference("seen_chats").await?.map(|s|serde_json::from_str(&s)).transpose()?.unwrap_or_default();
                if seen.is_empty(){return Ok("请把 Bot 加入目标频道后再 /bind，或回复目标群的转发消息发送 /bind，也可 /bind 群ID。".into());}
                let mut rows=vec![];
                for(id,title)in seen {rows.push(vec![teloxide::types::InlineKeyboardButton::callback(title,app.store.callback(chat,&serde_json::json!({"bind":id.to_string()})).await?)]);}
                send_buttons(app,chat,"选择目标频道：",rows).await;return Ok(String::new());
            }
            if target.trim_start_matches('-').len()<6 || target.parse::<i64>().is_err(){return Ok("用法：/bind 群ID 或 /bind reset。".into());}
            app.store.set_preference("target",target).await?;
            let mut note=format!("已绑定目标：{target}。请确认主账号已加入目标并有发送权限。");
            if let Some(bot)=&app.bot && bot.get_chat(recipient(target)).await.is_err() {note.push_str("Bot 暂时无法访问该目标；copy 媒体需将 Bot 加入频道并授予发消息权限。");}
            Ok(note)
        },
        "/status"=>{
            let (current,queued)=app.store.call(|c| {
                let mut statement=c.prepare("SELECT id,kind,phase,completed,total FROM jobs WHERE status IN ('running','cancelling') AND kind!='incoming_copy' ORDER BY rowid")?;
                let current=statement.query_map([],|r|Ok((r.get::<_,String>(0)?,format!("{} {} {} {}/{}",r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,u64>(3)?,r.get::<_,Option<u64>>(4)?.map(|n|n.to_string()).unwrap_or_else(||"?".into())))))?.collect::<Result<Vec<_>,_>>()?;
                let queued=c.query_row("SELECT count(*) FROM jobs WHERE status IN ('queued','waiting','interrupted') AND kind!='incoming_copy'",[],|r|r.get::<_,u32>(0))?;
                Ok((current,queued))
            }).await?;
            let mut lines=Vec::with_capacity(current.len());
            for (id,mut line) in current {
                let report=app.store.report(&id).await?;
                if let Some(page)=report.search_page {line.push_str(&format!("\n当前搜索：第 {page} 页"));}
                if let Some(key)=report.transfer_key {line.push_str(&format!("\n正在转存第 {key} 个密钥"));}
                if let Some(wait)=super::progress::waiting(app,&id).await? {line.push_str(&format!("\nTelegram 要求等待 {} 秒，加 60 秒后自动继续；剩余 {} 秒。",wait.seconds,(wait.retry_at-unix_time()).max(0)));}
                if report.search_retry>0 {
                    let wait=report.search_retry_at.map(|at|(at-crate::domain::unix_time()).max(0));
                    line.push_str(&format!("\n搜索重试：第 {}/20 轮，已保存 {} 页；{}",report.search_retry,report.pages,wait.map(|seconds|format!("约 {seconds} 秒后点击原消息的下一页")).unwrap_or_else(||"正在点击并等待新回复".into())));
                }
                lines.push(line);
            }
            Ok(format!("任务：{}\n队列：{queued} 个等待\n处理方式：{}\n目标：{}",if lines.is_empty(){"空闲".into()}else{lines.join("\n")},app.mode().await?.as_str(),app.target().await?))
        },
        "/stop"=>{
            let active=app.store.call(|c| {use rusqlite::OptionalExtension;Ok(c.query_row("SELECT id FROM jobs WHERE status='running' AND kind!='incoming_copy' ORDER BY rowid LIMIT 1",[],|r|r.get::<_,String>(0)).optional()?)}).await?;
            let Some(id)=words.get(1).map(|s|s.to_string()).or(active) else {return Ok("停止信号已发出，当前没有正在运行的协议任务。".into());};
            app.cancel(&id).await?;Ok("停止请求已提交，已完成记录保留。".into())
        },
        "/clear"=>Ok(format!("已清空 {} 个排队任务。",app.store.clear_queue().await?)),
        "/retry"=>{let id=words.get(1).ok_or_else(||anyhow::anyhow!("missing_job_id"))?;app.store.retry(id,false).await?;app.notify.notify_one();Ok(format!("已重新排队：{id}"))},
        "/search"=>{
            let (keyword,pages,resume)=commands::search_args(&words[1..].join(" "));
            let id=app.enqueue(JobPayload::Search{keyword,pages,sort:None,resume},Some(chat),update).await?;
            Ok(if id.is_empty(){"任务已接收。".into()}else{format!("搜索已排队：{id}")})
        },
        "/batch"|"/grab"|"/fetch" if words.len()==1=>batch_list(app,chat).await,
        "/batch"=>batch_command(app,chat,&words[1..],update).await,
        "/grab"|"/fetch"=>{
            let (keys,redo,dry_run)=commands::fetch_args(&words[1..].join(" "));
            if keys.is_empty() {return batch_list(app,chat).await;}
            let target=app.target().await?;anyhow::ensure!(!target.is_empty(),"missing_target");
            let mode=if command=="/fetch"{TransferMode::Deep}else{TransferMode::Copy};
            let id=app.enqueue(JobPayload::Transfer{options: Default::default(),keys,batch:None,start:1,end:None,mode,target,redo:command=="/fetch" && redo,dry_run},Some(chat),update).await?;
            Ok(format!("转存已排队：{id}"))
        },
        "/chats"=>{
            let token=app.shutdown.child_token();
            let account=app.users.account(&token).await?;let mut dialogs=account.client.iter_dialogs();let mut lines=vec![];
            while let Some(dialog)=crate::telegram::bounded(&token,app.config.limits.request_timeout_secs,async {dialogs.next().await.map_err(crate::telegram::rpc)}).await? {
                let peer=dialog.peer();if matches!(peer,grammers_client::peer::Peer::User(_)){continue;}lines.push(format!("{} {}",peer.id().bot_api_dialog_id_unchecked(),peer.name().unwrap_or("")));
            }
            Ok(lines.join("\n"))
        },
        "/log"=>{
            let limit=words.get(1).and_then(|s|s.parse::<u32>().ok()).unwrap_or(50).clamp(20,200);
            let logs=app.store.jobs(limit,0).await?;
            Ok(logs.iter().map(|j|format!("{} {} {} {}",j.updated_at,j.id,j.status,j.error_code.as_deref().unwrap_or("ok"))).collect::<Vec<_>>().join("\n"))
        },
        _ if commands::looks_like_key(text)=>{
            let target=app.target().await?;anyhow::ensure!(!target.is_empty(),"missing_target");
            let id=app.enqueue(JobPayload::Transfer{options: Default::default(),keys:vec![text.trim().into()],batch:None,start:1,end:None,mode:app.mode().await?,target,redo:false,dry_run:false},Some(chat),update).await?;
            Ok(format!("转存已排队：{id}"))
        },
        _=>Ok("未识别命令，请发送 /help。".into()),
    }
}

pub async fn run(app: Arc<App>) -> anyhow::Result<()> {
    let Some(bot) = app.bot.clone() else {
        return Ok(());
    };
    let mut offset = app
        .store
        .preference("bot_offset")
        .await?
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    loop {
        let updates = tokio::select! {
            _=app.shutdown.cancelled()=>break,
            result=bot.get_updates().offset(offset).limit(20).timeout(app.config.limits.poll_secs).allowed_updates(vec![AllowedUpdate::Message,AllowedUpdate::CallbackQuery,AllowedUpdate::MyChatMember])=>{
                match result {Ok(updates)=>updates,Err(_)=>{tracing::warn!(event="bot_poll_failed");tokio::select!{_=app.shutdown.cancelled()=>break,_=tokio::time::sleep(std::time::Duration::from_secs(5))=>{}};continue;}}
            }
        };
        for update in updates {
            let value = serde_json::to_value(update)?;
            let next = value
                .get("update_id")
                .and_then(Value::as_i64)
                .unwrap_or(offset as i64) as i32
                + 1;
            if let Err(error) = handle(&app, &value).await {
                let code = telemetry::safe_error(&error);
                tracing::warn!(event = "bot_update_failed", error_code = code);
                if code == "queue_full" {
                    tokio::select! {
                        _=app.shutdown.cancelled()=>return Ok(()),
                        _=tokio::time::sleep(std::time::Duration::from_secs(2))=>{}
                    }
                    break;
                }
                // Do not acknowledge an update when persistence itself failed.
                if error.chain().any(|cause| cause.is::<rusqlite::Error>()) {
                    return Err(error);
                }
                if let Some(chat) = number(&value, "/message/chat/id")
                    && let Some(owner) = number(&value, "/message/from/id")
                    && app.config.telegram.allowed_users.contains(&owner)
                {
                    send(&app, chat, &format!("操作失败：{code}")).await;
                }
            }
            offset = next;
            app.store
                .set_preference("bot_offset", &offset.to_string())
                .await?;
        }
    }
    Ok(())
}
