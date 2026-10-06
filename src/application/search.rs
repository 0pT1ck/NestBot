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

#[derive(Debug, PartialEq, Eq)]
enum PageState {
    Pending,
    Ready,
    End,
}

fn page_ready(message: &Message, previous: Option<&Message>) -> anyhow::Result<PageState> {
    let page = parser::parse_search(
        message.text(),
        &parser::line_links(
            message.text(),
            message.fmt_entities().map(Vec::as_slice).unwrap_or(&[]),
        ),
    );
    if !page.entries.is_empty() {
        if let Some(p) = previous {
            let same_content =
                p.text() == message.text() && p.fmt_entities() == message.fmt_entities();
            // Navigation can disappear without changing the final page's text.
            if same_content
                && parser::callback(p, &["下一页"]).is_some()
                && parser::callback(message, &["下一页"]).is_none()
            {
                return Ok(PageState::End);
            }
            if p.id() == message.id() && same_content && p.reply_markup() == message.reply_markup()
            {
                return Ok(PageState::Pending);
            }
        }
        return Ok(PageState::Ready);
    }
    if previous.is_some()
        && (parser::search_end(message.text()) || parser::no_search_results(message.text()))
    {
        return Ok(PageState::End);
    }
    if previous.is_some()
        && previous.is_some_and(|p| p.raw != message.raw)
        && !processing(message.text())
    {
        return Ok(PageState::Ready);
    }
    if previous.is_none() && (page.is_result() || parser::no_search_results(message.text())) {
        return Ok(PageState::Ready);
    }
    Ok(PageState::Pending)
}

fn processing(text: &str) -> bool {
    ["正在搜索", "正在处理", "正在获取", "处理中", "正在加载"]
        .iter()
        .any(|s| text.contains(s))
}

async fn wait_page(
    account: &impl PageHistory,
    receiver: &mut broadcast::Receiver<Message>,
    peer: PeerRef,
    after: i32,
    previous: Option<&Message>,
    timeout: u64,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<Message>> {
    let mut deadline = Instant::now() + Duration::from_secs(timeout);
    let mut next_poll = Instant::now();
    let mut updates_open = true;
    let mut best: Option<Message> = None;
    let mut seen = std::collections::BTreeMap::new();
    let mut last_new = Instant::now();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero()
            || (previous.is_none()
                && best.is_some()
                && last_new.elapsed() >= Duration::from_secs(35))
        {
            if previous.is_none() && best.is_some() {
                return Ok(best);
            }
            anyhow::bail!("telegram_timeout");
        }
        tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("cancelled"),
            result=tokio::time::timeout(remaining.min(Duration::from_secs(3)),async {
                if updates_open {receiver.recv().await} else {std::future::pending().await}
            })=>{
                match result {
                    Ok(Ok(message)) if message.peer_id()==peer.id && !message.outgoing() && (message.id()>after || previous.is_some_and(|p|p.id()==message.id()))=>{
                        let first_reply = seen.is_empty();
                        if observe(&mut seen, &mut best, &message) {
                            if previous.is_none() && first_reply {deadline=Instant::now()+Duration::from_secs(timeout);}
                            last_new=Instant::now();
                        }
                        match page_ready(&message, previous)? {
                            PageState::Ready => return Ok(Some(message)),
                            PageState::End => return Ok(None),
                            PageState::Pending => {},
                        }
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
                    let first_reply = seen.is_empty();
                    if observe(&mut seen, &mut best, &message) {
                        if previous.is_none() && first_reply {deadline=Instant::now()+Duration::from_secs(timeout);}
                        last_new=Instant::now();
                    }
                    match page_ready(&message, previous)? {
                        PageState::Ready => return Ok(Some(message)),
                        PageState::End => return Ok(None),
                        PageState::Pending => {},
                    }
                }
            }
        }
    }
}

