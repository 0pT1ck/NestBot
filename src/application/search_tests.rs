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
        PageMode::Next(&old),
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
        PageMode::Next(&old),
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
    assert_eq!(
        page_ready(&old, PageMode::Next(&old)).unwrap(),
        PageState::Pending
    );
    let mut new = old.clone();
    let grammers_tl_types::enums::Message::Message(raw) = &mut new.raw else {
        panic!("expected message")
    };
    raw.id += 1;
    assert_eq!(
        page_ready(&new, PageMode::Next(&old)).unwrap(),
        PageState::Ready
    );
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
        PageMode::Next(&old),
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
        PageMode::Initial,
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
        PageMode::Next(&old),
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
        PageMode::Initial,
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
        PageMode::Initial,
        5,
        &cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
}

#[test]
fn resume_uses_reused_page_6_even_when_old_database_cursor_says_12() {
    let reply = message("密钥：synthetic-six\n第 6 页");
    assert_eq!(resume_start(Some((12, Some(reply.id()))), true, &reply), 7);
    assert_eq!(resume_start(Some((12, None)), false, &reply), 13);
}

#[tokio::test(start_paused = true)]
async fn next_page_merges_successive_edits_and_ignores_a_late_old_page() {
    let mut old = message("密钥：synthetic-old\n第 1 页");
    next_button(&mut old);
    let peer = old.peer_ref().await.unwrap().unwrap();
    let partial = message("密钥：synthetic-partial\n第 2 页");
    let mut full = message("密钥：synthetic-partial\n密钥：synthetic-full\n第 2 页");
    next_button(&mut full);
    let (tx, mut rx) = broadcast::channel(8);
    tx.send(partial).unwrap();
    let late = old.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        tx.send(full).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(late).unwrap();
    });
    let received = wait_page(
        &History(Mutex::new(vec![old.clone()])),
        &mut rx,
        peer,
        old.id(),
        PageMode::Next(&old),
        10,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(parse(&received).entries.len(), 2);
    assert!(parser::callback(&received, &["下一页"]).is_some());
}

#[tokio::test(start_paused = true)]
async fn temporarily_removed_buttons_do_not_end_before_next_edit() {
    let mut old = message("密钥：synthetic-first\n第 1 页");
    next_button(&mut old);
    let peer = old.peer_ref().await.unwrap().unwrap();
    let temporary = message(old.text());
    let mut next = message("密钥：synthetic-next\n第 2 页");
    next_button(&mut next);
    let (tx, mut rx) = broadcast::channel(4);
    tx.send(temporary).unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        tx.send(next).unwrap();
    });
    let received = wait_page(
        &History(Mutex::new(vec![])),
        &mut rx,
        peer,
        old.id(),
        PageMode::Next(&old),
        5,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(parse(&received).page, Some(2));
}

struct FlakyHistory(std::sync::atomic::AtomicUsize, Message);
impl PageHistory for FlakyHistory {
    async fn poll(&self, _: PeerRef, _: i32, _: Option<i32>) -> anyhow::Result<Vec<Message>> {
        if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            anyhow::bail!("telegram_failed");
        }
        Ok(vec![self.1.clone()])
    }
}

#[tokio::test(start_paused = true)]
async fn transient_history_read_failure_keeps_waiting_for_next_page() {
    let old = message("密钥：synthetic-old\n第 12 页");
    let peer = old.peer_ref().await.unwrap().unwrap();
    let history = FlakyHistory(
        std::sync::atomic::AtomicUsize::new(0),
        message("密钥：synthetic-next\n第 13 页"),
    );
    let (_tx, mut rx) = broadcast::channel(4);
    let received = wait_page(
        &history,
        &mut rx,
        peer,
        old.id(),
        PageMode::Next(&old),
        20,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(parse(&received).page, Some(13));
    assert!(history.0.load(std::sync::atomic::Ordering::SeqCst) >= 2);
}

#[tokio::test(start_paused = true)]
async fn processing_edit_without_results_returns_notice_for_local_page_retry() {
    let old = message("密钥：synthetic-old\n第 6 页");
    let peer = old.peer_ref().await.unwrap().unwrap();
    let (_tx, mut rx) = broadcast::channel(4);
    let received = wait_page(
        &History(Mutex::new(vec![message("正在搜索下一页，请稍候")])),
        &mut rx,
        peer,
        old.id(),
        PageMode::Next(&old),
        5,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(parse(&received).entries.is_empty());
    assert!(processing(received.text()));
}

#[tokio::test(start_paused = true)]
async fn sorting_allows_replacing_entries_on_the_same_page() {
    let old = message("密钥：synthetic-old\n第 1 页");
    let peer = old.peer_ref().await.unwrap().unwrap();
    let sorted = message("密钥：synthetic-sorted\n第 1 页");
    assert_eq!(
        page_ready(&sorted, PageMode::Next(&old)).unwrap(),
        PageState::Pending
    );
    let (_tx, mut rx) = broadcast::channel(4);
    let received = wait_page(
        &History(Mutex::new(vec![sorted])),
        &mut rx,
        peer,
        old.id(),
        PageMode::Sort(&old),
        5,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(received.text().contains("synthetic-sorted"));
}
