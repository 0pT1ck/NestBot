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

async fn wait_page(
    account: &Account,
    receiver: &mut broadcast::Receiver<Message>,
    peer: PeerRef,
    after: i32,
    previous: Option<&Message>,
    timeout: u64,
    cancel: &CancellationToken,
) -> anyhow::Result<Message> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "telegram_timeout");
        tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("cancelled"),
            result=tokio::time::timeout(remaining.min(Duration::from_secs(3)),receiver.recv())=>{
                match result {
                    Ok(Ok(message)) if message.peer_id()==peer.id && !message.outgoing() && (message.id()>after || previous.is_some_and(|p|p.id()==message.id()))=>{
                        if !parser::parse_search(message.text(),&parser::line_links(message.text(),message.fmt_entities().map(Vec::as_slice).unwrap_or(&[]))).entries.is_empty()
                            && previous.is_none_or(|p|p.text()!=message.text()) {return Ok(message);}
                        if let Some(seconds)=parser::rate_wait(message.text()) {return Err(RetryLater{seconds}.into());}
                    }
                    _=>{}
                }
                // Also refetch the original message: edited placeholders and dropped
                // update buffers must not turn into lost search pages.
                if let Some(previous)=previous
                    && let Some(message)=bounded(cancel,timeout,async{Ok(account.client.get_messages_by_id(peer,&[previous.id()]).await.map_err(rpc)?.into_iter().flatten().next())}).await?
                        && message.text()!=previous.text() && !parser::parse_search(message.text(),&Default::default()).entries.is_empty() {return Ok(message);}
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
                &account,
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
                &account,
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
            &account,
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
