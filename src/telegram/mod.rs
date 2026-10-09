pub mod parser;
pub mod session;
pub mod transfer;

use crate::{config::Config, storage::Store};
use grammers_client::{
    Client,
    client::{ClientConfiguration, RetryContext, RetryPolicy, UpdatesConfiguration},
    message::Message,
    update::Update,
};
use grammers_mtsender::{ConnectionParams, InvocationError, SenderPool};
use grammers_session::{
    Session,
    types::{PeerId, PeerRef, UpdateState, UpdatesState},
};
use std::{ops::ControlFlow, sync::Arc, time::Duration};
use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct RetryLater {
    pub seconds: u64,
}
impl std::fmt::Display for RetryLater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("telegram_rate_limited")
    }
}
impl std::error::Error for RetryLater {}

tokio::task_local! {
    // Nested transfer and RPC deadlines must both exclude server-requested waits.
    static FLOOD_BUDGETS: Vec<watch::Sender<Duration>>;
    static FLOOD_EVENTS: watch::Sender<Option<FloodNotice>>;
    static EXTRACTION_RPC: bool;
}

#[derive(Clone, Copy)]
pub(crate) struct FloodNotice {
    pub wait: crate::domain::JobWait,
    pub until: tokio::time::Instant,
}

pub(crate) fn flood_delay(seconds: u64) -> Duration {
    flood_delay_for(seconds, Duration::from_secs(seconds.saturating_add(60)))
}

fn flood_delay_for(seconds: u64, delay: Duration) -> Duration {
    let _ = FLOOD_BUDGETS.try_with(|budgets| {
        for budget in budgets {
            budget.send_modify(|total| *total += delay);
        }
    });
    let _ = FLOOD_EVENTS.try_with(|events| {
        let notice = FloodNotice {
            wait: crate::domain::JobWait {
                seconds,
                retry_at: crate::domain::unix_time().saturating_add(delay.as_secs() as i64),
            },
            until: tokio::time::Instant::now() + delay,
        };
        events.send_if_modified(|current| {
            if current.is_none_or(|old| old.until <= notice.until) {
                *current = Some(notice);
                true
            } else {
                false
            }
        });
    });
    delay
}

pub(crate) async fn wait_flood(cancel: &CancellationToken, seconds: u64) -> anyhow::Result<()> {
    wait_flood_for(
        cancel,
        seconds,
        Duration::from_secs(seconds.saturating_add(60)),
    )
    .await
}

