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
            .filter(|s| !s.is_empty())
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
                options,
            } => {
                let mut options = options.clone();
                let current_target = if job.reply_chat.is_some() {
                    Some(self.target().await?)
                } else {
                    None
                };
                let target = current_target
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(target);
                let searched = if let Some(keyword) = &options.keyword {
                    search::run(
                        self,
                        job,
                        keyword,
                        options.pages,
                        options.sort.as_deref(),
                        false,
                        cancel,
                    )
                    .await?;
                    options.selection = Some(job.summary.id.clone());
                    Some(self.store.vault.index("keyword", keyword))
                } else {
                    None
                };
                let batch = batch.as_ref().or(searched.as_ref());
                let mode =
                    if batch.is_some() && options.selection.is_none() && job.reply_chat.is_some() {
                        self.mode().await?
                    } else {
                        *mode
                    };
                let mut processed = 0;
                if let Some(batch) = batch {
                    if !redo
                        && !options.confirmed
                        && options.selection.is_none()
                        && !crate::interfaces::bot::batch_confirmation(
                            self, job, batch, *start, *end,
                        )
                        .await?
                    {
                        return Ok(());
                    }
                    let mut offset = *start;
                    if options.selection.is_none() {
                        anyhow::ensure!(
                            !self.store.entries(batch, offset, 1).await?.is_empty(),
                            "invalid_range"
                        );
                    }
                    loop {
                        let entries = if let Some(selection) = &options.selection {
                            self.store
                                .selected_entries(selection, batch, offset, 32)
                                .await?
                        } else {
                            self.store.entries(batch, offset, 32).await?
                        };
                        if entries.is_empty() {
                            break;
                        }
                        for (seq, entry) in entries {
                            if options
                                .limit
                                .is_some_and(|limit| limit > 0 && processed >= limit)
                            {
                                return Ok(());
                            }
                            if end.is_some_and(|end| seq > end) {
                                return Ok(());
                            }
                            claim::run(
                                self,
                                job,
                                &if options.use_key {
                                    entry.key.trim().into()
                                } else {
                                    entry.payload()
                                },
                                entry.file_count,
                                mode,
                                target,
                                *redo,
                                *dry_run,
                                cancel,
                            )
                            .await?;
                            processed += 1;
                            offset = seq + 1;
                        }
                    }
                    Ok(())
                } else {
                    let mut seen = std::collections::HashSet::new();
                    for key in keys {
                        let key = key.trim();
                        if key.is_empty() || !seen.insert(key) {
                            continue;
                        }
                        claim::run(self, job, key, None, mode, target, *redo, *dry_run, cancel)
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
                caption_message,
            } => {
                let destination = if *mode == TransferMode::Deep {
                    self.target().await?
                } else {
                    target.clone()
                };
                crate::telegram::transfer::incoming(
                    self,
                    job,
                    *source_chat,
                    message_ids,
                    *mode,
                    &destination,
                    caption.as_deref(),
                    *caption_message,
                    cancel,
                )
                .await
            }
        }
    }
}
