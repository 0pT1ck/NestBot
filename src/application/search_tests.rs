use super::*;
use grammers_client::{Client, message::InputMessage};
use grammers_mtsender::{ConnectionParams, SenderPool};
use grammers_session::{storages::MemorySession, types::PeerId};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum ErrorMode {
    New,
    Edited,
    Popup,
    TimedPopup,
    Silent,
    Forever,
    Delayed(Duration),
}
struct RetryState {
    visible: Message,
    notice: Option<Message>,
    clicks: Vec<(i32, u32, Vec<u8>)>,
}
struct RetryBot {
    mode: ErrorMode,
    state: Arc<Mutex<RetryState>>,
    tx: broadcast::Sender<Message>,
}
fn result_page(page: u32) -> Message {
    let mut result = message(&format!(
        "搜索词：synthetic\n密钥：synthetic-{page}\n第 {page} 页 / 共 125 页"
    ));
    next_button(&mut result);
    result
}
impl PageHistory for RetryBot {
    async fn poll(&self, _: PeerRef, _: i32, _: Option<i32>) -> anyhow::Result<Vec<Message>> {
        let state = self.state.lock().unwrap();
        let mut messages = vec![state.visible.clone()];
        // Re-read the old error on EVERY poll, reproducing the reported loop.
        messages.extend(state.notice.clone());
        Ok(messages)
    }
}
impl PageSource for RetryBot {
    async fn click(
        &self,
        _: PeerRef,
        previous: &Message,
        data: Vec<u8>,
    ) -> anyhow::Result<Option<String>> {
        let mut state = self.state.lock().unwrap();
        state
            .clicks
            .push((previous.id(), parse(previous).page.unwrap(), data));
        let attempt = state.clicks.len();
        if matches!(self.mode, ErrorMode::Silent) && attempt > 1 {
            return Ok(None);
        }
        if attempt == 1 || matches!(self.mode, ErrorMode::Forever) {
            if matches!(self.mode, ErrorMode::Popup) {
                return Ok(Some("❌ 发生错误，请稍后重试".into()));
            }
            if matches!(self.mode, ErrorMode::TimedPopup) {
                return Ok(Some("请求频繁，请等待 120 秒后重试".into()));
            }
            let mut error = message("❌ 发生错误，请稍后重试");
            if !matches!(self.mode, ErrorMode::Edited) {
                let grammers_tl_types::enums::Message::Message(raw) = &mut error.raw else {
                    unreachable!()
                };
                raw.id += attempt as i32;
                state.notice = Some(error.clone());
            } else {
                state.visible = error.clone();
            }
            self.tx.send(error).unwrap();
            if let ErrorMode::Delayed(delay) = self.mode {
                let state = self.state.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let page = result_page(52);
                    state.lock().unwrap().visible = page.clone();
                    let _ = tx.send(page);
                });
            }
        } else {
            let page = result_page(parse(previous).page.unwrap() + 1);
            state.visible = page.clone();
            self.tx.send(page).unwrap();
        }
        Ok(None)
    }
}

fn retry_fixture() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Arc::new(
        crate::storage::vault::Vault::open(&dir.path().join("master.key"), "synthetic-password")
            .unwrap(),
    );
    let store =
        crate::storage::Store::open(&dir.path().join("test.sqlite"), vault, 1024, 20).unwrap();
    let mut config = crate::config::Config::default();
    config.telegram.bot_token_env = "NESTBOT_SYNTHETIC_SEARCH_TEST_BOT".into();
    config.limits.request_timeout_secs = 2;
    (dir, App::new(config, store).unwrap())
}
async fn retry_job(app: &App) -> Job {
    app.store
        .enqueue(
            crate::domain::JobPayload::Search {
                keyword: "synthetic".into(),
                pages: None,
                sort: None,
                resume: false,
            },
            None,
            None,
        )
        .await
        .unwrap();
    app.store.next_job_lane(false).await.unwrap().unwrap()
}
async fn replay_retry(
    app: &App,
    job: &Job,
    mode: ErrorMode,
    pages: Option<u32>,
    cancel: &CancellationToken,
) -> (anyhow::Result<()>, Arc<Mutex<RetryState>>) {
    let old = result_page(51);
    let peer = old.peer_ref().await.unwrap().unwrap();
    let (tx, rx) = broadcast::channel(16);
    let state = Arc::new(Mutex::new(RetryState {
        visible: old.clone(),
        notice: None,
        clicks: vec![],
    }));
    let bot = RetryBot {
        mode,
        state: state.clone(),
        tx,
    };
    let mut inbox = PageInbox::new(rx);
    let result = paginate(
        app,
        job,
        &bot,
        &mut inbox,
        peer,
        old,
        Pagination {
            keyword: "synthetic",
            pages,
            minimum: 51,
            reused: false,
        },
        cancel,
    )
    .await;
    (result, state)
}