async fn wait_flood_for(
    cancel: &CancellationToken,
    seconds: u64,
    delay: Duration,
) -> anyhow::Result<()> {
    let delay = flood_delay_for(seconds, delay);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => anyhow::bail!("cancelled"),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

pub(crate) async fn flood_scope<T>(
    events: watch::Sender<Option<FloodNotice>>,
    future: impl std::future::Future<Output = T>,
) -> T {
    FLOOD_EVENTS.scope(events, future).await
}

pub(crate) async fn extraction_scope<T>(future: impl std::future::Future<Output = T>) -> T {
    EXTRACTION_RPC.scope(true, future).await
}

// Tokio tasks do not inherit task locals. The media collector must share the
// extraction retry policy, report waits, and extend enclosing deadlines.
pub(crate) fn inherit_flood_context<T>(
    future: impl std::future::Future<Output = T>,
) -> impl std::future::Future<Output = T> {
    let budgets = FLOOD_BUDGETS.try_with(Clone::clone).unwrap_or_default();
    let events = FLOOD_EVENTS.try_with(Clone::clone).ok();
    let extraction = EXTRACTION_RPC.try_with(|active| *active).unwrap_or(false);
    async move {
        EXTRACTION_RPC
            .scope(
                extraction,
                FLOOD_BUDGETS.scope(budgets, async move {
                    if let Some(events) = events {
                        flood_scope(events, future).await
                    } else {
                        future.await
                    }
                }),
            )
            .await
    }
}

pub(crate) fn flood_waited() -> Duration {
    FLOOD_BUDGETS
        .try_with(|budgets| {
            budgets
                .last()
                .map(|budget| *budget.borrow())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

pub(crate) struct ActiveClock {
    start: tokio::time::Instant,
    flood_origin: Duration,
}

impl ActiveClock {
    pub(crate) fn new() -> Self {
        Self {
            start: tokio::time::Instant::now(),
            flood_origin: flood_waited(),
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.start
            .elapsed()
            .saturating_sub(flood_waited().saturating_sub(self.flood_origin))
    }
}

struct TelegramFloodSleep;
impl RetryPolicy for TelegramFloodSleep {
    fn should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration> {
        let InvocationError::Rpc(error) = &ctx.error else {
            return ControlFlow::Break(());
        };
        let seconds = error.value.unwrap_or(60).max(1) as u64;
        // Telegram definitively rejected this RPC. Grammers retries its original
        // serialized request, preserving random IDs; ambiguous failures never retry.
        if error.code != 420 || EXTRACTION_RPC.try_with(|active| *active).unwrap_or(false) {
            return ControlFlow::Break(());
        }
        let delay = flood_delay(seconds);
        tracing::warn!(
            event = "telegram_flood_wait",
            seconds,
            attempt = ctx.fail_count.get()
        );
        ControlFlow::Continue(delay)
    }
}

#[derive(Debug)]
pub struct TelegramRejected;
impl std::fmt::Display for TelegramRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("telegram_failed")
    }
}
impl std::error::Error for TelegramRejected {}

pub fn rpc(error: InvocationError) -> anyhow::Error {
    match error {
        InvocationError::Rpc(ref e) if e.code == 420 => RetryLater {
            seconds: e.value.unwrap_or(60).max(1) as u64,
        }
        .into(),
        InvocationError::Rpc(ref e) if (400..500).contains(&e.code) => TelegramRejected.into(),
        _ => anyhow::anyhow!("telegram_failed"),
    }
}

pub async fn bounded<T>(
    cancel: &CancellationToken,
    seconds: u64,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let (budget, mut waits) = watch::channel(Duration::ZERO);
    let mut budgets = FLOOD_BUDGETS.try_with(Clone::clone).unwrap_or_default();
    budgets.push(budget);
    let start = tokio::time::Instant::now();
    FLOOD_BUDGETS
        .scope(budgets, async {
            tokio::pin!(future);
            loop {
                let deadline = start + Duration::from_secs(seconds) + *waits.borrow_and_update();
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => anyhow::bail!("cancelled"),
                    _ = waits.changed() => {},
                    result = &mut future => return result,
                    _ = tokio::time::sleep_until(deadline) => anyhow::bail!("telegram_timeout"),
                }
            }
        })
        .await
}

pub struct Account {
    pub client: Client,
    pub messages: broadcast::Sender<Message>,
    session: Arc<session::EncryptedSession>,
    pool_task: tokio::task::JoinHandle<()>,
    update_task: tokio::task::JoinHandle<()>,
}

struct ConnectionGuard {
    client: Client,
    abort: tokio::task::AbortHandle,
    armed: bool,
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.client.disconnect();
            self.abort.abort();
        }
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        self.client.disconnect();
        self.update_task.abort();
        self.pool_task.abort();
    }
}

