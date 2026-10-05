use crate::{
    application::App,
    domain::Job,
    telegram::{Account, RetryLater, bounded, parser, rpc},
};
use grammers_client::message::Message;
use grammers_session::types::PeerRef;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

trait PageHistory: Sync {
    fn poll(
        &self,
        peer: PeerRef,
        after: i32,
        previous: Option<i32>,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<Message>>> + Send;
}

impl PageHistory for Account {
    async fn poll(
        &self,
        peer: PeerRef,
        after: i32,
        previous: Option<i32>,
    ) -> anyhow::Result<Vec<Message>> {
        let mut messages = vec![];
        if let Some(id) = previous {
            messages.extend(
                self.client
                    .get_messages_by_id(peer, &[id])
                    .await
                    .map_err(rpc)?
                    .into_iter()
                    .flatten(),
            );
        }
        messages.extend(self.recent_replies(peer, after).await?);
        Ok(messages)
    }
}

fn page_ready(message: &Message, previous: Option<&Message>) -> anyhow::Result<bool> {
    let page = parser::parse_search(
        message.text(),
        &parser::line_links(
            message.text(),
            message.fmt_entities().map(Vec::as_slice).unwrap_or(&[]),
        ),
    );
    if !page.entries.is_empty() {
        return Ok(previous.is_none_or(|p| {
            p.text() != message.text() || p.fmt_entities() != message.fmt_entities()
        }));
    }
    anyhow::ensure!(
        !parser::no_search_results(message.text()),
        "search_no_results"
    );
    if let Some(seconds) = parser::rate_wait(message.text()) {
        return Err(RetryLater { seconds }.into());
    }
    Ok(false)
}

async fn wait_page(
    account: &impl PageHistory,
    receiver: &mut broadcast::Receiver<Message>,
    peer: PeerRef,
    after: i32,
    previous: Option<&Message>,
    timeout: u64,
    cancel: &CancellationToken,
) -> anyhow::Result<Message> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut next_poll = Instant::now();
    let mut updates_open = true;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "telegram_timeout");
        tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("cancelled"),
            result=tokio::time::timeout(remaining.min(Duration::from_secs(3)),async {
                if updates_open {receiver.recv().await} else {std::future::pending().await}
            })=>{
                match result {
                    Ok(Ok(message)) if message.peer_id()==peer.id && !message.outgoing() && (message.id()>after || previous.is_some_and(|p|p.id()==message.id()))=>{
                        if page_ready(&message, previous)? {return Ok(message);}
                    }
                    Ok(Err(broadcast::error::RecvError::Closed)) => updates_open=false,
                    _=>{}
                }
                if Instant::now() < next_poll { continue; }
                next_poll = Instant::now() + Duration::from_secs(3);
                // Read recent replies even when the update stream dropped a message,
                // including the initial response and edits to processing placeholders.
                let messages = bounded(cancel,remaining.as_secs().max(1).min(timeout),account.poll(peer, after, previous.map(Message::id))).await?;
                for message in messages {
                    if page_ready(&message, previous)? {return Ok(message);}
                }
            }
        }
    }
}

