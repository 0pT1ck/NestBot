use crate::{
    application::App,
    domain::{Job, TransferMode},
    telegram::{Account, bounded, rpc},
};
use grammers_client::{
    media::Media,
    message::{InputMessage, Message},
};
use grammers_session::types::PeerRef;
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod tests;

pub fn media_id(media: &Media) -> Option<String> {
    match media {
        Media::Photo(photo) => Some(format!("p:{}", photo.id())),
        Media::Document(doc) => Some(format!("d:{}", doc.id())),
        Media::Sticker(sticker) => Some(format!("d:{}", sticker.document.id())),
        _ => None,
    }
}

fn media_size(media: &Media) -> Option<usize> {
    match media {
        Media::Photo(p) => p.size(),
        Media::Document(d) => d.size(),
        Media::Sticker(s) => s.document.size(),
        _ => None,
    }
}

async fn cache_check(app: &App, size: u64) -> anyhow::Result<()> {
    anyhow::ensure!(size <= app.config.limits.max_file_bytes, "file_too_large");
    let path = app.config.paths.cache.clone();
    let (used, free) = tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, u64)> {
        let mut used = 0u64;
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                used = used.saturating_add(entry.metadata()?.len());
            }
        }
        Ok((used, fs2::available_space(path)?))
    })
    .await??;
    anyhow::ensure!(
        used.saturating_add(size) <= app.config.limits.max_cache_bytes
            && free >= size.saturating_add(app.config.limits.reserve_disk_bytes),
        "disk_full"
    );
    Ok(())
}

struct CountedFile {
    file: tokio::fs::File,
    count: Arc<AtomicU64>,
}
impl AsyncRead for CountedFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.file).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            self.count
                .fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        }
        result
    }
}

async fn download(
    app: &App,
    job: &Job,
    account: &Account,
    media: &Media,
    path: &Path,
    cancel: &CancellationToken,
) -> anyhow::Result<u64> {
    let size = media_size(media).ok_or_else(|| anyhow::anyhow!("unknown_file_size"))? as u64;
    cache_check(app, size).await?;
    let mut file = tokio::fs::File::create(path).await?;
    let mut chunks = account.client.iter_download(media).chunk_size(512 * 1024);
    let mut current = 0;
    let mut last = Instant::now();
    loop {
        let chunk = bounded(cancel, app.config.limits.transfer_stall_secs, async {
            chunks.next().await.map_err(rpc)
        })
        .await?;
        let Some(chunk) = chunk else {
            break;
        };
        current += chunk.len() as u64;
        anyhow::ensure!(
            current <= app.config.limits.max_file_bytes,
            "file_too_large"
        );
        file.write_all(&chunk).await?;
        if last.elapsed() >= Duration::from_secs(2) {
            app.progress(&job.summary.id, "download", current, Some(size))
                .await?;
            last = Instant::now();
        }
    }
    file.flush().await?;
    anyhow::ensure!(current == size, "download_incomplete");
    Ok(current)
}

fn document_info(
    media: &Media,
) -> (
    String,
    String,
    Vec<grammers_tl_types::enums::DocumentAttribute>,
) {
    let doc = match media {
        Media::Document(d) => Some(d),
        Media::Sticker(s) => Some(&s.document),
        _ => None,
    };
    let Some(doc) = doc else {
        return ("photo.jpg".into(), "image/jpeg".into(), vec![]);
    };
    let attrs =
        if let Some(grammers_tl_types::enums::Document::Document(document)) = &doc.raw.document {
            document
                .attributes
                .iter()
                .filter(|a| !matches!(a, grammers_tl_types::enums::DocumentAttribute::Sticker(_)))
                .cloned()
                .collect()
        } else {
            vec![]
        };
    (
        doc.name().unwrap_or("file.bin").into(),
        doc.mime_type().unwrap_or("application/octet-stream").into(),
        attrs,
    )
}

