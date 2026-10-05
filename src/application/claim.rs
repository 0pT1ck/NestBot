use crate::{
    application::App,
    domain::{Job, TransferMode},
    storage::Store,
    telegram::{Account, RetryLater, bounded, parser, rpc, transfer},
};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

struct Collector {
    task: tokio::task::JoinHandle<anyhow::Result<u32>>,
    stop: CancellationToken,
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

#[allow(clippy::too_many_arguments)]
async fn collect(
    account: Arc<Account>,
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
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut count = 0u32;
    let mut last = Instant::now();
    let mut last_id = after;
    let mut next_button = None;
    let mut clicked = VecDeque::new();
    let mut fresh_navigation = None;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let event = tokio::select! {_=cancel.cancelled()=>anyhow::bail!("cancelled"),event=tokio::time::timeout(Duration::from_secs(2),receiver.recv())=>event};
        let mut messages = Vec::new();
        match event {
            Ok(Ok(message)) => messages.push(message),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                let mut history = account.client.iter_messages(peer);
                while let Some(message) = bounded(&cancel, request_timeout, async {
                    history.next().await.map_err(rpc)
                })
                .await?
                {
                    if message.id() <= last_id {
                        break;
                    }
                    if !message.outgoing()
                        && let Some(media) = message.media().and_then(|m| transfer::media_id(&m))
                        && store.inbox_push(&job, &claim, message.id(), &media).await?
                    {
                        count += 1;
                    }
                    // Keep only a bounded recent slice for navigation/text handling.
                    // All media locators are persisted even when history is huge.
                    if messages.len() < 100 {
                        messages.push(message);
                    }
                }
                messages.reverse();
            }
            _ => {}
        }
        for message in messages {
            if message.peer_id() != peer.id || message.outgoing() || message.id() <= after {
                continue;
            }
            last = Instant::now();
            last_id = last_id.max(message.id());
            if count == 0
                && let Some(seconds) = parser::rate_wait(message.text())
            {
                return Err(RetryLater { seconds }.into());
            }
            if let Some(data) = parser::callback(&message, &["全部获取"])
                && !clicked.contains(&(message.id(), data.clone()))
            {
                bounded(
                    &cancel,
                    request_timeout,
                    account.click(peer, &message, data.clone()),
                )
                .await?;
                clicked.push_back((message.id(), data));
            }
            if let Some(data) = parser::callback(&message, &["查看下一组", "下一组"]) {
                fresh_navigation = Some(message.id());
                next_button = Some((message.clone(), data));
            }
            if let Some(media) = message.media().and_then(|media| transfer::media_id(&media))
                && store.inbox_push(&job, &claim, message.id(), &media).await?
            {
                count += 1;
            }
            while clicked.len() > 64 {
                clicked.pop_front();
            }
        }
        if expected.is_some_and(|expected| count >= expected) {
            break;
        }
        if last.elapsed() >= Duration::from_secs(3) {
            if let Some((message, data)) = next_button.take()
                && !clicked.contains(&(message.id(), data.clone()))
            {
                bounded(
                    &cancel,
                    request_timeout,
                    account.click(peer, &message, data.clone()),
                )
                .await?;
                clicked.push_back((message.id(), data));
                last = Instant::now();
            }
            if let Some(id) = fresh_navigation
                && let Some(message) = bounded(&cancel, request_timeout, async {
                    Ok(account
                        .client
                        .get_messages_by_id(peer, &[id])
                        .await
                        .map_err(rpc)?
                        .into_iter()
                        .flatten()
                        .next())
                })
                .await?
                && let Some(data) = parser::callback(&message, &["查看下一组", "下一组"])
                && !clicked.contains(&(id, data.clone()))
            {
                next_button = Some((message, data));
            }
            if next_button.is_none() && last.elapsed() >= Duration::from_secs(8) {
                break;
            }
        }
    }
    Ok(count)
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
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
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
    loop {
        let ids = app.store.inbox_pending(&job.summary.id, &claim).await?;
        if ids.is_empty() {
            if collector.task.is_finished() {
                break;
            }
            tokio::select! {_=cancel.cancelled()=>anyhow::bail!("cancelled"),_=tokio::time::sleep(Duration::from_millis(200))=>{}};
            continue;
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
        for message in messages.into_iter().flatten() {
            if !dry {
                transfer::one(
                    app,
                    job,
                    &account,
                    &sender,
                    destination,
                    &message,
                    &scope,
                    redo,
                    cancel,
                )
                .await?;
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
    let received = (&mut collector.task).await??;
    anyhow::ensure!(
        received > 0 && expected.is_none_or(|n| received >= n) && count == received,
        "claim_incomplete"
    );
    Ok(())
}