impl Account {
    pub async fn connect(config: &Config, store: Store, role: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(config.telegram.api_id > 0, "missing_api_credentials");
        let session = session::EncryptedSession::open(store, role).await?;
        let proxy = Config::env_secret(&config.telegram.proxy_env).map(|v| v.to_string());
        anyhow::ensure!(
            proxy.as_ref().is_none_or(|p| p.starts_with("socks5://")),
            "mtproto_requires_socks5_proxy"
        );
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::with_configuration(
            session.clone(),
            config.telegram.api_id,
            ConnectionParams {
                proxy_url: proxy,
                ..Default::default()
            },
        );
        let client = Client::with_configuration(
            handle,
            ClientConfiguration {
                retry_policy: Box::new(TelegramFloodSleep),
                auto_cache_peers: true,
            },
        );
        let pool_task = tokio::spawn(async move {
            let _ = runner.run().await;
        });
        let mut guard = ConnectionGuard {
            client: client.clone(),
            abort: pool_task.abort_handle(),
            armed: true,
        };
        // Initialize before returning the account. Otherwise the stream's first
        // GetState can race a fast bot reply and mark that reply as already seen.
        let initialized = match client
            .invoke(&grammers_tl_types::functions::updates::GetState {})
            .await
        {
            Ok(grammers_tl_types::enums::updates::State::State(state)) => {
                if let Err(error) = session
                    .set_update_state(UpdateState::All(UpdatesState {
                        pts: state.pts,
                        qts: state.qts,
                        date: state.date,
                        seq: state.seq,
                        channels: vec![],
                    }))
                    .await
                {
                    client.disconnect();
                    pool_task.abort();
                    return Err(error.into());
                }
                true
            }
            Err(InvocationError::Rpc(error)) if error.code == 401 => false,
            Err(error) => {
                client.disconnect();
                pool_task.abort();
                return Err(rpc(error));
            }
        };
        let stream = client
            .stream_updates(
                updates,
                UpdatesConfiguration {
                    catch_up: initialized,
                    update_queue_limit: Some(64),
                },
            )
            .await
            .map_err(|_| anyhow::anyhow!("telegram_failed"));
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                client.disconnect();
                pool_task.abort();
                return Err(e);
            }
        };
        let (messages, _) = broadcast::channel(128);
        let tx = messages.clone();
        let update_task = tokio::spawn(async move {
            loop {
                match stream.next().await {
                    Ok(Update::NewMessage(message) | Update::MessageEdited(message)) => {
                        let _ = tx.send(message.into_inner());
                    }
                    Ok(_) => {}
                    Err(_) => {
                        tracing::warn!(event = "mtproto_update_failed");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
        guard.armed = false;
        Ok(Self {
            client,
            messages,
            session,
            pool_task,
            update_task,
        })
    }

    pub async fn recent_replies(&self, peer: PeerRef, after: i32) -> anyhow::Result<Vec<Message>> {
        let mut history = self.client.iter_messages(peer).limit(32);
        let mut messages = vec![];
        while let Some(message) = history.next().await.map_err(rpc)? {
            if message.id() <= after {
                break;
            }
            if !message.outgoing() {
                messages.push(message);
            }
        }
        messages.reverse();
        Ok(messages)
    }

    pub async fn resolve(&self, ident: &str) -> anyhow::Result<PeerRef> {
        if let Ok(id) = ident.parse::<i64>() {
            let id = PeerId::from_bot_api_dialog_id(id)
                .ok_or_else(|| anyhow::anyhow!("invalid_target"))?;
            if let Some(peer) = self.session.peer_ref(id).await? {
                return Ok(peer);
            }
            // Obtain missing access hashes from this account's dialogs.
            let mut dialogs = self.client.iter_dialogs();
            while let Some(dialog) = dialogs.next().await.map_err(rpc)? {
                if dialog.peer().id() == id {
                    return dialog
                        .peer()
                        .to_ref()
                        .await
                        .map_err(|_| anyhow::anyhow!("telegram_failed"))?
                        .ok_or_else(|| anyhow::anyhow!("target_not_found"));
                }
            }
            anyhow::bail!("target_not_found");
        }
        let peer = self
            .client
            .resolve_username(ident.trim_start_matches('@'))
            .await
            .map_err(rpc)?
            .ok_or_else(|| anyhow::anyhow!("target_not_found"))?;
        peer.to_ref()
            .await
            .map_err(|_| anyhow::anyhow!("telegram_failed"))?
            .ok_or_else(|| anyhow::anyhow!("target_not_found"))
    }

    pub async fn click(
        &self,
        peer: PeerRef,
        message: &Message,
        data: Vec<u8>,
    ) -> anyhow::Result<Option<String>> {
        let answer = self
            .client
            .invoke(
                &grammers_tl_types::functions::messages::GetBotCallbackAnswer {
                    game: false,
                    peer: peer.into(),
                    msg_id: message.id(),
                    data: Some(data),
                    password: None,
                },
            )
            .await;
        // Some bots edit the result but never acknowledge the callback. The
        // caller must still read the resulting message before deciding it failed.
        let answer = match answer {
            Ok(answer) => answer,
            Err(InvocationError::Rpc(ref error)) if error.name == "BOT_RESPONSE_TIMEOUT" => {
                return Ok(None);
            }
            Err(error) => return Err(rpc(error)),
        };
        let grammers_tl_types::enums::messages::BotCallbackAnswer::Answer(answer) = answer;
        Ok(answer.message)
    }
}

// Extraction alone shares a sticky choice and per-account cooldowns. Sender
// selection and ordinary account lookup never consult this state.
#[derive(Default)]
struct ExtractionAccounts {
    upload: bool,
    cooling: [Option<tokio::time::Instant>; 2],
}

impl ExtractionAccounts {
    fn choose(
        &mut self,
        has_upload: bool,
        now: tokio::time::Instant,
    ) -> (bool, Option<tokio::time::Instant>) {
        if !has_upload {
            self.upload = false;
        }
        let preferred = usize::from(self.upload);
        if self.cooling[preferred].is_none_or(|until| until <= now) {
            return (self.upload, None);
        }
        (self.upload, self.cooling[usize::from(self.upload)])
    }

    fn limited(&mut self, upload: bool, seconds: u64) {
        let until = tokio::time::Instant::now() + Duration::from_secs(seconds.saturating_add(60));
        let cooling = &mut self.cooling[usize::from(upload)];
        *cooling = Some(cooling.map_or(until, |old| old.max(until)));
        self.upload = !upload;
    }
}

pub struct Users {
    config: Arc<Config>,
    store: Store,
    main: Mutex<Option<Arc<Account>>>,
    upload: Mutex<Option<Arc<Account>>>,
    extraction: Mutex<ExtractionAccounts>,
}

impl Users {
    pub fn new(config: Arc<Config>, store: Store) -> Self {
        Self {
            config,
            store,
            main: Mutex::new(None),
            upload: Mutex::new(None),
            extraction: Mutex::new(ExtractionAccounts::default()),
        }
    }

    pub async fn account(
        &self,
        upload: bool,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Arc<Account>> {
        let slot = if upload { &self.upload } else { &self.main };
        let mut slot = slot.lock().await;
        if let Some(account) = slot.as_ref() {
            return Ok(account.clone());
        }
        let account = bounded(cancel, self.config.limits.request_timeout_secs, async {
            let account = Account::connect(
                &self.config,
                self.store.clone(),
                if upload { "upload" } else { "main" },
            )
            .await?;
            anyhow::ensure!(
                account.client.is_authorized().await.map_err(rpc)?,
                "main_account_not_logged_in"
            );
            Ok(Arc::new(account))
        })
        .await?;
        *slot = Some(account.clone());
        Ok(account)
    }

    /// Select the extraction source, preserving it until it is rate limited.
    pub(crate) async fn extraction_choice(
        &self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<bool> {
        loop {
            anyhow::ensure!(!cancel.is_cancelled(), "cancelled");
            let has_upload = self
                .store
                .preference("session:upload:self")
                .await?
                .is_some();
            let (upload, until) = self
                .extraction
                .lock()
                .await
                .choose(has_upload, tokio::time::Instant::now());
            let Some(until) = until else {
                return Ok(upload);
            };
            let delay = until.saturating_duration_since(tokio::time::Instant::now());
            // The safety minute was added when marking the account. Report and
            // wait only its remaining cooldown, never add that minute twice.
            wait_flood_for(cancel, delay.as_secs().saturating_sub(60), delay).await?;
        }
    }

    /// Return the selected source without falling back on connection errors.
    pub async fn extraction_account(
        &self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(bool, Arc<Account>)> {
        loop {
            let upload = self.extraction_choice(cancel).await?;
            let account = self.account(upload, cancel).await?;
            let state = self.extraction.lock().await;
            // Connecting may have overlapped another extraction's rejection.
            if state.upload == upload
                && state.cooling[usize::from(upload)]
                    .is_none_or(|until| until <= tokio::time::Instant::now())
            {
                return Ok((upload, account));
            }
        }
    }

    pub async fn extraction_limited(&self, upload: bool, seconds: u64) -> anyhow::Result<()> {
        self.extraction.lock().await.limited(upload, seconds);
        Ok(())
    }

    pub async fn sender(&self, cancel: &CancellationToken) -> anyhow::Result<Arc<Account>> {
        if self
            .store
            .preference("session:upload:self")
            .await?
            .is_some()
        {
            self.account(true, cancel).await
        } else {
            self.account(false, cancel).await
        }
    }
}

#[cfg(test)]
mod flood_tests {
    use super::*;
    use std::num::NonZeroU32;

    fn context(seconds: u32, attempt: u32) -> RetryContext {
        RetryContext {
            fail_count: NonZeroU32::new(attempt).unwrap(),
            slept_so_far: Duration::ZERO,
            error: InvocationError::Rpc(grammers_mtsender::RpcError {
                code: 420,
                name: "FLOOD_WAIT".into(),
                value: Some(seconds),
                caused_by: None,
            }),
        }
    }

    #[test]
    fn all_definitive_floods_wait_an_extra_minute_without_replaying_network_errors() {
        for attempt in 1..=20 {
            assert_eq!(
                TelegramFloodSleep.should_retry(&context(60, attempt)),
                ControlFlow::Continue(Duration::from_secs(120))
            );
        }
        assert_eq!(
            TelegramFloodSleep.should_retry(&context(763, 6)),
            ControlFlow::Continue(Duration::from_secs(823))
        );
        let mut ctx = context(1, 1);
        ctx.error = InvocationError::Dropped;
        assert!(TelegramFloodSleep.should_retry(&ctx).is_break());
        ctx.error = InvocationError::Rpc(grammers_mtsender::RpcError {
            code: 400,
            name: "BAD_REQUEST".into(),
            value: None,
            caused_by: None,
        });
        assert!(TelegramFloodSleep.should_retry(&ctx).is_break());
    }

    async fn simulate_request() -> anyhow::Result<u32> {
        let mut accepted = 59;
        for attempt in 1..=2 {
            let ControlFlow::Continue(delay) =
                TelegramFloodSleep.should_retry(&context(60, attempt))
            else {
                panic!("definitive rejection must wait");
            };
            tokio::time::sleep(delay).await;
        }
        // Retry the rejected RPC only; the prior 59 successes are untouched.
        accepted += 1;
        Ok(accepted)
    }

    #[tokio::test(start_paused = true)]
    async fn short_waits_do_not_consume_nested_request_or_transfer_deadlines() {
        let cancel = CancellationToken::new();
        assert_eq!(
            bounded(&cancel, 10, async {
                bounded(&cancel, 2, simulate_request()).await
            })
            .await
            .unwrap(),
            60
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_flood_sleep_immediately() {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            trigger.cancel();
        });
        let start = tokio::time::Instant::now();
        let error = bounded(&cancel, 2, simulate_request()).await.unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_request_still_times_out_after_flood_wait() {
        let cancel = CancellationToken::new();
        let start = tokio::time::Instant::now();
        let result: anyhow::Result<()> = bounded(&cancel, 2, async {
            let _ = TelegramFloodSleep.should_retry(&context(60, 1));
            tokio::time::sleep(Duration::from_secs(120)).await;
            std::future::pending().await
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), "telegram_timeout");
        assert_eq!(start.elapsed(), Duration::from_secs(122));
    }

    #[tokio::test]
    async fn extraction_floods_surface_without_sleeping_or_changing_normal_retries() {
        let (events, notices) = watch::channel(None);
        let cancel = CancellationToken::new();
        flood_scope(
            events,
            bounded(
                &cancel,
                2,
                extraction_scope(async {
                    assert!(TelegramFloodSleep.should_retry(&context(763, 1)).is_break());
                    assert_eq!(flood_waited(), Duration::ZERO);
                    assert!(notices.borrow().is_none());
                    let error = rpc(context(763, 1).error);
                    assert_eq!(error.downcast_ref::<RetryLater>().unwrap().seconds, 763);
                    Ok(())
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            TelegramFloodSleep.should_retry(&context(763, 1)),
            ControlFlow::Continue(Duration::from_secs(823))
        );
    }

    #[tokio::test]
    async fn collector_inherits_extraction_policy_and_flood_reporting_context() {
        let (events, notices) = watch::channel(None);
        let cancel = CancellationToken::new();
        flood_scope(
            events,
            bounded(
                &cancel,
                2,
                extraction_scope(async {
                    tokio::spawn(inherit_flood_context(async {
                        assert!(TelegramFloodSleep.should_retry(&context(20, 1)).is_break());
                        assert_eq!(flood_waited(), Duration::ZERO);
                        // Explicit cooldown waits still share the parent's budget/events.
                        flood_delay(20);
                    }))
                    .await
                    .unwrap();
                    assert_eq!(flood_waited(), Duration::from_secs(80));
                    assert_eq!(notices.borrow().unwrap().wait.seconds, 20);
                    Ok(())
                }),
            ),
        )
        .await
        .unwrap();
        let normal = tokio::spawn(inherit_flood_context(async {
            TelegramFloodSleep.should_retry(&context(20, 1))
        }))
        .await
        .unwrap();
        assert_eq!(normal, ControlFlow::Continue(Duration::from_secs(80)));
    }

    #[tokio::test(start_paused = true)]
    async fn extraction_rotates_main_upload_main_and_sticks_across_successful_keys() {
        let (_dir, app) = crate::interfaces::progress::tests::fixture();
        app.store
            .set_preference("session:upload:self", "fixture")
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        for _ in 0..3 {
            assert!(!app.users.extraction_choice(&cancel).await.unwrap());
        }
        app.users.extraction_limited(false, 10).await.unwrap();
        assert!(app.users.extraction_choice(&cancel).await.unwrap());
        tokio::time::advance(Duration::from_secs(70)).await;
        // Main has recovered, but successful keys must remain on upload.
        for _ in 0..3 {
            assert!(app.users.extraction_choice(&cancel).await.unwrap());
        }
        app.users.extraction_limited(true, 20).await.unwrap();
        for _ in 0..3 {
            assert!(!app.users.extraction_choice(&cancel).await.unwrap());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn both_cooling_wait_for_next_account_even_when_other_recovers_first() {
        let (_dir, app) = crate::interfaces::progress::tests::fixture();
        app.store
            .set_preference("session:upload:self", "fixture")
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let start = tokio::time::Instant::now();
        app.users.extraction_limited(false, 200).await.unwrap();
        app.users.extraction_limited(true, 20).await.unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        let (events, notices) = watch::channel(None);
        assert!(
            !flood_scope(
                events,
                bounded(&cancel, 2, async {
                    app.users.extraction_choice(&cancel).await
                })
            )
            .await
            .unwrap()
        );
        assert_eq!(start.elapsed(), Duration::from_secs(260));
        assert_eq!(
            notices.borrow().unwrap().until,
            start + Duration::from_secs(260)
        );
        assert!(!app.users.extraction_choice(&cancel).await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn both_cooling_wait_is_cancellable_and_does_not_hold_selector_lock() {
        let (_dir, app) = crate::interfaces::progress::tests::fixture();
        app.store
            .set_preference("session:upload:self", "fixture")
            .await
            .unwrap();
        app.users.extraction_limited(false, 100).await.unwrap();
        app.users.extraction_limited(true, 200).await.unwrap();
        let cancel = CancellationToken::new();
        let (events, mut notices) = watch::channel(None);
        let start = tokio::time::Instant::now();
        let (selection, ()) = tokio::join!(
            flood_scope(events, app.users.extraction_choice(&cancel)),
            async {
                notices.changed().await.unwrap();
                app.users.extraction_limited(false, 300).await.unwrap();
                tokio::time::sleep(Duration::from_secs(1)).await;
                cancel.cancel();
            },
        );
        assert_eq!(selection.unwrap_err().to_string(), "cancelled");
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn missing_upload_login_waits_for_main_instead_of_using_second_account() {
        let (_dir, app) = crate::interfaces::progress::tests::fixture();
        let cancel = CancellationToken::new();
        assert!(!app.users.extraction_choice(&cancel).await.unwrap());
        app.users.extraction_limited(false, 10).await.unwrap();
        let start = tokio::time::Instant::now();
        assert!(!app.users.extraction_choice(&cancel).await.unwrap());
        assert_eq!(start.elapsed(), Duration::from_secs(70));
        app.users.extraction_limited(false, 10).await.unwrap();
        cancel.cancel();
        assert_eq!(
            app.users
                .extraction_choice(&cancel)
                .await
                .unwrap_err()
                .to_string(),
            "cancelled"
        );
    }
}