#[allow(clippy::too_many_arguments)]
async fn upload(
    app: &App,
    job: &Job,
    sender: &Account,
    source: &Account,
    media: &Media,
    path: &Path,
    size: u64,
    cancel: &CancellationToken,
) -> anyhow::Result<InputMessage> {
    let (name, mime, attrs) = document_info(media);
    let count = Arc::new(AtomicU64::new(0));
    let mut stream = CountedFile {
        file: tokio::fs::File::open(path).await?,
        count: count.clone(),
    };
    let future = sender
        .client
        .upload_stream(&mut stream, size as usize, name);
    tokio::pin!(future);
    let mut last_bytes = 0;
    let clock = super::ActiveClock::new();
    let mut last_progress = Duration::ZERO;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let uploaded = loop {
        tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("cancelled"),
            result=&mut future=>break result.map_err(|_|anyhow::anyhow!("upload_failed"))?,
            _=tick.tick()=>{
                let active_time=clock.elapsed();
                let bytes=count.load(Ordering::Relaxed);
                if bytes!=last_bytes {last_bytes=bytes;last_progress=last_progress.max(active_time);}
                anyhow::ensure!(active_time.saturating_sub(last_progress)<Duration::from_secs(app.config.limits.transfer_stall_secs),"telegram_timeout");
                app.progress(&job.summary.id,"upload",bytes,Some(size)).await?;
            }
        }
    };
    let mut input = if matches!(media, Media::Photo(_)) {
        InputMessage::new().photo(uploaded)
    } else {
        InputMessage::new().media(grammers_tl_types::types::InputMediaUploadedDocument {
            nosound_video: false,
            force_file: false,
            spoiler: false,
            file: uploaded.raw,
            thumb: None,
            mime_type: mime,
            attributes: attrs,
            stickers: None,
            ttl_seconds: None,
            video_cover: None,
            video_timestamp: None,
        })
    };
    let document = match media {
        Media::Document(d) => Some(d),
        Media::Sticker(s) => Some(&s.document),
        _ => None,
    };
    if let Some(thumb) = document.and_then(|d| {
        d.thumbs()
            .into_iter()
            .rev()
            .find(|t| t.size() > 0 && t.size() <= 1024 * 1024)
    }) {
        let temp = path.with_extension("thumb.jpg");
        let result = bounded(cancel, 60, async {
            source
                .client
                .download_media(&thumb, &temp)
                .await
                .map_err(rpc)?;
            let uploaded = sender
                .client
                .upload_file(&temp)
                .await
                .map_err(|_| anyhow::anyhow!("upload_failed"))?;
            Ok(uploaded)
        })
        .await;
        let _ = tokio::fs::remove_file(temp).await;
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        if let Ok(uploaded) = result {
            input = input.thumbnail(uploaded);
        }
    }
    Ok(input)
}

