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
}

struct PythonFloodSleep;
impl RetryPolicy for PythonFloodSleep {
    fn should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration> {
        let InvocationError::Rpc(error) = &ctx.error else {
            return ControlFlow::Break(());
        };
        let seconds = error.value.unwrap_or(60).max(1) as u64;
        // Match Telethon's default short flood waits and finite request retries.
        // Never replay ambiguous network failures or restart an entire claim/job.
        if error.code != 420 || seconds > 60 || ctx.fail_count.get() > 5 {
            return ControlFlow::Break(());
        }
        let delay = Duration::from_secs(seconds);
        let _ = FLOOD_BUDGETS.try_with(|budgets| {
            for budget in budgets {
                budget.send_modify(|total| *total += delay);
            }
        });
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
                retry_policy: Box::new(PythonFloodSleep),
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

pub struct Users {
    config: Arc<Config>,
    store: Store,
    main: Mutex<Option<Arc<Account>>>,
    upload: Mutex<Option<Arc<Account>>>,
}

impl Users {
    pub fn new(config: Arc<Config>, store: Store) -> Self {
        Self {
            config,
            store,
            main: Mutex::new(None),
            upload: Mutex::new(None),
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
    fn only_definitive_short_floods_retry_with_finite_attempts() {
        for attempt in 1..=5 {
            assert_eq!(
                PythonFloodSleep.should_retry(&context(60, attempt)),
                ControlFlow::Continue(Duration::from_secs(60))
            );
        }
        assert!(PythonFloodSleep.should_retry(&context(61, 1)).is_break());
        assert!(PythonFloodSleep.should_retry(&context(1, 6)).is_break());
        let mut ctx = context(1, 1);
        ctx.error = InvocationError::Dropped;
        assert!(PythonFloodSleep.should_retry(&ctx).is_break());
        ctx.error = InvocationError::Rpc(grammers_mtsender::RpcError {
            code: 400,
            name: "BAD_REQUEST".into(),
            value: None,
            caused_by: None,
        });
        assert!(PythonFloodSleep.should_retry(&ctx).is_break());
    }

    async fn simulate_request() -> anyhow::Result<u32> {
        let mut accepted = 59;
        for attempt in 1..=2 {
            let ControlFlow::Continue(delay) = PythonFloodSleep.should_retry(&context(60, attempt))
            else {
                panic!("short rejection must wait");
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
            let _ = PythonFloodSleep.should_retry(&context(60, 1));
            tokio::time::sleep(Duration::from_secs(60)).await;
            std::future::pending().await
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), "telegram_timeout");
        assert_eq!(start.elapsed(), Duration::from_secs(62));
    }
}