pub async fn run(
    app: &App,
    job: &Job,
    keyword: &str,
    pages: Option<u32>,
    sort: Option<&str>,
    resume: bool,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let account = app.users.account(false, cancel).await?;
    let timeout = app.config.limits.request_timeout_secs;
    let peer = bounded(
        cancel,
        timeout,
        account.resolve(&app.config.telegram.search_bot),
    )
    .await?;
    let cursor = if resume {
        app.store.cursor(keyword).await?
    } else {
        None
    };
    let mut receiver = account.messages.subscribe();
    let mut reply = None;
    if let Some((_, Some(message_id))) = cursor {
        reply = bounded(cancel, timeout, async {
            Ok(account
                .client
                .get_messages_by_id(peer, &[message_id])
                .await
                .map_err(rpc)?
                .into_iter()
                .flatten()
                .next())
        })
        .await?;
    }
    let reused = reply.is_some();
    if reply.is_none() {
        let sent = bounded(cancel, timeout, async {
            account
                .client
                .send_message(peer, keyword)
                .await
                .map_err(rpc)
        })
        .await?;
        reply = Some(
            wait_page(
                account.as_ref(),
                &mut receiver,
                peer,
                sent.id(),
                None,
                timeout,
                cancel,
            )
            .await?,
        );
    }
    let mut reply = reply.unwrap();
    if !resume && let Some(sort) = sort {
        let needles = match sort {
            "time" => vec!["时间"],
            "hot" => vec!["热度"],
            _ => vec!["文件数量", "文件个数"],
        };
        if let Some(data) = parser::callback(&reply, &needles) {
            bounded(cancel, timeout, account.click(peer, &reply, data)).await?;
            reply = wait_page(
                account.as_ref(),
                &mut receiver,
                peer,
                reply.id(),
                Some(&reply),
                timeout,
                cancel,
            )
            .await?;
        }
    }
    let minimum = cursor.map(|(page, _)| page + 1).unwrap_or(1);
    let mut collected = 0u32;
    let mut first = reused;
    loop {
        if !first {
            let page = parser::parse_search(
                reply.text(),
                &parser::line_links(
                    reply.text(),
                    reply.fmt_entities().map(Vec::as_slice).unwrap_or(&[]),
                ),
            );
            let current = page.page.unwrap_or(minimum + collected);
            if current >= minimum {
                anyhow::ensure!(!page.entries.is_empty(), "search_no_results");
                app.store
                    .save_page(keyword, current, Some(reply.id()), page.entries)
                    .await?;
                collected += 1;
                app.progress(
                    &job.summary.id,
                    "search",
                    collected as u64,
                    pages.map(u64::from),
                )
                .await?;
                if pages.is_some_and(|limit| collected >= limit) {
                    break;
                }
            }
        }
        first = false;
        let Some(data) = parser::callback(&reply, &["下一页"]) else {
            break;
        };
        bounded(cancel, timeout, async {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            Ok(())
        })
        .await?;
        bounded(cancel, timeout, account.click(peer, &reply, data)).await?;
        reply = wait_page(
            account.as_ref(),
            &mut receiver,
            peer,
            reply.id(),
            Some(&reply),
            timeout,
            cancel,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_client::{Client, message::InputMessage};
    use grammers_mtsender::{ConnectionParams, SenderPool};
    use grammers_session::{storages::MemorySession, types::PeerId};
    use std::sync::{Arc, Mutex};

    fn message(text: &str) -> Message {
        let SenderPool { handle, .. } = SenderPool::with_configuration(
            Arc::new(MemorySession::default()),
            1,
            ConnectionParams::default(),
        );
        let client = Client::new(handle);
        let peer = PeerRef {
            id: PeerId::from_bot_api_dialog_id(42).unwrap(),
            auth: Default::default(),
        };
        Message::from_raw_short_updates(
            &client,
            grammers_tl_types::types::UpdateShortSentMessage {
                out: false,
                id: 11,
                pts: 1,
                pts_count: 1,
                date: 1,
                media: None,
                entities: None,
                ttl_period: None,
            },
            InputMessage::new().text(text),
            peer,
        )
    }

    struct History(Mutex<Vec<Message>>);
    impl PageHistory for History {
        async fn poll(&self, _: PeerRef, _: i32, _: Option<i32>) -> anyhow::Result<Vec<Message>> {
            Ok(std::mem::take(&mut *self.0.lock().unwrap()))
        }
    }

    #[tokio::test]
    async fn processing_placeholder_does_not_resend_before_result_arrives() {
        let placeholder = message("正在搜索，请稍候……");
        let result = message("密钥：synthetic-key\n描述：synthetic\n第 1 页");
        let peer = result.peer_ref().await.unwrap().unwrap();
        let (tx, mut rx) = broadcast::channel(4);
        tx.send(placeholder).unwrap();
        tx.send(result).unwrap();
        let received = wait_page(
            &History(Mutex::new(vec![])),
            &mut rx,
            peer,
            10,
            None,
            5,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(received.text().contains("synthetic-key"));
    }

    #[tokio::test]
    async fn missing_updates_fall_back_to_history_and_detect_an_edited_page() {
        let old = message("密钥：synthetic-old\n第 1 页");
        let edited = message("密钥：synthetic-new\n第 2 页");
        let peer = edited.peer_ref().await.unwrap().unwrap();
        let (tx, mut rx) = broadcast::channel(4);
        drop(tx);
        let received = wait_page(
            &History(Mutex::new(vec![edited])),
            &mut rx,
            peer,
            old.id(),
            Some(&old),
            5,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(received.text().contains("synthetic-new"));
    }

    #[tokio::test]
    async fn empty_result_is_terminal_and_cancellation_interrupts_history_wait() {
        let empty = message("🔎 搜索词：synthetic\n🔍 未找到相关结果");
        let peer = empty.peer_ref().await.unwrap().unwrap();
        let (tx, mut rx) = broadcast::channel(4);
        drop(tx);
        let error = wait_page(
            &History(Mutex::new(vec![empty])),
            &mut rx,
            peer,
            10,
            None,
            5,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "search_no_results");
        assert!(error.downcast_ref::<RetryLater>().is_none());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = wait_page(
            &History(Mutex::new(vec![])),
            &mut rx,
            peer,
            10,
            None,
            5,
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
    }
}
