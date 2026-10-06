use crate::{
    application::App,
    domain::{ClaimRecord, Job, TransferMode},
    storage::Store,
    telegram::{Account, RetryLater, bounded, parser, rpc, transfer},
};
use grammers_client::message::Message;
use grammers_session::types::PeerRef;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct Collector {
    task: tokio::task::JoinHandle<anyhow::Result<u32>>,
    stop: CancellationToken,
}

#[derive(Debug)]
struct BotRateLimit(u64);
impl std::fmt::Display for BotRateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bot_rate_limited")
    }
}
impl std::error::Error for BotRateLimit {}

#[derive(Default)]
struct ClaimStats {
    files: u32,
    skipped: u32,
    resumed: u32,
    failed: u32,
    incomplete: bool,
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

struct Scan {
    messages: Vec<Message>,
    newest: i32,
    inserted: u32,
    latest: i32,
}
trait ClaimSource: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn scan(
        &self,
        store: &Store,
        job: &str,
        claim: &str,
        peer: PeerRef,
        after: i32,
        cancel: &CancellationToken,
        timeout: u64,
    ) -> impl std::future::Future<Output = anyhow::Result<Scan>> + Send;
    fn refresh(
        &self,
        peer: PeerRef,
        ids: &[i32],
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<Message>>> + Send;
    fn click(
        &self,
        peer: PeerRef,
        message: &Message,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<String>>> + Send;
}
impl ClaimSource for Account {
    async fn scan(
        &self,
        store: &Store,
        job: &str,
        claim: &str,
        peer: PeerRef,
        after: i32,
        cancel: &CancellationToken,
        timeout: u64,
    ) -> anyhow::Result<Scan> {
        let mut scan = Scan {
            messages: vec![],
            newest: after,
            inserted: 0,
            latest: after,
        };
        let mut history = self.client.iter_messages(peer);
        while let Some(message) =
            bounded(cancel, timeout, async { history.next().await.map_err(rpc) }).await?
        {
            if message.id() <= after {
                break;
            }
            scan.newest = scan.newest.max(message.id());
            if !message.outgoing() {
                scan.latest = scan.latest.max(message.id());
                if let Some(media) = message.media().and_then(|m| transfer::media_id(&m))
                    && store
                        .inbox_push_group(job, claim, message.id(), &media, message.grouped_id())
                        .await?
                {
                    scan.inserted += 1;
                }
            }
            if scan.messages.len() < 100 {
                scan.messages.push(message);
            }
        }
        scan.messages.reverse();
        Ok(scan)
    }
    async fn refresh(&self, peer: PeerRef, ids: &[i32]) -> anyhow::Result<Vec<Message>> {
        Ok(self
            .client
            .get_messages_by_id(peer, ids)
            .await
            .map_err(rpc)?
            .into_iter()
            .flatten()
            .collect())
    }
    async fn click(
        &self,
        peer: PeerRef,
        message: &Message,
        data: Vec<u8>,
    ) -> anyhow::Result<Option<String>> {
        Account::click(self, peer, message, data).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn collect(
    account: Arc<impl ClaimSource>,
    store: Store,
    job: String,
    claim: String,
    peer: grammers_session::types::PeerRef,
    after: i32,
    expected: Option<u32>,
    timeout: u64,
    request_timeout: u64,
    cancel: CancellationToken,
    mut receiver: tokio::sync::broadcast::Receiver<grammers_client::message::Message>,
) -> anyhow::Result<u32> {
    let mut deadline = Instant::now() + Duration::from_secs(timeout);
    let mut initial_batch = true;
    let mut count = 0u32;
    let mut last = Instant::now();
    let mut last_id = after;
    let mut navigation = BTreeMap::new();
    let mut clicked: HashMap<i32, String> = HashMap::new();
    let mut seen = HashMap::new();
    let mut saw_reply = false;
    let mut limited = None;
    let mut latest_reply = after;
    let mut awaiting_group: Option<(i32, Instant)> = None;
    let mut poll = tokio::time::interval(Duration::from_secs(3));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut updates_open = true;
    loop {
        if Instant::now() >= deadline && (!saw_reply || !initial_batch) {
            break;
        }
        let event = tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("cancelled"),
            _=poll.tick()=>None,
            event=receiver.recv(), if updates_open=>Some(event),
        };
        let mut messages = Vec::new();
        match event {
            Some(Ok(message)) => messages.push(message),
            None | Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                let scan = account
                    .scan(
                        &store,
                        &job,
                        &claim,
                        peer,
                        last_id,
                        &cancel,
                        request_timeout,
                    )
                    .await?;
                last_id = scan.newest;
                if scan.inserted > 0 {
                    count += scan.inserted;
                    last = Instant::now();
                    saw_reply = true;
                }
                if awaiting_group.is_some_and(|(id, _)| scan.latest > id) {
                    awaiting_group = None;
                }
                latest_reply = latest_reply.max(scan.latest);
                messages = scan.messages;
            }
            Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                updates_open = false;
            }
        }
        for message in messages {
            if message.peer_id() != peer.id || message.outgoing() || message.id() <= after {
                continue;
            }
            if awaiting_group.is_some_and(|(id, _)| message.id() > id) {
                awaiting_group = None;
            }
            latest_reply = latest_reply.max(message.id());
            let changed = seen.get(&message.id()) != Some(&message.raw);
            if changed {
                last = Instant::now();
                seen.insert(message.id(), message.raw.clone());
                saw_reply = true;
            }
            while seen.len() > 128 {
                if let Some(id) = seen.keys().min().copied() {
                    seen.remove(&id);
                }
            }
            if count == 0
                && message.media().is_none()
                && let Some(seconds) = parser::claim_rate_wait(message.text())
            {
                limited = Some(seconds);
            }
            if let Some((label, data)) =
                parser::callback_button(&message, &["全部获取", "查看下一组", "下一组"])
            {
                navigation.insert(message.id(), (message.clone(), label, data));
            } else {
                navigation.remove(&message.id());
            }
            while navigation.len() > 64 {
                if let Some(id) = navigation.keys().next().copied() {
                    navigation.remove(&id);
                }
            }
            if let Some(media) = message.media().and_then(|media| transfer::media_id(&media))
                && store
                    .inbox_push_group(&job, &claim, message.id(), &media, message.grouped_id())
                    .await?
            {
                count += 1;
                last = Instant::now();
                saw_reply = true;
            }
        }
        if saw_reply && expected.is_some_and(|expected| count >= expected) {
            break;
        }
        // Refresh navigation even if no new message was emitted: the bot can
        // edit the label while reusing exactly the same callback bytes.
        let ids = navigation.keys().copied().collect::<Vec<_>>();
        if !ids.is_empty() && last.elapsed() >= Duration::from_secs(3) {
            for message in bounded(&cancel, request_timeout, account.refresh(peer, &ids)).await? {
                if let Some((label, data)) =
                    parser::callback_button(&message, &["全部获取", "查看下一组", "下一组"])
                {
                    navigation.insert(message.id(), (message, label, data));
                } else {
                    navigation.remove(&message.id());
                }
            }
        }
        if awaiting_group.is_some_and(|(_, until)| Instant::now() >= until) {
            break;
        }
        if saw_reply && awaiting_group.is_none() && last.elapsed() >= Duration::from_secs(8) {
            if initial_batch {
                deadline = Instant::now() + Duration::from_secs(timeout);
                initial_batch = false;
            }
            if let Some((message, label, data)) = navigation
                .values()
                .rev()
                .find(|(m, label, _)| clicked.get(&m.id()) != Some(label))
                .cloned()
            {
                bounded(&cancel, request_timeout, async {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    Ok(())
                })
                .await?;
                let result = bounded(
                    &cancel,
                    request_timeout,
                    account.click(peer, &message, data.clone()),
                )
                .await;
                if let Err(error) = result {
                    if cancel.is_cancelled() || error.downcast_ref::<RetryLater>().is_some() {
                        return Err(error);
                    }
                    break;
                }
                clicked.insert(message.id(), label);
                last = Instant::now();
                awaiting_group = Some((
                    latest_reply,
                    Instant::now()
                        + deadline
                            .saturating_duration_since(Instant::now())
                            .clamp(Duration::from_secs(30), Duration::from_secs(60)),
                ));
                continue;
            }
            if count == 0
                && let Some(seconds) = limited
            {
                return Err(BotRateLimit(seconds).into());
            }
            break;
        }
    }
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
async fn once(
    app: &App,
    job: &Job,
    payload: &str,
    expected: Option<u32>,
    mode: TransferMode,
    target: &str,
    redo: bool,
    dry: bool,
    batch: Option<(&str, u32)>,
    cancel: &CancellationToken,
) -> anyhow::Result<ClaimStats> {
    let mut record = if redo {
        ClaimRecord::default()
    } else {
        app.store.claim_record(payload).await?
    };
    let account = app.users.account(false, cancel).await?;
    let sender = app.users.sender(cancel).await?;
    let timeout = app.config.limits.request_timeout_secs;
    let peer = bounded(
        cancel,
        timeout,
        account.resolve(&app.config.telegram.file_bot),
    )
    .await?;
    let destination = bounded(cancel, timeout, sender.resolve(target)).await?;
    let identity = bounded(cancel, timeout, async {
        Ok(account
            .client
            .get_me()
            .await
            .map_err(rpc)?
            .id()
            .bot_api_dialog_id_unchecked())
    })
    .await?;
    let claim = app.store.vault.index("claim", payload);
    let scope = app.store.vault.index(
        "transfer_scope",
        &serde_json::to_string(&(identity, target, mode.as_str(), payload))?,
    );
    app.store.reset_inbox(&job.summary.id, &claim).await?;
    let receiver = account.messages.subscribe();
    let sent = bounded(cancel, timeout, async {
        account
            .client
            .send_message(peer, format!("/start {payload}"))
            .await
            .map_err(rpc)
    })
    .await?;
    let stop = cancel.child_token();
    let mut collector = Collector {
        task: tokio::spawn(collect(
            account.clone(),
            app.store.clone(),
            job.summary.id.clone(),
            claim.clone(),
            peer,
            sent.id(),
            expected,
            app.config.limits.claim_timeout_secs,
            timeout,
            stop.clone(),
            receiver,
        )),
        stop,
    };
    let mut count = 0;
    let mut stats = ClaimStats::default();
    let received = (&mut collector.task).await??;
    loop {
        let ids = app.store.inbox_next(&job.summary.id, &claim).await?;
        if ids.is_empty() {
            break;
        }
        let messages = bounded(cancel, timeout, async {
            account
                .client
                .get_messages_by_id(peer, &ids)
                .await
                .map_err(rpc)
        })
        .await?;
        anyhow::ensure!(messages.iter().all(Option::is_some), "source_media_missing");
        let mut pending = vec![];
        for message in messages.into_iter().flatten() {
            let media = message
                .media()
                .and_then(|m| transfer::media_id(&m))
                .ok_or_else(|| anyhow::anyhow!("source_media_missing"))?;
            let ledger_done = !redo
                && app
                    .store
                    .transfer_status(&scope, &media)
                    .await?
                    .is_some_and(|(status, _)| status == "done");
            let resumed = !redo && (record.contains(&media) || ledger_done);
            let duplicate = app.store.media_seen(&job.summary.id, &media).await? || resumed;
            if duplicate {
                stats.skipped += 1;
                if resumed {
                    stats.resumed += 1;
                    record.add(&media);
                }
                app.store
                    .inbox_done(&job.summary.id, &claim, message.id())
                    .await?;
                count += 1;
            } else {
                pending.push(message);
            }
        }
        let album = if mode == TransferMode::Copy && pending.len() > 1 && !dry {
            Some(
                transfer::album(
                    app,
                    job,
                    &sender,
                    destination,
                    &pending,
                    &scope,
                    redo,
                    Some(payload),
                    cancel,
                )
                .await,
            )
        } else {
            None
        };
        if let Some(Err(error)) = &album
            && (cancel.is_cancelled()
                || error.downcast_ref::<RetryLater>().is_some()
                || app.store.has_uncertain(&job.summary.id).await?)
        {
            return Err(album.unwrap().unwrap_err());
        }
        for (index, message) in pending.into_iter().enumerate() {
            let media = message
                .media()
                .and_then(|m| transfer::media_id(&m))
                .unwrap();
            if !dry {
                let result = if let Some(album) = &album {
                    match album {
                        Ok(sent) => Ok(Some(sent[index])),
                        Err(_) => Err(anyhow::anyhow!("telegram_failed")),
                    }
                } else {
                    transfer::one(
                        app,
                        job,
                        &account,
                        &sender,
                        destination,
                        &message,
                        &scope,
                        redo,
                        Some(payload),
                        mode,
                        cancel,
                    )
                    .await
                };
                match result {
                    Ok(Some(sent)) => {
                        stats.files += 1;
                        let mut report = app.store.report(&job.summary.id).await?;
                        report.files += 1;
                        app.store.save_report(&job.summary.id, &report).await?;
                        record.add(&media);
                        record.status = "partial".into();
                        app.store.save_claim(payload, &record).await?;
                        app.store.mark_media_seen(&job.summary.id, &media).await?;
                        if let Some(owner) = job.reply_chat {
                            crate::interfaces::bot::remember_media(
                                app,
                                owner,
                                target,
                                &[(message.id(), sent)],
                                &transfer::caption(job, message.text(), Some(payload)),
                            )
                            .await?;
                        }
                    }
                    Ok(None) => {
                        stats.failed += 1;
                    }
                    Err(error) => {
                        if cancel.is_cancelled()
                            || error.downcast_ref::<RetryLater>().is_some()
                            || app.store.has_uncertain(&job.summary.id).await?
                        {
                            return Err(error);
                        }
                        stats.failed += 1;
                    }
                }
            } else {
                stats.files += 1;
                let mut report = app.store.report(&job.summary.id).await?;
                report.files += 1;
                app.store.save_report(&job.summary.id, &report).await?;
                app.store.mark_media_seen(&job.summary.id, &media).await?;
            }
            app.store
                .inbox_done(&job.summary.id, &claim, message.id())
                .await?;
            count += 1;
            app.progress(
                &job.summary.id,
                "claim",
                count as u64,
                expected.map(u64::from),
            )
            .await?;
        }
    }
    stats.incomplete = received == 0
        || expected.is_some_and(|n| {
            if dry {
                stats.files + stats.resumed < n
            } else {
                record.files < n
            }
        })
        || count != received
        || stats.failed > 0;
    if !dry && (stats.files > 0 || stats.resumed > 0) {
        record.failed = stats.failed;
        record.status = if !stats.incomplete { "done" } else { "partial" }.into();
        app.store
            .save_claim_for_batch(payload, &record, batch)
            .await?;
    }
    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    app: &App,
    job: &Job,
    payload: &str,
    expected: Option<u32>,
    mode: TransferMode,
    target: &str,
    redo: bool,
    dry: bool,
    batch: Option<(&str, u32)>,
    cancel: &CancellationToken,
) -> anyhow::Result<bool> {
    if !redo && app.store.claim_record(payload).await?.complete(expected) {
        let mut report = app.store.report(&job.summary.id).await?;
        report.skipped_keys += 1;
        app.store.save_report(&job.summary.id, &report).await?;
        return Ok(true);
    }
    for round in 0..=5 {
        match once(
            app, job, payload, expected, mode, target, redo, dry, batch, cancel,
        )
        .await
        {
            Err(error) if error.downcast_ref::<BotRateLimit>().is_some() && round < 5 => {
                let seconds = error
                    .downcast_ref::<BotRateLimit>()
                    .unwrap()
                    .0
                    .clamp(1, 3600);
                app.progress(&job.summary.id, "claim_waiting", 0, expected.map(u64::from))
                    .await?;
                bounded(cancel, seconds + 1, async {
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    Ok(())
                })
                .await?;
            }
            result => {
                let mut report = app.store.report(&job.summary.id).await?;
                let completed = match result {
                    Ok(stats) => {
                        report.skipped_files += stats.skipped;
                        report.failed_files += stats.failed;
                        if stats.incomplete {
                            report.failed_keys += 1;
                            if !report.warnings.iter().any(|s| s == "claim_incomplete") {
                                report.warnings.push("claim_incomplete".into());
                            }
                        }
                        !stats.incomplete
                    }
                    Err(error) => {
                        if cancel.is_cancelled()
                            || error.downcast_ref::<RetryLater>().is_some()
                            || app.store.has_uncertain(&job.summary.id).await?
                            || error.chain().any(|e| e.is::<rusqlite::Error>())
                        {
                            return Err(error);
                        }
                        report.failed_keys += 1;
                        let code = crate::telemetry::safe_error(&error).to_string();
                        if !report.warnings.contains(&code) {
                            report.warnings.push(code);
                        }
                        false
                    }
                };
                app.store.save_report(&job.summary.id, &report).await?;
                return Ok(completed);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
#[path = "claim_tests.rs"]
mod tests;
