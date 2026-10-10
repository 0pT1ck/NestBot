use crate::{
    application::App,
    domain::Job,
    telegram::{Account, RetryLater, bounded, parser, rpc},
};
use grammers_client::message::Message;
use grammers_session::types::PeerRef;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

trait PageHistory: Sync {
    fn poll(
        &self,
        peer: PeerRef,
        after: i32,
        previous: Option<i32>,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<Message>>> + Send;
}

trait PageSource: PageHistory {
    fn click(
        &self,
        peer: PeerRef,
        message: &Message,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<String>>> + Send;
}

impl PageSource for Account {
    async fn click(
        &self,
        peer: PeerRef,
        message: &Message,
        data: Vec<u8>,
    ) -> anyhow::Result<Option<String>> {
        Account::click(self, peer, message, data).await
    }
}

struct PageInbox {
    receiver: broadcast::Receiver<Message>,
    seen: std::collections::BTreeMap<i32, Message>,
}
impl PageInbox {
    fn new(receiver: broadcast::Receiver<Message>) -> Self {
        Self {
            receiver,
            seen: Default::default(),
        }
    }
    fn remember(&mut self, message: &Message) -> bool {
        if self.seen.get(&message.id()).is_some_and(|old| {
            old.raw == message.raw
                || old
                    .edit_date()
                    .zip(message.edit_date())
                    .is_some_and(|(old, new)| new < old)
        }) {
            return false;
        }
        self.seen.insert(message.id(), message.clone());
        while self.seen.len() > 128 {
            self.seen.pop_first();
        }
        true
    }
    fn accept(&mut self, message: &Message, mode: PageMode<'_>) -> anyhow::Result<bool> {
        let fresh = self.remember(message);
        // A page received during the retry wait remains usable even if the wait
        // timer fired before the edit-settling window finished. Old error notices
        // cannot re-enter the retry loop.
        let state = page_ready(message, mode)?;
        Ok(fresh
            || state == PageState::End
            || (state == PageState::Ready && !parse(message).entries.is_empty()))
    }
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

#[derive(Clone, Copy)]
enum PageMode<'a> {
    Initial,
    Next(&'a Message),
    Sort(&'a Message),
}
impl<'a> PageMode<'a> {
    fn previous(self) -> Option<&'a Message> {
        match self {
            Self::Initial => None,
            Self::Next(message) | Self::Sort(message) => Some(message),
        }
    }
}

fn page_ready(message: &Message, mode: PageMode<'_>) -> anyhow::Result<PageState> {
    let previous = mode.previous();
    let page = parser::parse_search(
        message.text(),
        &parser::line_links(
            message.text(),
            message.fmt_entities().map(Vec::as_slice).unwrap_or(&[]),
        ),
    );
    if !page.entries.is_empty() {
        if let Some(p) = previous {
            let old = parse(p);
            if !old.keyword.is_empty() && !page.keyword.is_empty() && old.keyword != page.keyword {
                return Ok(PageState::Pending);
            }
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
            // Queued edits/history from an earlier page cannot advance pagination.
            // Sorting intentionally replaces entries while staying on the same page.
            if matches!(mode, PageMode::Next(_))
                && old.page.zip(page.page).is_some_and(|(old, new)| new <= old)
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

fn retryable_search_error(text: &str) -> bool {
    ["错误", "失败"].iter().any(|signal| text.contains(signal))
        && ["重试", "再试"].iter().any(|signal| text.contains(signal))
}

fn settle(
    candidate: &mut Option<(Message, PageState, Instant)>,
    message: Message,
    state: PageState,
) {
    if let Some((old, _, _)) = candidate
        && (old.raw == message.raw
            || parse(old)
                .page
                .zip(parse(&message).page)
                .is_some_and(|(old, new)| new < old))
    {
        return;
    }
    *candidate = Some((message, state, Instant::now()));
}

fn resume_start(cursor: Option<(u32, Option<i32>)>, reused: bool, reply: &Message) -> u32 {
    if reused
        && let Some((saved, _)) = cursor
        && parse(reply).page.is_some_and(|current| current > saved)
    {
        // The bot may have advanced while the task was waiting or cancelled.
        // That visible page has not been committed yet and must be collected.
        return saved + 1;
    }
    let page = if reused {
        parse(reply).page.or(cursor.map(|(page, _)| page))
    } else {
        cursor.map(|(page, _)| page)
    };
    page.map(|page| page + 1).unwrap_or(1)
}

fn reused_page_saved(cursor: Option<(u32, Option<i32>)>, reply: &Message) -> bool {
    cursor
        .map(|(saved, _)| saved)
        .zip(parse(reply).page)
        .is_none_or(|(saved, current)| current <= saved)
}

async fn wait_page(
    account: &impl PageHistory,
    receiver: &mut PageInbox,
    peer: PeerRef,
    after: i32,
    mode: PageMode<'_>,
    timeout: u64,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<Message>> {
    bounded(
        cancel,
        timeout.saturating_mul(2),
        wait_page_inner(account, receiver, peer, after, mode, timeout, cancel),
    )
    .await
}

async fn wait_page_inner(
    account: &impl PageHistory,
    receiver: &mut PageInbox,
    peer: PeerRef,
    after: i32,
    mode: PageMode<'_>,
    timeout: u64,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<Message>> {
    let previous = mode.previous();
    let mut deadline = Instant::now() + Duration::from_secs(timeout);
    let mut flood_origin = crate::telegram::flood_waited();
    let mut next_poll = Instant::now();
    let mut updates_open = true;
    let mut best: Option<Message> = None;
    let mut seen = std::collections::BTreeMap::new();
    let mut last_new = Instant::now();
    let mut candidate: Option<(Message, PageState, Instant)> = None;
    let mut notice: Option<Message> = None;
    loop {
        anyhow::ensure!(!cancel.is_cancelled(), "cancelled");
        if candidate
            .as_ref()
            .is_some_and(|(_, _, changed)| changed.elapsed() >= Duration::from_millis(800))
        {
            let (message, state, _) = candidate.take().unwrap();
            return Ok((state == PageState::Ready).then_some(message));
        }
        let flood_delay = crate::telegram::flood_waited().saturating_sub(flood_origin);
        let remaining = (deadline + flood_delay).saturating_duration_since(Instant::now());
        if remaining.is_zero()
            || (previous.is_none()
                && best.is_some()
                && last_new.elapsed() >= Duration::from_secs(35))
            || (notice.is_some()
                && candidate.is_none()
                && last_new.elapsed() >= Duration::from_secs(35))
        {
            if previous.is_some() && notice.is_some() {
                return Ok(notice);
            }
            if previous.is_none() && best.is_some() {
                return Ok(best);
            }
            anyhow::bail!("telegram_timeout");
        }
        tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("cancelled"),
            result=tokio::time::timeout(remaining.min(candidate.as_ref().map(|(_,_,changed)| Duration::from_millis(800).saturating_sub(changed.elapsed())).unwrap_or(Duration::from_secs(3))),async {
                if updates_open {receiver.receiver.recv().await} else {std::future::pending().await}
            })=>{
                match result {
                    Ok(Ok(message)) if message.peer_id()==peer.id && !message.outgoing() && (message.id()>after || previous.is_some_and(|p|p.id()==message.id()))=>{
                        if !receiver.accept(&message,mode)? { continue; }
                        let first_reply = seen.is_empty();
                        if previous.is_some_and(|p| p.raw != message.raw) && parse(&message).entries.is_empty() { notice=Some(message.clone()); }
                        if observe(&mut seen, &mut best, &message) {
                            if previous.is_none() && first_reply {deadline=Instant::now()+Duration::from_secs(timeout);flood_origin=crate::telegram::flood_waited();}
                            last_new=Instant::now();
                        }
                        match page_ready(&message, mode)? {
                            PageState::Ready if previous.is_none() => return Ok(Some(message)),
                            state @ (PageState::End | PageState::Ready) => { settle(&mut candidate, message, state); },
                            PageState::Pending => {
                                if candidate.as_ref().is_some_and(|(m,_,_)| m.id()==message.id()) && processing(message.text()) {candidate=None;}
                            },
                        }
                    }
                    Ok(Err(broadcast::error::RecvError::Closed)) => updates_open=false,
                    _=>{}
                }
                if Instant::now() < next_poll { continue; }
                next_poll = Instant::now() + Duration::from_secs(3);
                // Read recent replies even when the update stream dropped a message,
                // including the initial response and edits to processing placeholders.
                // History is a read-only fallback. A transient read failure must
                // not discard a healthy update stream or the whole search.
                let messages = match bounded(cancel,remaining.as_secs().clamp(1,10),account.poll(peer, after, previous.map(Message::id))).await {
                    Ok(messages) => messages,
                    Err(error) if cancel.is_cancelled() || error.downcast_ref::<RetryLater>().is_some() => return Err(error),
                    Err(error) => {
                        tracing::warn!(event="search_history_failed",error_code=crate::telemetry::safe_error(&error));
                        continue;
                    },
                };
                for message in messages {
                    if !receiver.accept(&message,mode)? { continue; }
                    let first_reply = seen.is_empty();
                    if previous.is_some_and(|p| p.raw != message.raw) && parse(&message).entries.is_empty() { notice=Some(message.clone()); }
                    if observe(&mut seen, &mut best, &message) {
                        if previous.is_none() && first_reply {deadline=Instant::now()+Duration::from_secs(timeout);flood_origin=crate::telegram::flood_waited();}
                        last_new=Instant::now();
                    }
                    match page_ready(&message, mode)? {
                        PageState::Ready if previous.is_none() => return Ok(Some(message)),
                        state @ (PageState::End | PageState::Ready) => {
                            // Replayed history must not postpone the edit settling window.
                            settle(&mut candidate, message, state);
                        },
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
    let mut report = app.store.report(&job.summary.id).await?;
    report.pages = 0;
    report.search_retry = 0;
    report.search_retry_at = None;
    report.search_page = None;
    report.search_total_pages = None;
    report
        .warnings
        .retain(|warning| !warning.starts_with("search_"));
    app.store.save_report(&job.summary.id, &report).await?;
    let account = app.users.account(cancel).await?;
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
    let mut receiver = PageInbox::new(account.messages.subscribe());
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
                PageMode::Initial,
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
                PageMode::Initial,
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
                PageMode::Sort(&reply),
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
    // Older databases could pair the maximum saved page with a newer search's
    // earlier message. Trust the actual reused message when it has a page label.
    let minimum = resume_start(cursor, reused, &reply);
    let skip_first = reused && reused_page_saved(cursor, &reply);
    paginate(
        app,
        job,
        account.as_ref(),
        &mut receiver,
        peer,
        reply,
        Pagination {
            keyword,
            pages,
            minimum,
            reused: skip_first,
        },
        cancel,
    )
    .await
}

struct Pagination<'a> {
    keyword: &'a str,
    pages: Option<u32>,
    minimum: u32,
    reused: bool,
}

const SEARCH_RETRY_LIMIT: u32 = 20;
const SEARCH_RETRY_SECONDS: u64 = 60;

enum RetryWake {
    Retry,
    Page(Box<Message>),
    End,
}

async fn refresh_retry(
    account: &impl PageHistory,
    receiver: &mut PageInbox,
    peer: PeerRef,
    previous: &Message,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<RetryWake>> {
    let messages = match bounded(
        cancel,
        10,
        account.poll(peer, previous.id(), Some(previous.id())),
    )
    .await
    {
        Ok(messages) => messages,
        Err(error) if cancel.is_cancelled() || error.downcast_ref::<RetryLater>().is_some() => {
            return Err(error);
        }
        Err(_) => return Ok(None),
    };
    let mut candidate = None;
    for message in messages {
        if !receiver.accept(&message, PageMode::Next(previous))? {
            continue;
        }
        match page_ready(&message, PageMode::Next(previous))? {
            PageState::End => return Ok(Some(RetryWake::End)),
            PageState::Ready if !parse(&message).entries.is_empty() => {
                settle(&mut candidate, message, PageState::Ready)
            }
            _ => {}
        }
    }
    Ok(candidate.map(|(message, _, _)| RetryWake::Page(Box::new(message))))
}

async fn wait_retry(
    account: &impl PageHistory,
    receiver: &mut PageInbox,
    peer: PeerRef,
    previous: &Message,
    cancel: &CancellationToken,
) -> anyhow::Result<RetryWake> {
    let deadline = Instant::now() + Duration::from_secs(SEARCH_RETRY_SECONDS);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(RetryWake::Retry);
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("cancelled"),
            _ = tokio::time::sleep_until(deadline) => return Ok(RetryWake::Retry),
            result = wait_page(account,receiver,peer,previous.id(),PageMode::Next(previous),remaining.as_secs().max(1),cancel) => {
                match result {
                    Ok(Some(message)) if !parse(&message).entries.is_empty() => return Ok(RetryWake::Page(Box::new(message))),
                    Ok(None) => return Ok(RetryWake::End),
                    Err(error) if cancel.is_cancelled() || error.downcast_ref::<RetryLater>().is_some() => return Err(error),
                    _ => {},
                }
            }
        }
    }
}

async fn notify(
    app: &App,
    job: &Job,
    text: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    if let Some(chat) = job.reply_chat {
        bounded(cancel, 10, async {
            crate::interfaces::bot::send(app, chat, text).await;
            Ok(())
        })
        .await
        .or_else(|error| {
            if cancel.is_cancelled() {
                Err(error)
            } else {
                Ok(())
            }
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn paginate(
    app: &App,
    job: &Job,
    account: &impl PageSource,
    receiver: &mut PageInbox,
    peer: PeerRef,
    mut reply: Message,
    options: Pagination<'_>,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let timeout = app.config.limits.request_timeout_secs;
    let Pagination {
        keyword,
        pages,
        minimum,
        reused,
    } = options;
    let mut collected = 0u32;
    let mut first = reused;
    let mut retries = 0;
    if reused {
        let page = parse(&reply);
        let mut report = app.store.report(&job.summary.id).await?;
        report.search_page = page.page;
        report.search_total_pages = page.total_pages;
        app.store.save_report(&job.summary.id, &report).await?;
    }
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
                let recovered = report.search_retry > 0;
                report.pages = collected;
                report.search_page = Some(current);
                report.search_total_pages = page.total_pages;
                report.search_retry = 0;
                report.search_retry_at = None;
                app.store.save_report(&job.summary.id, &report).await?;
                if recovered {
                    notify(
                        app,
                        job,
                        &format!("搜索已恢复：已保存第 {current} 页，继续翻页。"),
                        cancel,
                    )
                    .await?;
                }
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
        if retries > 0 {
            match refresh_retry(account, receiver, peer, &reply, cancel).await? {
                Some(RetryWake::Page(next)) => {
                    reply = *next;
                    retries = 0;
                    first = false;
                    continue;
                }
                Some(RetryWake::End) => break,
                _ => {}
            }
        }
        if retries > 0 {
            let mut report = app.store.report(&job.summary.id).await?;
            report.search_retry_at = None;
            app.store.save_report(&job.summary.id, &report).await?;
            app.progress(
                &job.summary.id,
                "search_retrying",
                collected as u64,
                pages.map(u64::from),
            )
            .await?;
            notify(app,job,&format!("正在使用原消息的“下一页”按钮重试（第 {retries}/{SEARCH_RETRY_LIMIT} 轮，已保存 {collected} 页）。"),cancel).await?;
        }
        tracing::info!(event="search_next_click",job_id=%job.summary.id,message_id=reply.id(),page=parse(&reply).page,retry=retries);
        let answer = match bounded(cancel, timeout, account.click(peer, &reply, data)).await {
            Ok(answer) => answer,
            Err(error) if error.downcast_ref::<RetryLater>().is_some() => return Err(error),
            Err(error) if cancel.is_cancelled() => return Err(error),
            Err(error) => {
                // A callback acknowledgement may fail while the bot still edits
                // its result. Observe that edit before declaring pagination stuck.
                tracing::warn!(event="search_callback_failed",job_id=%job.summary.id,error_code=crate::telemetry::safe_error(&error));
                None
            }
        };
        if answer.as_deref().is_some_and(parser::search_end) {
            break;
        }
        if let Some(seconds) = answer.as_deref().and_then(parser::rate_wait) {
            crate::telegram::wait_flood(cancel, seconds).await?;
            first = true;
            continue;
        }
        let next = if answer.as_deref().is_some_and(retryable_search_error) {
            Some(reply.clone())
        } else {
            match wait_page(
                account,
                receiver,
                peer,
                reply.id(),
                PageMode::Next(&reply),
                timeout,
                cancel,
            )
            .await
            {
                Ok(next) => next,
                Err(error) if error.downcast_ref::<RetryLater>().is_some() => return Err(error),
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => {
                    tracing::warn!(event="search_page_wait_failed",job_id=%job.summary.id,error_code=crate::telemetry::safe_error(&error),page=parse(&reply).page);
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
        if let Some(seconds) = parser::rate_wait(next.text()) {
            crate::telegram::wait_flood(cancel, seconds).await?;
            first = true;
            continue;
        }
        if parse(&next).entries.is_empty() || next.raw == reply.raw {
            retries += 1;
            if retries > SEARCH_RETRY_LIMIT {
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
            let mut report = app.store.report(&job.summary.id).await?;
            report.search_retry = retries;
            report.search_retry_at = Some(crate::domain::unix_time() + SEARCH_RETRY_SECONDS as i64);
            app.store.save_report(&job.summary.id, &report).await?;
            notify(app,job,&format!("搜索 Bot 暂时无法翻页，已保存 {collected} 页。{SEARCH_RETRY_SECONDS} 秒后自动重试原消息的“下一页”（第 {retries}/{SEARCH_RETRY_LIMIT} 轮）；可用 /status 查看，/stop 停止。"),cancel).await?;
            first = true;
            match wait_retry(account, receiver, peer, &reply, cancel).await? {
                RetryWake::Page(next) => {
                    reply = *next;
                    retries = 0;
                    first = false;
                }
                RetryWake::End => break,
                RetryWake::Retry => {}
            }
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
#[path = "search_tests.rs"]
pub(crate) mod tests;
