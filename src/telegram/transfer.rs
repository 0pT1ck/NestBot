use crate::{
    application::App,
    domain::{Job, TransferMode},
    telegram::{Account, bounded, rpc},
};
use grammers_client::{
    media::{Attribute, Media},
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

fn document_info(media: &Media) -> (String, String, Vec<Attribute>) {
    let doc = match media {
        Media::Document(d) => Some(d),
        Media::Sticker(s) => Some(&s.document),
        _ => None,
    };
    let Some(doc) = doc else {
        return ("photo.jpg".into(), "image/jpeg".into(), vec![]);
    };
    let mut attrs = vec![];
    if let Some(grammers_tl_types::enums::Document::Document(document)) = &doc.raw.document {
        for attr in &document.attributes {
            use grammers_tl_types::enums::DocumentAttribute as A;
            match attr {
                A::Filename(name) => attrs.push(Attribute::FileName(name.file_name.clone())),
                A::Video(video) => attrs.push(Attribute::Video {
                    round_message: video.round_message,
                    supports_streaming: video.supports_streaming,
                    duration: Duration::from_secs_f64(video.duration.max(0.0)),
                    w: video.w,
                    h: video.h,
                }),
                A::Audio(audio) if audio.voice => attrs.push(Attribute::Voice {
                    duration: Duration::from_secs(audio.duration.max(0) as u64),
                    waveform: audio.waveform.clone(),
                }),
                A::Audio(audio) => attrs.push(Attribute::Audio {
                    duration: Duration::from_secs(audio.duration.max(0) as u64),
                    title: audio.title.clone(),
                    performer: audio.performer.clone(),
                }),
                _ => {}
            }
        }
    }
    (
        doc.name().unwrap_or("file.bin").into(),
        doc.mime_type().unwrap_or("application/octet-stream").into(),
        attrs,
    )
}

async fn upload(
    app: &App,
    job: &Job,
    sender: &Account,
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
    let mut last_progress = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let uploaded = loop {
        tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("cancelled"),
            result=&mut future=>break result.map_err(|_|anyhow::anyhow!("upload_failed"))?,
            _=tick.tick()=>{
                let bytes=count.load(Ordering::Relaxed);
                if bytes!=last_bytes {last_bytes=bytes;last_progress=Instant::now();}
                anyhow::ensure!(last_progress.elapsed()<Duration::from_secs(app.config.limits.transfer_stall_secs),"telegram_timeout");
                app.progress(&job.summary.id,"upload",bytes,Some(size)).await?;
            }
        }
    };
    let mut input = if matches!(media, Media::Photo(_)) {
        InputMessage::new().photo(uploaded)
    } else {
        InputMessage::new().document(uploaded).mime_type(&mime)
    };
    for attr in attrs {
        input = input.attribute(attr);
    }
    Ok(input)
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
    let mode = match &job.payload {
        crate::domain::JobPayload::Transfer { mode, .. }
        | crate::domain::JobPayload::Incoming { mode, .. } => *mode,
        _ => TransferMode::Copy,
    };
    let mut path: Option<PathBuf> = None;
    let prepare=async {
        let input=match mode {
            TransferMode::Copy=>InputMessage::new().copy_media(&media),
            TransferMode::Deep=>{
                let temp=app.config.paths.cache.join(format!("{}-{}.part",job.summary.id,app.store.vault.index("media",&id)));
                path=Some(temp.clone());
                let size=download(app,job,account,&media,&temp,cancel).await?;
                upload(app,job,sender,&media,&temp,size,cancel).await?
            }
        }.text(message.text()).fmt_entities(message.fmt_entities().cloned().unwrap_or_default());
        app.store.transfer_intent(scope,&id,&job.summary.id,redo).await?;
        let result=bounded(cancel,app.config.limits.request_timeout_secs,async {sender.client.send_message(target,input).await.map_err(rpc)}).await;
        let sent=match result {
            Ok(sent)=>sent,
            Err(e)=>{
                // A flood-wait rejection is definitive: no target message was sent.
                if e.downcast_ref::<super::RetryLater>().is_some() {
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
pub async fn incoming(
    app: &App,
    job: &Job,
    source_chat: i64,
    ids: &[i32],
    mode: TransferMode,
    target: &str,
    caption: Option<&str>,
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
            bot.copy_messages(
                destination.clone(),
                ChatId(source_chat),
                ids.iter().copied().map(MessageId),
            )
            .await
            .map_err(|_| anyhow::anyhow!("bot_copy_failed"))
        })
        .await?;
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
        crate::interfaces::bot::remember_media(
            app,
            source_chat,
            target,
            &originals,
            caption.unwrap_or(""),
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
        anyhow::ensure!(messages.iter().all(Option::is_some), "source_media_missing");
        let mut saved = Vec::new();
        for message in messages.into_iter().flatten() {
            if let Some(id) = one(
                app,
                job,
                &account,
                &sender,
                target_peer,
                &message,
                &scope,
                false,
                cancel,
            )
            .await?
            {
                saved.push((message.id(), id));
            }
        }
        crate::interfaces::bot::remember_media(
            app,
            source_chat,
            target,
            &saved,
            caption.unwrap_or(""),
        )
        .await?;
        Ok(())
    }
}
