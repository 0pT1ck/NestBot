use super::*;
use grammers_tl_types::{enums, types};
use std::{collections::VecDeque, sync::Mutex};
use tokio::time::Instant;

struct FloodReplay {
    inner: Arc<Replay>,
    waited: std::sync::atomic::AtomicBool,
}
impl ClaimSource for FloodReplay {
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
        if !self.waited.swap(true, std::sync::atomic::Ordering::Relaxed) {
            bounded(cancel, 2, crate::telegram::wait_flood(cancel, 763)).await?;
        }
        self.inner
            .scan(store, job, claim, peer, after, cancel, timeout)
            .await
    }
    async fn refresh(&self, peer: PeerRef, ids: &[i32]) -> anyhow::Result<Vec<Message>> {
        self.inner.refresh(peer, ids).await
    }
    async fn click(
        &self,
        peer: PeerRef,
        message: &Message,
        data: Vec<u8>,
    ) -> anyhow::Result<Option<String>> {
        self.inner.click(peer, message, data).await
    }
}

#[tokio::test(start_paused = true)]
async fn collector_waits_823_seconds_then_saves_media_and_ends_after_normal_quiet_window() {
    let (_dir, app) = crate::interfaces::progress::tests::fixture();
    let job = crate::interfaces::progress::tests::job(&app).await;
    let peer = message(1, None, true).peer_ref().await.unwrap().unwrap();
    let source = Arc::new(FloodReplay {
        inner: Replay::new(vec![message(1, None, true)], vec![]),
        waited: std::sync::atomic::AtomicBool::new(false),
    });
    let (tx, rx) = tokio::sync::broadcast::channel(4);
    drop(tx);
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let count = crate::application::progress::observe(
        &app,
        &job,
        &cancel,
        collect(
            source,
            app.store.clone(),
            job.summary.id.clone(),
            "synthetic-claim".into(),
            peer,
            0,
            None,
            30,
            2,
            cancel.clone(),
            rx,
        ),
    )
    .await
    .unwrap();
    assert_eq!(count.count, 1);
    assert_eq!(count.limited, None);
    assert!(start.elapsed() >= Duration::from_secs(823));
    assert!(start.elapsed() < Duration::from_secs(840));
    assert!(
        !app.store
            .inbox_next(&job.summary.id, "synthetic-claim")
            .await
            .unwrap()
            .is_empty()
    );
}

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
    let (collected, store, dir) = replay_attempt(source, expected, 300).await;
    assert_eq!(collected.limited, None);
    (collected.count, store, dir)
}

async fn replay_attempt(
    source: Arc<Replay>,
    expected: Option<u32>,
    timeout: u64,
) -> (Collected, Store, tempfile::TempDir) {
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
    let collected = task.await.unwrap().unwrap();
    (collected, store, dir)
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

fn restriction(id: i32, text: &str) -> Message {
    let mut message = message(id, None, false);
    let enums::Message::Message(raw) = &mut message.raw else {
        panic!("expected message")
    };
    raw.message = text.into();
    message
}

#[tokio::test]
async fn bot_limit_before_media_surfaces_without_waiting_for_deadline() {
    let source = Replay::new(vec![restriction(1, "请求频繁，请稍后60秒重试")], vec![]);
    let started = Instant::now();
    let (collected, store, _dir) = replay_attempt(source.clone(), Some(2), 300).await;
    assert_eq!(collected.count, 0);
    assert_eq!(collected.limited, Some(60));
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(source.clicks.lock().unwrap().is_empty());
    assert!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn bot_limit_while_awaiting_group_preserves_partial_inbox() {
    let source = Replay::new(
        vec![
            message(1, None, true),
            message(2, Some("下一组 (2/2)"), false),
        ],
        vec![(12, vec![restriction(3, "暂时限制，请稍后120秒重试")])],
    );
    let (collected, store, _dir) = replay_attempt(source.clone(), Some(2), 30).await;
    assert_eq!(collected.count, 1);
    assert_eq!(collected.limited, Some(120));
    assert_eq!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap(),
        vec![1]
    );
    assert_eq!(source.clicks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn callback_answer_limit_preserves_partial_inbox() {
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
        .push_back(Ok(Some("请求频繁，请稍后90秒重试".into())));
    let (collected, store, _dir) = replay_attempt(source.clone(), Some(2), 300).await;
    assert_eq!(collected.count, 1);
    assert_eq!(collected.limited, Some(90));
    assert_eq!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap(),
        vec![1]
    );
    assert_eq!(source.clicks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn callback_rpc_limit_preserves_partial_inbox() {
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
        .push_back(Err(RetryLater { seconds: 75 }.into()));
    let (collected, store, _dir) = replay_attempt(source, Some(2), 300).await;
    assert_eq!(collected.count, 1);
    assert_eq!(collected.limited, Some(75));
    assert_eq!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap(),
        vec![1]
    );
}

#[tokio::test]
async fn refreshed_navigation_limit_surfaces_while_awaiting_group() {
    let source = Replay::new(
        vec![
            message(1, None, true),
            message(2, Some("下一组 (2/2)"), false),
        ],
        vec![(0, vec![restriction(2, "暂时限制，请稍后45秒重试")])],
    );
    let (collected, store, _dir) = replay_attempt(source, Some(2), 30).await;
    assert_eq!(collected.count, 1);
    assert_eq!(collected.limited, Some(45));
    assert_eq!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap(),
        vec![1]
    );
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
    assert_eq!(collected.count, 1);
    assert_eq!(collected.limited, None);
    assert_eq!(
        store
            .inbox_next("synthetic-job", "synthetic-claim")
            .await
            .unwrap(),
        vec![1]
    );
}