pub fn caption(job: &Job, original: &str, tag: Option<&str>) -> String {
    let options = match &job.payload {
        crate::domain::JobPayload::Transfer { options, .. } => options.clone(),
        _ => Default::default(),
    };
    let text = if options.keep_caption {
        original.to_owned()
    } else {
        String::new()
    };
    if options.tag_key
        && let Some(tag) = tag
    {
        format!(
            "🔑 {tag}{}",
            if text.is_empty() {
                String::new()
            } else {
                format!("\n{text}")
            }
        )
    } else {
        text
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn one(
    app: &App,
    job: &Job,
    account: &Account,
    sender: &Account,
    target: PeerRef,
    message: &Message,
    scope: &str,
    redo: bool,
    tag: Option<&str>,
    mode: TransferMode,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<i32>> {
    let Some(media) = message.media() else {
        return Ok(None);
    };
    let Some(id) = media_id(&media) else {
        return Ok(None);
    };
    if let Some((status, target_id)) = app.store.transfer_status(scope, &id).await? {
        anyhow::ensure!(status != "sending", "transfer_uncertain");
        if status == "done" && !redo {
            return Ok(target_id);
        }
    }
    let mut path: Option<PathBuf> = None;
    let prepare=async {
        let input=match mode {
            TransferMode::Copy=>InputMessage::new().copy_media(&media),
            TransferMode::Deep=>{
                let temp=app.config.paths.cache.join(format!("{}-{}.part",job.summary.id,app.store.vault.index("media",&id)));
                path=Some(temp.clone());
                let size=bounded(cancel,app.config.limits.download_timeout_secs,download(app,job,account,&media,&temp,cancel)).await?;
                bounded(cancel,app.config.limits.upload_timeout_secs,upload(app,job,sender,account,&media,&temp,size,cancel)).await?
            }
        };
        let caption=caption(job,message.text(),tag);
        let input=input.text(&caption).fmt_entities(if mode==TransferMode::Copy && caption==message.text() {message.fmt_entities().cloned().unwrap_or_default()} else {vec![]});
        app.store.transfer_intent(scope,&id,&job.summary.id,redo).await?;
        let result=bounded(cancel,app.config.limits.request_timeout_secs,async {sender.client.send_message(target,input).await.map_err(rpc)}).await;
        let sent=match result {
            Ok(sent)=>sent,
            Err(e)=>{
                // A flood-wait rejection is definitive: no target message was sent.
                if e.downcast_ref::<super::RetryLater>().is_some() || e.downcast_ref::<super::TelegramRejected>().is_some() {
                    let (scope,id)=(scope.to_owned(),id.clone());
                    app.store.call(move|c|{c.execute("DELETE FROM transfers WHERE scope=?1 AND media_id=?2 AND status='sending'",rusqlite::params![scope,id])?;Ok(())}).await?;
                }
                return Err(e);
            }
        };
        app.store.transfer_done(scope,&id,sent.id()).await?;
        Ok(Some(sent.id()))
    }.await;
    if let Some(path) = path {
        let _ = tokio::fs::remove_file(path).await;
    }
    prepare
}

#[allow(clippy::too_many_arguments)]
pub async fn album(
    app: &App,
    job: &Job,
    sender: &Account,
    target: PeerRef,
    messages: &[Message],
    scope: &str,
    redo: bool,
    tag: Option<&str>,
    cancel: &CancellationToken,
) -> anyhow::Result<Vec<i32>> {
    use grammers_client::media::InputMedia;
    let mut ids = vec![];
    let mut inputs = vec![];
    for message in messages {
        let media = message
            .media()
            .ok_or_else(|| anyhow::anyhow!("source_media_missing"))?;
        ids.push(media_id(&media).ok_or_else(|| anyhow::anyhow!("source_media_missing"))?);
        let text = caption(job, message.text(), tag);
        inputs.push(
            InputMedia::new()
                .copy_media(&media)
                .caption(&text)
                .fmt_entities(if text == message.text() {
                    message.fmt_entities().cloned().unwrap_or_default()
                } else {
                    vec![]
                }),
        );
    }
    app.store
        .album_intent(scope, &ids, &job.summary.id, redo)
        .await?;
    let result = bounded(cancel, app.config.limits.request_timeout_secs, async {
        sender.client.send_album(target, inputs).await.map_err(rpc)
    })
    .await;
    let sent = match result {
        Ok(sent) => sent,
        Err(error) => {
            if error.downcast_ref::<super::RetryLater>().is_some()
                || error.downcast_ref::<super::TelegramRejected>().is_some()
            {
                let (scope, ids) = (scope.to_owned(), ids.clone());
                app.store.call(move|c|{for id in ids{c.execute("DELETE FROM transfers WHERE scope=?1 AND media_id=?2 AND status='sending'",rusqlite::params![scope,id])?;}Ok(())}).await?;
            }
            return Err(error);
        }
    };
    anyhow::ensure!(
        sent.len() == ids.len() && sent.iter().all(Option::is_some),
        "transfer_uncertain"
    );
    let sent = sent
        .into_iter()
        .flatten()
        .map(|m| m.id())
        .collect::<Vec<_>>();
    app.store
        .album_done(
            scope,
            &ids.into_iter()
                .zip(sent.iter().copied())
                .collect::<Vec<_>>(),
        )
        .await?;
    Ok(sent)
}

#[allow(clippy::too_many_arguments)]
pub async fn incoming(
    app: &App,
    job: &Job,
    source_chat: i64,
    ids: &[i32],
    mode: TransferMode,
    target: &str,
    caption: Option<&str>,
    caption_message: Option<i32>,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    if mode == TransferMode::Copy {
        use teloxide::{
            prelude::*,
            types::{MessageId, Recipient},
        };
        let bot = app
            .bot
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("bot_not_configured"))?;
        let destination = target
            .parse::<i64>()
            .map(|id| Recipient::Id(ChatId(id)))
            .unwrap_or_else(|_| Recipient::ChannelUsername(target.into()));
        // One stable ledger entry for the whole Bot API album.
        let scope = app
            .store
            .vault
            .index("incoming_scope", &format!("{source_chat}|{target}|copy"));
        let media = ids.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
        if let Some((status, _)) = app.store.transfer_status(&scope, &media).await? {
            if status == "done" {
                return Ok(());
            }
            anyhow::bail!("transfer_uncertain");
        }
        app.store
            .transfer_intent(&scope, &media, &job.summary.id, false)
            .await?;
        let copied = bounded(cancel, app.config.limits.request_timeout_secs, async {
            loop {
                match bot
                    .copy_messages(
                        destination.clone(),
                        ChatId(source_chat),
                        ids.iter().copied().map(MessageId),
                    )
                    .await
                {
                    Ok(copied) => break Ok(copied),
                    Err(teloxide::RequestError::RetryAfter(seconds)) => {
                        super::wait_flood(cancel, seconds.seconds() as u64).await?;
                    }
                    Err(error) => {
                        break Err(match error {
                            teloxide::RequestError::Api(_)
                            | teloxide::RequestError::MigrateToChatId(_) => {
                                anyhow::Error::new(super::TelegramRejected)
                            }
                            _ => anyhow::anyhow!("bot_copy_failed"),
                        });
                    }
                }
            }
        })
        .await;
        let copied = match copied {
            Ok(copied) => copied,
            Err(error) => {
                if error.downcast_ref::<super::TelegramRejected>().is_some() {
                    let (scope, media) = (scope.clone(), media.clone());
                    app.store.call(move |c| {c.execute("DELETE FROM transfers WHERE scope=?1 AND media_id=?2 AND status='sending'",rusqlite::params![scope,media])?;Ok(())}).await?;
                }
                return Err(error);
            }
        };
        anyhow::ensure!(copied.len() == ids.len(), "transfer_uncertain");
        let first = copied
            .first()
            .ok_or_else(|| anyhow::anyhow!("transfer_uncertain"))?
            .0;
        app.store.transfer_done(&scope, &media, first).await?;
        let mut originals = Vec::new();
        for (source, sent) in ids.iter().zip(copied) {
            originals.push((*source, sent.0));
        }
        crate::interfaces::bot::remember_media_carrier(
            app,
            source_chat,
            target,
            &originals,
            caption.unwrap_or(""),
            caption_message,
        )
        .await?;
        app.progress(
            &job.summary.id,
            "copy",
            ids.len() as u64,
            Some(ids.len() as u64),
        )
        .await?;
        Ok(())
    } else {
        let account = app.users.account(false, cancel).await?;
        let sender = app.users.sender(cancel).await?;
        let bot = app
            .bot
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("bot_not_configured"))?;
        use teloxide::prelude::*;
        let me = bounded(cancel, app.config.limits.request_timeout_secs, async {
            bot.get_me()
                .await
                .map_err(|_| anyhow::anyhow!("telegram_failed"))
        })
        .await?;
        let source = account
            .resolve(
                me.user
                    .username
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("bot_username_missing"))?,
            )
            .await?;
        let target_peer = sender.resolve(target).await?;
        let scope = app
            .store
            .vault
            .index("incoming_scope", &format!("{source_chat}|{target}|deep"));
        let messages = bounded(cancel, app.config.limits.request_timeout_secs, async {
            account
                .client
                .get_messages_by_id(source, ids)
                .await
                .map_err(rpc)
        })
        .await?;
        let mut report = app.store.report(&job.summary.id).await?;
        for message in messages {
            let Some(message) = message else {
                report.failed_files += 1;
                continue;
            };
            let result = one(
                app,
                job,
                &account,
                &sender,
                target_peer,
                &message,
                &scope,
                false,
                None,
                mode,
                cancel,
            )
            .await;
            match result {
                Ok(Some(id)) => {
                    report.files += 1;
                    crate::interfaces::bot::remember_media(
                        app,
                        source_chat,
                        target,
                        &[(message.id(), id)],
                        message.text(),
                    )
                    .await?;
                }
                Ok(None) => report.failed_files += 1,
                Err(error) => {
                    if cancel.is_cancelled()
                        || error.downcast_ref::<super::RetryLater>().is_some()
                        || app.store.has_uncertain(&job.summary.id).await?
                    {
                        return Err(error);
                    }
                    report.failed_files += 1;
                }
            }
            app.store.save_report(&job.summary.id, &report).await?;
        }
        app.store.save_report(&job.summary.id, &report).await?;
        Ok(())
    }
}