fn observe(
    seen: &mut std::collections::BTreeMap<i32, Message>,
    best: &mut Option<Message>,
    message: &Message,
) -> bool {
    if seen
        .get(&message.id())
        .is_some_and(|old| old.raw == message.raw)
    {
        return false;
    }
    seen.insert(message.id(), message.clone());
    while seen.len() > 128 {
        seen.pop_first();
    }
    // Python chooses the most result-like message, rather than the latest
    // service notice. History replay must not restart the quiet window.
    let score = |m: &Message| {
        (
            !parse(m).entries.is_empty(),
            m.text().contains("密钥"),
            m.text().contains("搜索词") || m.text().contains('第'),
            m.text().chars().count(),
        )
    };
    *best = seen
        .values()
        .fold(None, |best: Option<&Message>, m| {
            if best.is_none_or(|old| score(m) > score(old)) {
                Some(m)
            } else {
                best
            }
        })
        .cloned();
    true
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
    // An expired cursor or missing next button falls back to a fresh search
    // which skips pages already present in the database.
    if reply
        .as_ref()
        .is_some_and(|m| parser::callback(m, &["下一页"]).is_none())
    {
        reply = None;
    }
    let reused = reused && reply.is_some();
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
            .await?
            .ok_or_else(|| anyhow::anyhow!("search_no_results"))?,
        );
        if reply.as_ref().is_some_and(|m| !parse(m).is_result()) {
            bounded(cancel, timeout, async {
                account
                    .client
                    .send_message(peer, "/start")
                    .await
                    .map_err(rpc)
            })
            .await?;
            bounded(cancel, timeout, async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(())
            })
            .await?;
            let sent = bounded(cancel, timeout, async {
                account
                    .client
                    .send_message(peer, keyword)
                    .await
                    .map_err(rpc)
            })
            .await?;
            reply = wait_page(
                account.as_ref(),
                &mut receiver,
                peer,
                sent.id(),
                None,
                timeout,
                cancel,
            )
            .await?;
        }
    }
    let Some(mut reply) = reply else {
        return Ok(());
    };
    if !resume && let Some(sort) = sort {
        let needles = match sort {
            "time" => vec!["时间"],
            "hot" => vec!["热度"],
            _ => vec!["文件数量", "文件个数"],
        };
        if let Some(data) = parser::callback(&reply, &needles) {
            bounded(cancel, timeout, account.click(peer, &reply, data)).await?;
            let sorted = wait_page(
                account.as_ref(),
                &mut receiver,
                peer,
                reply.id(),
                Some(&reply),
                timeout,
                cancel,
            )
            .await?
            .unwrap_or_else(|| reply.clone());
            if sorted.id() == reply.id() {
                reply = sorted;
            } else {
                app.store
                    .warning(&job.summary.id, "search_sort_notice_ignored")
                    .await?;
            }
        }
    }
    let minimum = cursor.map(|(page, _)| page + 1).unwrap_or(1);
    let mut collected = 0u32;
    let mut first = reused;
    let mut retries = 0;
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
                if page.entries.is_empty() {
                    app.store
                        .warning(&job.summary.id, "search_no_results")
                        .await?;
                    break;
                }
                let batch = app
                    .store
                    .save_page(keyword, current, Some(reply.id()), page.entries.clone())
                    .await?;
                app.store
                    .select_page(&job.summary.id, &batch, &page.entries)
                    .await?;
                collected += 1;
                let mut report = app.store.report(&job.summary.id).await?;
                report.pages = collected;
                app.store.save_report(&job.summary.id, &report).await?;
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
        let answer = match bounded(cancel, timeout, account.click(peer, &reply, data)).await {
            Ok(answer) => answer,
            Err(error) if error.downcast_ref::<RetryLater>().is_some() => return Err(error),
            Err(error) if cancel.is_cancelled() => return Err(error),
            Err(_) => {
                app.store
                    .warning(&job.summary.id, "search_page_stalled")
                    .await?;
                break;
            }
        };
        if answer.as_deref().is_some_and(parser::search_end) {
            break;
        }
        let next = if answer.as_deref().and_then(parser::rate_wait).is_some() {
            Some(reply.clone())
        } else {
            match wait_page(
                account.as_ref(),
                &mut receiver,
                peer,
                reply.id(),
                Some(&reply),
                timeout,
                cancel,
            )
            .await
            {
                Ok(next) => next,
                Err(error) if error.downcast_ref::<RetryLater>().is_some() => return Err(error),
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(_) => {
                    app.store
                        .warning(&job.summary.id, "search_page_stalled")
                        .await?;
                    break;
                }
            }
        };
        let Some(next) = next else {
            break;
        };
        if parse(&next).entries.is_empty() || next.raw == reply.raw {
            retries += 1;
            if retries > 20 {
                app.store
                    .warning(&job.summary.id, "search_retries_exhausted")
                    .await?;
                break;
            }
            app.progress(
                &job.summary.id,
                "search_waiting",
                collected as u64,
                pages.map(u64::from),
            )
            .await?;
            bounded(cancel, 61, async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(())
            })
            .await?;
            first = true;
            continue;
        }
        retries = 0;
        reply = next;
    }
    Ok(())
}

