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
    if let Some(bot) = &app.bot {
        let truncated = text.chars().take(3800).collect::<String>();
        let _ = bot.send_message(ChatId(chat), truncated).await;
    }
}

#[derive(Serialize, Deserialize)]
struct CaptionRecord {
    id: i32,
    caption: String,
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
    for (index, (_, id)) in ids.iter().enumerate() {
        recent.messages.push(CaptionRecord {
            id: *id,
            caption: if index == 0 {
                caption.into()
            } else {
                String::new()
            },
        });
    }
    if recent.messages.len() > 100 {
        recent.messages.drain(..recent.messages.len() - 100);
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
    for tag in text
        .split_whitespace()
        .filter(|s| s.starts_with('#') && s.len() > 1 && s.chars().count() <= 64)
    {
        if !recent.tags.iter().any(|t| t == tag) {
            anyhow::ensure!(recent.tags.len() < 32, "too_many_tags");
            recent.tags.push(tag.into());
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
    for message in &recent.messages {
        let caption = if message.caption.is_empty() {
            recent.tags.join(" ")
        } else {
            format!("{}\n{}", message.caption, recent.tags.join(" "))
        };
        anyhow::ensure!(caption.encode_utf16().count() <= 1024, "caption_too_long");
        bot.edit_message_caption(recipient(&recent.target), MessageId(message.id))
            .caption(caption)
            .await
            .map_err(|_| anyhow::anyhow!("tag_edit_failed"))?;
    }
    app.store
        .set_preference(&key, &serde_json::to_string(&recent)?)
        .await?;
    Ok(format!("已补标 {} 条媒体。", recent.messages.len()))
}

fn number(value: &Value, path: &str) -> Option<i64> {
    value.pointer(path).and_then(Value::as_i64)
}

pub async fn handle(app: &App, value: &Value) -> anyhow::Result<()> {
    let update = value.get("update_id").and_then(Value::as_i64).unwrap_or(0) as i32;
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
        if let Some(data) = callback
            .get("data")
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix("batch:"))
        {
            let target = app.target().await?;
            anyhow::ensure!(!target.is_empty(), "missing_target");
            let payload = JobPayload::Transfer {
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
    if message.pointer("/chat/type").and_then(Value::as_str) != Some("private") {
        return Ok(());
    }
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
    let response = command(app, chat, text, Some(update)).await?;
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
        "/start"|"/help"=>Ok("归巢 Rust\n转发媒体自动转存；#标签 补标\n/search 关键词 [页数|continue]\n/grab 密钥…：copy\n/fetch 密钥…：deep\n/batch：批次列表\n/batch ID [起] [止] [redo]\n/copy /deep /mode\n/bind 群ID；/target\n/status /stop [任务ID] /clear\n/retry 任务ID\n/chats：账号群组\n/log：最近运行事件\nWeb 管理通过本机 8787 端口访问。".into()),
        "/copy"|"/deep"=>{let mode=&command[1..];app.store.set_preference("mode",mode).await?;Ok(format!("已切换为 {mode}。"))},
        "/mode"=>Ok(format!("当前模式：{}",app.mode().await?.as_str())),
        "/target"=>Ok(format!("目标：{}",app.target().await?)),
        "/bind"=>{
            let target=words.get(1).copied().unwrap_or("");
            anyhow::ensure!(!target.is_empty(),"missing_target");
            let target=if target=="reset"{app.config.default_target.as_str()}else{target};
            if let Some(bot)=&app.bot {bot.get_chat(recipient(target)).await.map_err(|_|anyhow::anyhow!("target_not_found"))?;}
            app.store.set_preference("target",target).await?;Ok("目标已保存；已排队任务仍使用入队时的目标。".into())
        },
        "/status"=>{let jobs=app.store.jobs(10,0).await?;Ok(jobs.iter().map(|j|format!("{} {} {} {}",j.id,j.kind,j.status,j.completed)).collect::<Vec<_>>().join("\n"))},
        "/stop"=>{
            let active=app.store.jobs(100,0).await?.into_iter().find(|j|j.status=="running" && j.kind!="incoming_copy").map(|j|j.id);
            let id=words.get(1).map(|s|s.to_string()).or(active).ok_or_else(||anyhow::anyhow!("no_active_job"))?;
            app.cancel(&id).await?;Ok("停止请求已提交，已完成记录保留。".into())
        },
        "/clear"=>Ok(format!("已清空 {} 个排队任务。",app.store.clear_queue().await?)),
        "/retry"=>{let id=words.get(1).ok_or_else(||anyhow::anyhow!("missing_job_id"))?;app.store.retry(id,false).await?;app.notify.notify_one();Ok("已重新排队。".into())},
        "/search"=>{
            let mut args=words[1..].to_vec();let mut pages=None;let mut resume=false;
            if args.last()==Some(&"continue") {args.pop();resume=true;}
            else if let Some(n)=args.last().and_then(|s|s.parse::<u32>().ok()) {args.pop();pages=Some(n);}
            let id=app.enqueue(JobPayload::Search{keyword:args.join(" "),pages,sort:None,resume},Some(chat),update).await?;
            Ok(if id.is_empty(){"任务已接收。".into()}else{format!("搜索已排队：{id}")})
        },
        "/batch"|"/grab"|"/fetch" if words.len()==1=>{
            let batches=app.store.batches(20,0).await?;
            if batches.is_empty(){return Ok("密钥夹为空。".into());}
            let mut lines=vec![];
            for batch in batches {let keyword=app.store.batch_keyword(&batch.id).await?;lines.push(format!("{}\n{}：{} 条，第 {} 页",batch.id,keyword,batch.entries,batch.page));}
            Ok(lines.join("\n\n"))
        },
        "/batch"=>{
            let id=words[1];
            let (batch,start)=if id=="continue" {
                let batch=app.store.preference("last_batch").await?.ok_or_else(||anyhow::anyhow!("no_previous_batch"))?;
                (batch,words.get(2).and_then(|s|s.parse().ok()).unwrap_or(1))
            }else{(id.into(),words.get(2).and_then(|s|s.parse().ok()).unwrap_or(1))};
            let end=words.get(3).and_then(|s|s.parse().ok());
            let target=app.target().await?;anyhow::ensure!(!target.is_empty(),"missing_target");
            let id=app.enqueue(JobPayload::Transfer{keys:vec![],batch:Some(batch.clone()),start,end,mode:app.mode().await?,target,redo:words.contains(&"redo"),dry_run:words.contains(&"--dry")},Some(chat),update).await?;
            app.store.set_preference("last_batch",&batch).await?;Ok(format!("批量转存已排队：{id}"))
        },
        "/grab"|"/fetch"=>{
            let keys=words[1..].iter().filter(|s|!s.starts_with("--")).map(|s|s.to_string()).collect();
            let target=app.target().await?;anyhow::ensure!(!target.is_empty(),"missing_target");
            let mode=if command=="/fetch"{TransferMode::Deep}else{TransferMode::Copy};
            let id=app.enqueue(JobPayload::Transfer{keys,batch:None,start:1,end:None,mode,target,redo:words.contains(&"--redo"),dry_run:words.contains(&"--dry")},Some(chat),update).await?;
            Ok(format!("转存已排队：{id}"))
        },
        "/chats"=>{
            let token=app.shutdown.child_token();
            let account=app.users.account(false,&token).await?;let mut dialogs=account.client.iter_dialogs();let mut lines=vec![];
            while let Some(dialog)=crate::telegram::bounded(&token,app.config.limits.request_timeout_secs,async {dialogs.next().await.map_err(crate::telegram::rpc)}).await? {
                let peer=dialog.peer();lines.push(format!("{} {}",peer.id().bot_api_dialog_id_unchecked(),peer.name().unwrap_or("")));
                if lines.len()>=50 {break;}
            }
            Ok(lines.join("\n"))
        },
        "/log"=>{
            let logs=app.store.jobs(10,0).await?;
            Ok(logs.iter().map(|j|format!("{} {} {} {}",j.updated_at,j.id,j.status,j.error_code.as_deref().unwrap_or("ok"))).collect::<Vec<_>>().join("\n"))
        },
        _ if !text.starts_with('/') && words.len()==1=>{
            let target=app.target().await?;anyhow::ensure!(!target.is_empty(),"missing_target");
            let id=app.enqueue(JobPayload::Transfer{keys:vec![text.trim().into()],batch:None,start:1,end:None,mode:app.mode().await?,target,redo:false,dry_run:false},Some(chat),update).await?;
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
            result=bot.get_updates().offset(offset).limit(20).timeout(app.config.limits.poll_secs).allowed_updates(vec![AllowedUpdate::Message,AllowedUpdate::CallbackQuery])=>{
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