#[tokio::test(start_paused = true)]
async fn error_new_message_or_edit_retries_original_button_and_saves_pages_52_and_53() {
    for mode in [ErrorMode::New, ErrorMode::Edited] {
        let (_dir, app) = retry_fixture();
        let job = retry_job(&app).await;
        let (result, state) =
            replay_retry(&app, &job, mode, Some(3), &CancellationToken::new()).await;
        result.unwrap();
        assert_eq!(
            state
                .lock()
                .unwrap()
                .clicks
                .iter()
                .map(|(_, page, _)| *page)
                .collect::<Vec<_>>(),
            vec![51, 51, 52]
        );
        assert!(
            state
                .lock()
                .unwrap()
                .clicks
                .iter()
                .all(|(id, _, data)| *id == 11 && data == b"next")
        );
        let batch = app.store.vault.index("keyword", "synthetic");
        let entries = app.store.entries(&batch, 1, 10).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|(_, entry)| entry.page.unwrap())
                .collect::<Vec<_>>(),
            vec![51, 52, 53]
        );
        let report = app.store.report(&job.summary.id).await.unwrap();
        assert_eq!(report.pages, 3);
        assert_eq!(report.search_retry, 0);
        assert!(report.search_retry_at.is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn unchanged_old_error_does_not_trigger_twenty_more_waits_when_retry_has_no_reply() {
    let (_dir, app) = retry_fixture();
    let job = retry_job(&app).await;
    let start = Instant::now();
    let (result, state) = replay_retry(
        &app,
        &job,
        ErrorMode::Silent,
        None,
        &CancellationToken::new(),
    )
    .await;
    result.unwrap();
    assert_eq!(state.lock().unwrap().clicks.len(), 2);
    let report = app.store.report(&job.summary.id).await.unwrap();
    assert_eq!(report.pages, 1);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning == "search_page_stalled")
    );
    assert!(start.elapsed() < Duration::from_secs(90));
}

#[tokio::test(start_paused = true)]
async fn callback_error_popup_retries_the_original_button() {
    let (_dir, app) = retry_fixture();
    let job = retry_job(&app).await;
    let (result, state) = replay_retry(
        &app,
        &job,
        ErrorMode::Popup,
        Some(2),
        &CancellationToken::new(),
    )
    .await;
    result.unwrap();
    assert_eq!(state.lock().unwrap().clicks.len(), 2);
    assert_eq!(
        app.store.report(&job.summary.id).await.unwrap().search_page,
        Some(52)
    );
}

#[tokio::test(start_paused = true)]
async fn late_page_during_retry_wait_or_timer_boundary_is_saved_without_an_extra_click() {
    for delay in [Duration::from_secs(10), Duration::from_millis(60_500)] {
        let (_dir, app) = retry_fixture();
        let job = retry_job(&app).await;
        let (result, state) = replay_retry(
            &app,
            &job,
            ErrorMode::Delayed(delay),
            Some(2),
            &CancellationToken::new(),
        )
        .await;
        result.unwrap();
        assert_eq!(state.lock().unwrap().clicks.len(), 1);
        assert_eq!(
            app.store.report(&job.summary.id).await.unwrap().search_page,
            Some(52)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn twenty_new_errors_end_with_retry_limit_and_keep_the_saved_page() {
    let (_dir, app) = retry_fixture();
    let job = retry_job(&app).await;
    let (result, state) = replay_retry(
        &app,
        &job,
        ErrorMode::Forever,
        None,
        &CancellationToken::new(),
    )
    .await;
    result.unwrap();
    assert_eq!(state.lock().unwrap().clicks.len(), 21);
    let report = app.store.report(&job.summary.id).await.unwrap();
    assert_eq!(report.pages, 1);
    assert_eq!(report.search_retry, 20);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning == "search_retries_exhausted")
    );
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_retry_wait_keeps_the_committed_page() {
    let (_dir, app) = retry_fixture();
    let job = retry_job(&app).await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        trigger.cancel();
    });
    let (result, state) = replay_retry(&app, &job, ErrorMode::New, None, &cancel).await;
    assert_eq!(result.unwrap_err().to_string(), "cancelled");
    assert_eq!(state.lock().unwrap().clicks.len(), 1);
    assert_eq!(app.store.report(&job.summary.id).await.unwrap().pages, 1);
}