fn parse(message: &Message) -> parser::SearchPage {
    parser::parse_search(
        message.text(),
        &parser::line_links(
            message.text(),
            message.fmt_entities().map(Vec::as_slice).unwrap_or(&[]),
        ),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use grammers_client::{Client, message::InputMessage};
    use grammers_mtsender::{ConnectionParams, SenderPool};
    use grammers_session::{storages::MemorySession, types::PeerId};
    use std::sync::{Arc, Mutex};

    #[test]
    fn history_replay_does_not_reset_quiet_time_and_best_reply_matches_python() {
        let mut seen = std::collections::BTreeMap::new();
        let mut best = None;
        let mut result = message("🔎 搜索词：synthetic\n第 1 页，等待中");
        if let grammers_tl_types::enums::Message::Message(raw) = &mut result.raw {
            raw.id = 101;
        }
        let mut notice = message("处理中");
        if let grammers_tl_types::enums::Message::Message(raw) = &mut notice.raw {
            raw.id = 102;
        }
        assert!(observe(&mut seen, &mut best, &result));
        assert!(observe(&mut seen, &mut best, &notice));
        assert_eq!(best.as_ref().unwrap().id(), 101);
        assert!(!observe(&mut seen, &mut best, &result));
        assert!(!observe(&mut seen, &mut best, &notice));
        if let grammers_tl_types::enums::Message::Message(raw) = &mut result.raw {
            raw.message = "🔎 搜索词：synthetic\n密钥：synthetic-key\n第 1 页".into();
        }
        assert!(observe(&mut seen, &mut best, &result));
        assert!(best.as_ref().unwrap().text().contains("synthetic-key"));
    }

    pub(crate) fn message(text: &str) -> Message {
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

    fn next_button(message: &mut Message) {
        use grammers_tl_types::{enums, types};
        let enums::Message::Message(raw) = &mut message.raw else {
            panic!("expected message")
        };
        raw.reply_markup = Some(
            types::ReplyInlineMarkup {
                rows: vec![
                    types::KeyboardButtonRow {
                        buttons: vec![
                            types::KeyboardButtonCallback {
                                requires_password: false,
                                style: None,
                                text: "下一页 ➡️".into(),
                                data: b"next".to_vec(),
                            }
                            .into(),
                        ],
                    }
                    .into(),
                ],
            }
            .into(),
        );
    }

    #[tokio::test]
    async fn final_page_button_removal_is_detected_without_text_change() {
        let mut old = message("密钥：synthetic-last\n第 2 页");
        let final_page = old.clone();
        next_button(&mut old);
        let peer = old.peer_ref().await.unwrap().unwrap();
        let (tx, mut rx) = broadcast::channel(4);
        drop(tx);
        let received = wait_page(
            &History(Mutex::new(vec![final_page])),
            &mut rx,
            peer,
            old.id(),
            Some(&old),
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(received.is_none());
    }

    #[tokio::test]
    async fn separate_end_notice_finishes_pagination() {
        let old = message("密钥：synthetic-last\n第 2 页");
        let peer = old.peer_ref().await.unwrap().unwrap();
        let (tx, mut rx) = broadcast::channel(4);
        drop(tx);
        let received = wait_page(
            &History(Mutex::new(vec![message("已经是最后一页，没有更多结果")])),
            &mut rx,
            peer,
            old.id(),
            Some(&old),
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(received.is_none());
        assert!(parser::search_end("已是最后一页"));
        assert!(!parser::search_end("第 2 页 / 共 2 页"));
        assert!(!parser::search_end("正在加载下一页，请稍候"));
    }

    #[test]
    fn identical_content_in_a_new_message_is_a_response_but_stale_history_is_not() {
        let old = message("密钥：synthetic-key");
        assert_eq!(page_ready(&old, Some(&old)).unwrap(), PageState::Pending);
        let mut new = old.clone();
        let grammers_tl_types::enums::Message::Message(raw) = &mut new.raw else {
            panic!("expected message")
        };
        raw.id += 1;
        assert_eq!(page_ready(&new, Some(&old)).unwrap(), PageState::Ready);
    }

    #[tokio::test]
    async fn unchanged_next_page_remains_a_timeout_instead_of_false_success() {
        let mut old = message("密钥：synthetic-key\n第 2 页 / 共 2 页");
        next_button(&mut old);
        let peer = old.peer_ref().await.unwrap().unwrap();
        let (tx, mut rx) = broadcast::channel(4);
        drop(tx);
        let error = wait_page(
            &History(Mutex::new(vec![old.clone()])),
            &mut rx,
            peer,
            old.id(),
            Some(&old),
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "telegram_timeout");
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
        .unwrap()
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
        .unwrap()
        .unwrap();
        assert!(received.text().contains("synthetic-new"));
    }

    #[tokio::test]
    async fn empty_result_is_terminal_and_cancellation_interrupts_history_wait() {
        let empty = message("🔎 搜索词：synthetic\n🔍 未找到相关结果");
        let peer = empty.peer_ref().await.unwrap().unwrap();
        let (tx, mut rx) = broadcast::channel(4);
        drop(tx);
        let received = wait_page(
            &History(Mutex::new(vec![empty])),
            &mut rx,
            peer,
            10,
            None,
            5,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(parser::no_search_results(received.text()));
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
