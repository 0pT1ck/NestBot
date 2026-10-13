use super::*;
use grammers_tl_types::{enums, types};
use std::{collections::VecDeque, sync::Mutex};
fn message(id: i32, label: Option<&str>, media: bool) -> Message {
    let mut message = crate::application::search::tests::message("");
    let enums::Message::Message(raw) = &mut message.raw else {
        panic!("expected message")
    };
    raw.id = id;
    if media {
        raw.media = Some(
            types::MessageMediaPhoto {
                spoiler: false,
                live_photo: false,
                photo: Some(
                    types::Photo {
                        has_stickers: false,
                        id: id as i64,
                        access_hash: 0,
                        file_reference: vec![],
                        date: 0,
                        sizes: vec![],
                        video_sizes: None,
                        dc_id: 1,
                    }
                    .into(),
                ),
                ttl_seconds: None,
                video: None,
            }
            .into(),
        );
    }
    if let Some(label) = label {
        raw.reply_markup = Some(
            types::ReplyInlineMarkup {
                rows: vec![
                    types::KeyboardButtonRow {
                        buttons: vec![
                            types::KeyboardButtonCallback {
                                requires_password: false,
                                style: None,
                                text: label.into(),
                                data: b"same-callback".to_vec(),
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
    message
}
struct Replay {
    messages: Arc<Mutex<Vec<Message>>>,
    replies: Mutex<VecDeque<(u64, Vec<Message>)>>,
    clicks: Mutex<Vec<String>>,
    answers: tokio::sync::Mutex<VecDeque<anyhow::Result<Option<String>>>>,
}
impl Replay {
    fn new(initial: Vec<Message>, replies: Vec<(u64, Vec<Message>)>) -> Arc<Self> {
        Arc::new(Self {
            messages: Arc::new(Mutex::new(initial)),
            replies: Mutex::new(replies.into()),
            clicks: Mutex::new(vec![]),
            answers: tokio::sync::Mutex::new(VecDeque::new()),
        })
    }
}
impl ClaimSource for Replay {
    async fn scan(
        &self,
        store: &Store,
        job: &str,
        claim: &str,
        _peer: PeerRef,
        after: i32,
        _cancel: &CancellationToken,
        _timeout: u64,
    ) -> anyhow::Result<Scan> {
        let messages = self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.id() > after)
            .cloned()
            .collect::<Vec<_>>();
        let newest = messages.iter().map(Message::id).max().unwrap_or(after);
        let mut inserted = 0;
        for m in &messages {
            if let Some(media) = m.media().and_then(|m| transfer::media_id(&m))
                && store
                    .inbox_push_group(job, claim, m.id(), &media, m.grouped_id())
                    .await?
            {
                inserted += 1;
            }
        }
        Ok(Scan {
            messages,
            newest,
            inserted,
            latest: newest,
        })
    }
    async fn refresh(&self, _peer: PeerRef, ids: &[i32]) -> anyhow::Result<Vec<Message>> {
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| ids.contains(&m.id()))
            .cloned()
            .collect())
    }
    async fn click(
        &self,
        _peer: PeerRef,
        message: &Message,
        _data: Vec<u8>,
    ) -> anyhow::Result<Option<String>> {
        self.clicks.lock().unwrap().push(
            parser::callback_button(message, &["全部获取", "下一组"])
                .unwrap()
                .0,
        );
        if let Some((delay, batch)) = self.replies.lock().unwrap().pop_front() {
            let messages = self.messages.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                let mut messages = messages.lock().unwrap();
                for next in batch {
                    messages.retain(|m| m.id() != next.id());
                    messages.push(next);
                }
            });
        }
        self.answers.lock().await.pop_front().unwrap_or(Ok(None))
    }
}
async fn replay(source: Arc<Replay>, expected: Option<u32>) -> (u32, Store, tempfile::TempDir) {
    replay_attempt(source, expected, 300).await
}

async fn replay_attempt(
    source: Arc<Replay>,
    expected: Option<u32>,
    timeout: u64,
) -> (u32, Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Arc::new(
        crate::storage::vault::Vault::open(&dir.path().join("master.key"), "synthetic-password")
            .unwrap(),
    );
    let store = Store::open(&dir.path().join("test.sqlite"), vault, 1024, 5).unwrap();
    let peer = crate::application::search::tests::message("")
        .peer_ref()
        .await
        .unwrap()
        .unwrap();
    let (tx, rx) = tokio::sync::broadcast::channel(4);
    drop(tx);
    tokio::time::pause();
    let protocol_started = tokio::time::Instant::now();
    let task = tokio::spawn(collect(
        source,
        store.clone(),
        "synthetic-job".into(),
        "synthetic-claim".into(),
        peer,
        0,
        expected,
        timeout,
        90,
        CancellationToken::new(),
        rx,
    ));
    for _ in 0..400 {
        if task.is_finished() {
            break;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        // Allow SQLite's blocking pool to finish without advancing protocol time.
        std::thread::sleep(Duration::from_millis(2));
        tokio::task::yield_now().await;
    }
    assert!(task.is_finished(), "collector exceeded its total deadline");
    let collected = task.await.unwrap().unwrap();
    let elapsed = protocol_started.elapsed();
    assert!(elapsed <= Duration::from_secs(timeout + 10));
    if elapsed >= Duration::from_secs(timeout) {
        assert!(collected.incomplete, "timeout cannot mark a claim complete");
    }
    (collected.count, store, dir)
}

#[tokio::test]
async fn python_32_files_in_four_groups_includes_the_final_group() {
    let mut first = (1..=10)
        .map(|id| message(id, None, true))
        .collect::<Vec<_>>();
    first.push(message(11, Some("查看下一组 (2/4)"), false));
    let mut second = (12..=21)
        .map(|id| message(id, None, true))
        .collect::<Vec<_>>();
    second.push(message(22, Some("查看下一组 (3/4)"), false));
    let mut third = (23..=32)
        .map(|id| message(id, None, true))
        .collect::<Vec<_>>();
    third.push(message(33, Some("查看下一组 (4/4)"), false));
    let source = Replay::new(
        first,
        vec![
            (0, second),
            (0, third),
            (0, vec![message(34, None, true), message(35, None, true)]),
        ],
    );
    let (count, store, _dir) = replay(source.clone(), Some(32)).await;
    assert_eq!(count, 32);
    assert_eq!(source.clicks.lock().unwrap().len(), 3);
    assert_eq!(
        store
            .call(
                |c| Ok(c.query_row("SELECT count(*) FROM claim_inbox", [], |r| r
                    .get::<_, u32>(0))?)
            )
            .await
            .unwrap(),
        32
    );
}

#[tokio::test]
async fn python_unknown_count_stops_without_reclicking_a_stale_button() {
    let source = Replay::new(
        vec![
            message(1, None, true),
            message(2, Some("下一组 (2/3)"), false),
        ],
        vec![(0, vec![message(3, None, true)])],
    );
    let (count, _, _) = replay(source.clone(), None).await;
    assert_eq!(count, 2);
    assert_eq!(source.clicks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn python_edited_label_is_clicked_even_when_callback_bytes_are_reused() {
    let source = Replay::new(
        vec![
            message(1, None, true),
            message(2, Some("下一组 (2/3)"), false),
        ],
        vec![
            (
                0,
                vec![
                    message(3, None, true),
                    message(2, Some("下一组 (3/3)"), false),
                ],
            ),
            (0, vec![message(4, None, true)]),
        ],
    );
    let (count, _, _) = replay(source.clone(), Some(3)).await;
    assert_eq!(count, 3);
    assert_eq!(
        &*source.clicks.lock().unwrap(),
        &["下一组 (2/3)", "下一组 (3/3)"]
    );
}

#[tokio::test]
async fn a_group_delayed_more_than_quiet_window_is_still_received() {
    let source = Replay::new(
        vec![
            message(1, None, true),
            message(2, Some("下一组 (2/2)"), false),
        ],
        vec![(12, vec![message(3, None, true)])],
    );
    let (count, _, _) = replay(source, Some(2)).await;
    assert_eq!(count, 2);
}

#[tokio::test]
async fn python_all_types_selection_uses_the_group_collector() {
    let source = Replay::new(
        vec![message(1, Some("全部获取"), false)],
        vec![
            (
                0,
                vec![
                    message(2, None, true),
                    message(3, Some("下一组 (2/2)"), false),
                ],
            ),
            (0, vec![message(4, None, true)]),
        ],
    );
    let (count, _, _) = replay(source.clone(), Some(2)).await;
    assert_eq!(count, 2);
    assert_eq!(
        &*source.clicks.lock().unwrap(),
        &["全部获取", "下一组 (2/2)"]
    );
}

#[tokio::test]
async fn python_click_without_a_reply_keeps_the_received_media() {
    let source = Replay::new(
        vec![
            message(1, None, true),
            message(2, Some("下一组 (2/2)"), false),
        ],
        vec![],
    );
    let (count, _, _) = replay(source.clone(), Some(9)).await;
    assert_eq!(count, 1);
    assert_eq!(source.clicks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn nonlimit_callback_error_keeps_existing_partial_result() {
    let source = Replay::new(
        vec![
            message(1, None, true),
            message(2, Some("下一组 (2/2)"), false),
        ],
        vec![],
    );
    source
        .answers
        .lock()
        .await
        .push_back(Err(crate::telegram::TelegramRejected.into()));
    let (collected, store, _dir) = replay_attempt(source, Some(2), 300).await;
    assert_eq!(collected, 1);
    assert_eq!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap(),
        vec![1]
    );
}

#[tokio::test]
async fn processing_reply_and_callback_do_not_interrupt_the_first_key() {
    let mut hint = message(1, Some("全部获取"), false);
    let enums::Message::Message(raw) = &mut hint.raw else {
        panic!("expected message");
    };
    raw.message = "正在处理，请稍候".into();
    let source = Replay::new(vec![hint], vec![(2, vec![message(2, None, true)])]);
    source
        .answers
        .lock()
        .await
        .push_back(Ok(Some("正在获取文件，请稍后".into())));
    let (collected, store, _dir) = replay_attempt(source, Some(1), 60).await;
    assert_eq!(collected, 1);
    assert_eq!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap(),
        vec![2]
    );
}

#[tokio::test]
async fn processing_without_buttons_waits_for_delayed_first_media() {
    let source = Replay::new(vec![message(1, None, false)], vec![]);
    let messages = source.messages.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        messages.lock().unwrap().push(message(2, None, true));
    });
    let (count, _, _) = replay_attempt(source, Some(1), 60).await;
    assert_eq!(count, 1);
}

#[tokio::test]
async fn known_count_waits_past_quiet_window_for_remaining_media() {
    let source = Replay::new(vec![message(1, None, true)], vec![]);
    let messages = source.messages.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        messages.lock().unwrap().push(message(2, None, true));
    });
    let (count, _, _) = replay_attempt(source, Some(2), 60).await;
    assert_eq!(count, 2);
}

#[tokio::test]
async fn continuous_processing_updates_cannot_extend_total_deadline() {
    let source = Replay::new(vec![message(1, None, false)], vec![]);
    let messages = source.messages.clone();
    let updater = tokio::spawn(async move {
        for id in 2..100 {
            tokio::time::sleep(Duration::from_secs(3)).await;
            messages.lock().unwrap().push(message(id, None, false));
        }
    });
    let (count, _, _) = replay_attempt(source, None, 30).await;
    updater.abort();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn rate_limited_collection_preserves_inbox_for_manual_retry() {
    let source = Replay::new(
        vec![
            message(1, None, true),
            message(2, Some("下一组 (2/2)"), false),
        ],
        vec![],
    );
    source
        .answers
        .lock()
        .await
        .push_back(Err(RateLimited.into()));
    let (_dir, app) = crate::restart_tests::fixture();
    let peer = message(0, None, false).peer_ref().await.unwrap().unwrap();
    let (tx, rx) = tokio::sync::broadcast::channel(4);
    drop(tx);
    tokio::time::pause();
    let task = tokio::spawn(collect(
        source.clone(),
        app.store.clone(),
        "job".into(),
        "claim".into(),
        peer,
        0,
        Some(2),
        60,
        90,
        CancellationToken::new(),
        rx,
    ));
    for _ in 0..100 {
        if task.is_finished() {
            break;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        std::thread::sleep(Duration::from_millis(2));
        tokio::task::yield_now().await;
    }
    assert!(task.is_finished());
    assert!(task.await.unwrap().unwrap_err().is::<RateLimited>());
    assert_eq!(source.clicks.lock().unwrap().len(), 1);
    assert_eq!(pending_count(&app.store, "job", "claim").await.unwrap(), 1);
    app.store.inbox_done("job", "claim", 1).await.unwrap();
    assert_eq!(pending_count(&app.store, "job", "claim").await.unwrap(), 0);
}

#[tokio::test]
async fn timed_text_and_callback_are_not_interpreted_as_cooldowns() {
    let mut hint = message(1, Some("全部获取"), false);
    let enums::Message::Message(raw) = &mut hint.raw else {
        panic!("expected message");
    };
    raw.message = "请求频繁，请等待 120 秒后重试".into();
    let source = Replay::new(vec![hint], vec![(2, vec![message(2, None, true)])]);
    source
        .answers
        .lock()
        .await
        .push_back(Ok(Some("暂时限制，请等待 8 秒后重试".into())));
    let (count, store, _dir) = replay_attempt(source, Some(1), 60).await;
    assert_eq!(count, 1);
    assert_eq!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap(),
        vec![2]
    );
}