#[tokio::test]
async fn status_displays_search_retry_count_and_wait_and_old_reports_still_deserialize() {
    let (_dir, app) = retry_fixture();
    let job = retry_job(&app).await;
    let mut report: crate::domain::JobReport =
        serde_json::from_str(r#"{"pages":51,"warnings":[]}"#).unwrap();
    assert_eq!(report.search_retry, 0);
    report.search_retry = 3;
    report.search_retry_at = Some(crate::domain::unix_time() + 60);
    app.store
        .save_report(&job.summary.id, &report)
        .await
        .unwrap();
    app.store
        .progress(&job.summary.id, "search_waiting", 51, None)
        .await
        .unwrap();
    let status = crate::interfaces::bot::command(&app, 42, "/status", None)
        .await
        .unwrap();
    assert!(status.contains("3/20"));
    assert!(status.contains("已保存 51 页"));
    assert!(status.contains("秒后点击原消息的下一页"));
}

#[tokio::test]
async fn retry_notification_reaches_bot_api_using_only_a_local_mock_server() {
    use axum::{Json, Router};
    let messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = messages.clone();
    let router=Router::new().fallback(move|Json(body):Json<serde_json::Value>| {
        let captured=captured.clone();
        async move {
            captured.lock().unwrap().push(body["text"].as_str().unwrap().to_owned());
            Json(serde_json::json!({"ok":true,"result":{"message_id":1,"date":0,"chat":{"id":42,"type":"private","first_name":"Synthetic"},"text":body["text"]}}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, mut app) = retry_fixture();
    Arc::get_mut(&mut app).unwrap().bot = Some(
        teloxide::Bot::new("000000:synthetic")
            .set_api_url(format!("http://{address}").parse().unwrap()),
    );
    let mut job = retry_job(&app).await;
    job.reply_chat = Some(42);
    notify(
        &app,
        &job,
        "搜索 Bot 暂时无法翻页，60 秒后重试（第 1/20 轮）",
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(messages.lock().unwrap().len(), 1);
    assert!(messages.lock().unwrap()[0].contains("60 秒后重试"));
    server.abort();
}

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
    let (tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let (tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let (tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let (tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let (tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let (tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let pending = message("密钥：synthetic-pending\n第 52 页");
    assert_eq!(
        resume_start(Some((51, Some(pending.id()))), true, &pending),
        52
    );
    assert!(!reused_page_saved(Some((51, Some(pending.id()))), &pending));
}

#[tokio::test(start_paused = true)]
async fn resume_saves_visible_page_52_when_only_51_was_committed_without_clicking_ahead() {
    let (_dir, app) = retry_fixture();
    let job = retry_job(&app).await;
    let pending = result_page(52);
    let peer = pending.peer_ref().await.unwrap().unwrap();
    let (tx, rx) = broadcast::channel(4);
    let state = Arc::new(Mutex::new(RetryState {
        visible: pending.clone(),
        notice: None,
        clicks: vec![],
    }));
    let source = RetryBot {
        mode: ErrorMode::New,
        state: state.clone(),
        tx,
    };
    let cursor = Some((51, Some(pending.id())));
    let options = Pagination {
        keyword: "synthetic",
        pages: Some(1),
        minimum: resume_start(cursor, true, &pending),
        reused: reused_page_saved(cursor, &pending),
    };
    paginate(
        &app,
        &job,
        &source,
        &mut PageInbox::new(rx),
        peer,
        pending,
        options,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(state.lock().unwrap().clicks.is_empty());
    assert_eq!(app.store.cursor("synthetic").await.unwrap().unwrap().0, 52);
    assert_eq!(app.store.report(&job.summary.id).await.unwrap().pages, 1);
}

#[tokio::test(start_paused = true)]
async fn next_page_merges_successive_edits_and_ignores_a_late_old_page() {
    let mut old = message("密钥：synthetic-old\n第 1 页");
    next_button(&mut old);
    let peer = old.peer_ref().await.unwrap().unwrap();
    let partial = message("密钥：synthetic-partial\n第 2 页");
    let mut full = message("密钥：synthetic-partial\n密钥：synthetic-full\n第 2 页");
    next_button(&mut full);
    let (tx, rx) = broadcast::channel(8);
    let mut rx = PageInbox::new(rx);
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
    let (tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let (_tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let (_tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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
    let (_tx, rx) = broadcast::channel(4);
    let mut rx = PageInbox::new(rx);
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

#[tokio::test(start_paused = true)]
async fn timed_callback_restriction_uses_returned_time_instead_of_generic_search_delay() {
    let (_dir, app) = retry_fixture();
    let job = retry_job(&app).await;
    let started = Instant::now();
    let (result, _) = replay_retry(
        &app,
        &job,
        ErrorMode::TimedPopup,
        Some(2),
        &CancellationToken::new(),
    )
    .await;
    result.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(180));
    assert!(started.elapsed() < Duration::from_secs(190));
    assert_eq!(
        app.store.report(&job.summary.id).await.unwrap().search_page,
        Some(52)
    );
}
