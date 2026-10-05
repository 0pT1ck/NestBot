pub mod parser;
pub mod session;
pub mod transfer;

use crate::{config::Config, storage::Store};
use grammers_client::{
    Client,
    client::{ClientConfiguration, NoRetries, UpdatesConfiguration},
    message::Message,
    update::Update,
};
use grammers_mtsender::{ConnectionParams, InvocationError, SenderPool};
use grammers_session::{
    Session,
    types::{PeerId, PeerRef},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, broadcast};
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

pub fn rpc(error: InvocationError) -> anyhow::Error {
    match error {
        InvocationError::Rpc(ref e) if e.code == 420 => RetryLater {
            seconds: e.value.unwrap_or(60).max(1) as u64,
        }
        .into(),
        _ => anyhow::anyhow!("telegram_failed"),
    }
}

pub async fn bounded<T>(
    cancel: &CancellationToken,
    seconds: u64,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        biased;
        _=cancel.cancelled()=>anyhow::bail!("cancelled"),
        result=tokio::time::timeout(Duration::from_secs(seconds),future)=>result.map_err(|_|anyhow::anyhow!("telegram_timeout"))?,
    }
}

pub struct Account {
    pub client: Client,
    pub messages: broadcast::Sender<Message>,
    session: Arc<session::EncryptedSession>,
    pool_task: tokio::task::JoinHandle<()>,
    update_task: tokio::task::JoinHandle<()>,
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
                retry_policy: Box::new(NoRetries),
                auto_cache_peers: true,
            },
        );
        let pool_task = tokio::spawn(async move {
            let _ = runner.run().await;
        });
        let stream = client
            .stream_updates(
                updates,
                UpdatesConfiguration {
                    catch_up: false,
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
        Ok(Self {
            client,
            messages,
            session,
            pool_task,
            update_task,
        })
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
    ) -> anyhow::Result<()> {
        self.client
            .invoke(
                &grammers_tl_types::functions::messages::GetBotCallbackAnswer {
                    game: false,
                    peer: peer.into(),
                    msg_id: message.id(),
                    data: Some(data),
                    password: None,
                },
            )
            .await
            .map_err(rpc)?;
        Ok(())
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
