pub mod claim;
pub mod migration;
pub mod search;

use crate::{config::Config, domain::*, storage::Store, telegram::Users};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

pub struct App {
    pub config: Arc<Config>,
    pub store: Store,
    pub users: Users,
    pub notify: Notify,
    pub shutdown: CancellationToken,
    pub events: broadcast::Sender<ServiceEvent>,
    pub active: Mutex<HashMap<String, CancellationToken>>,
    pub bot: Option<teloxide::Bot>,
}

impl App {
    pub fn new(config: Config, store: Store) -> anyhow::Result<Arc<Self>> {
        let config = Arc::new(config);
        let bot = if let Some(token) = Config::env_secret(&config.telegram.bot_token_env) {
            let mut builder = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(
                    config.limits.poll_secs as u64 + 20,
                ))
                .pool_max_idle_per_host(2);
            if let Some(proxy) = Config::env_secret(&config.telegram.proxy_env) {
                builder = builder.proxy(
                    reqwest::Proxy::all(proxy.as_str())
                        .map_err(|_| anyhow::anyhow!("invalid_proxy"))?,
                );
            }
            Some(teloxide::Bot::with_client(token.as_str(), builder.build()?))
        } else {
            None
        };
        let (events, _) = broadcast::channel(64);
        Ok(Arc::new(Self {
            users: Users::new(config.clone(), store.clone()),
            config,
            store,
            notify: Notify::new(),
            shutdown: CancellationToken::new(),
            events,
            active: Mutex::new(HashMap::new()),
            bot,
        }))
    }

    pub async fn enqueue(
        &self,
        payload: JobPayload,
        chat: Option<i64>,
        update: Option<i32>,
    ) -> anyhow::Result<String> {
        let id = self.store.enqueue(payload, chat, update).await?;
        self.notify.notify_one();
        self.publish(&id).await;
        Ok(id)
    }

    pub async fn publish(&self, id: &str) {
        if let Ok(Some(job)) = self.store.job(id).await {
            let _ = self.events.send(ServiceEvent {
                event: "job".into(),
                job: Some(job),
            });
        }
    }

    pub async fn progress(
        &self,
        id: &str,
        phase: &str,
        done: u64,
        total: Option<u64>,
    ) -> anyhow::Result<()> {
        self.store.progress(id, phase, done, total).await?;
        self.publish(id).await;
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> anyhow::Result<()> {
        self.store.cancel(id).await?;
        if let Some(token) = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("service_unavailable"))?
            .get(id)
        {
            token.cancel();
        }
        self.publish(id).await;
        Ok(())
    }

    pub async fn target(&self) -> anyhow::Result<String> {
        Ok(self
            .store
            .preference("target")
            .await?
            .unwrap_or_else(|| self.config.default_target.clone()))
    }
    pub async fn mode(&self) -> anyhow::Result<TransferMode> {
        Ok(match self.store.preference("mode").await?.as_deref() {
            Some("deep") => TransferMode::Deep,
            Some("copy") => TransferMode::Copy,
            _ => self.config.default_mode,
        })
    }

    pub async fn execute(&self, job: &Job, cancel: &CancellationToken) -> anyhow::Result<()> {
        match &job.payload {
            JobPayload::Search {
                keyword,
                pages,
                sort,
                resume,
            } => search::run(self, job, keyword, *pages, sort.as_deref(), *resume, cancel).await,
            JobPayload::Transfer {
                keys,
                batch,
                start,
                end,
                mode,
                target,
                redo,
                dry_run,
            } => {
                if let Some(batch) = batch {
                    let mut offset = *start;
                    loop {
                        let entries = self.store.entries(batch, offset, 32).await?;
                        if entries.is_empty() {
                            break;
                        }
                        for (seq, entry) in entries {
                            if end.is_some_and(|end| seq > end) {
                                return Ok(());
                            }
                            claim::run(
                                self,
                                job,
                                &entry.payload(),
                                entry.file_count,
                                *mode,
                                target,
                                *redo,
                                *dry_run,
                                cancel,
                            )
                            .await?;
                            offset = seq + 1;
                        }
                    }
                    Ok(())
                } else {
                    for key in keys {
                        claim::run(self, job, key, None, *mode, target, *redo, *dry_run, cancel)
                            .await?;
                    }
                    Ok(())
                }
            }
            JobPayload::Incoming {
                source_chat,
                message_ids,
                mode,
                target,
                caption,
            } => {
                crate::telegram::transfer::incoming(
                    self,
                    job,
                    *source_chat,
                    message_ids,
                    *mode,
                    target,
                    caption.as_deref(),
                    cancel,
                )
                .await
            }
        }
    }
}
