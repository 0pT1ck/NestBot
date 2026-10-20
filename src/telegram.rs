use crate::{config::Config, paths::Paths};
use anyhow::{Context, Result, anyhow, bail, ensure};
use grammers_client::{Client, SignInError, client::ClientConfiguration, peer::Peer};
use grammers_mtsender::{
    ConnectionParams, InvocationError, NoRetries, SenderPool, SenderPoolHandle,
};
pub use grammers_session::types::PeerRef;
use grammers_session::{
    storages::SqliteSession,
    types::{PeerAuth, PeerId, PeerKind},
    updates::UpdatesLike,
};
use grammers_tl_types as tl;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    future::Future,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, mpsc},
    task::{AbortHandle, JoinHandle},
    time::{MissedTickBehavior, timeout},
};

const UPDATE_CAPACITY: usize = 64;
const DIALOG_SCAN_LIMIT: usize = 1000;
const PAGE_SIZE: usize = 20;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
static RANDOM_ID: AtomicU64 = AtomicU64::new(0);
// This binary owns one personal account. Retain server cooldown through pool
// replacement so a transport reconnect cannot accidentally bypass FloodWait.
static COOLDOWN: StdMutex<Option<tokio::time::Instant>> = StdMutex::new(None);

/// Deliberately contains no text, media, sender profile, or full PeerMap.
#[derive(Clone, Copy)]
pub struct Event {
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i32,
    pub date: i64,
    pub protected: bool,
    pub peer: PeerRef,
}

#[derive(Clone, Copy)]
struct Source {
    peer: PeerRef,
    protected: bool,
}

struct Inner {
    client: Client,
    handle: SenderPoolHandle,
    runner: Mutex<Option<JoinHandle<()>>>,
    abort: AbortHandle,
    updates: Mutex<Option<mpsc::Receiver<UpdatesLike>>>,
    // Only explicitly validated supergroups, never all observed users/chats.
    sources: StdMutex<BTreeMap<i64, Source>>,
    source_limit: usize,
    relay: OnceLock<PeerRef>,
    rpc_gate: Mutex<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.handle.quit();
        // Dropping the runner drops its JoinSet and aborts every per-DC task.
        self.abort.abort();
    }
}

// Dropping an in-flight RPC future does not cancel the library's queued request.
// Invalidate its entire connection generation instead of allowing repeated
// externally-cancelled calls to accumulate in Sender.requests.
struct RpcCancellation {
    abort: AbortHandle,
    armed: bool,
}

impl Drop for RpcCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.abort.abort();
        }
    }
}

#[derive(Clone)]
pub struct Telegram(Arc<Inner>);

impl Telegram {
    async fn open(config: &Config, paths: &Paths) -> Result<Self> {
        let path = paths.file("data/telegram.session")?;
        // Validate existing SQLite sidecars as well as the primary file.
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = paths.file(&format!("data/telegram.session{suffix}"))?;
            if sidecar.exists() {
                ensure!(sidecar.is_file(), "session sidecar is not a regular file");
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).context("open Telegram session file")?;
        ensure!(file.metadata()?.is_file(), "session is not a regular file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        drop(file);
        let session = Arc::new(
            SqliteSession::open(path)
                .await
                .context("open Telegram SQLite session")?,
        );
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::with_configuration(
            session.clone(),
            config.api_id,
            ConnectionParams {
                updates_channel_capacity: NonZeroUsize::new(UPDATE_CAPACITY).unwrap(),
                retry_policy: Box::new(NoRetries),
                ..Default::default()
            },
        );
        let client = Client::with_configuration(
            handle.clone(),
            ClientConfiguration {
                auto_cache_peers: false,
            },
        );
        let runner = tokio::spawn(runner.run());
        Ok(Self(Arc::new(Inner {
            client,
            handle: handle.thin,
            abort: runner.abort_handle(),
            runner: Mutex::new(Some(runner)),
            updates: Mutex::new(Some(updates)),
            sources: StdMutex::new(BTreeMap::new()),
            source_limit: config.max_rules.saturating_add(1),
            relay: OnceLock::new(),
            rpc_gate: Mutex::new(()),
        })))
    }

    /// Run mode never requests phone, code, or password.
    pub async fn connect(config: &Config, paths: &Paths) -> Result<Self> {
        let telegram = Self::open(config, paths).await?;
        ensure!(
            telegram.rpc(telegram.0.client.is_authorized()).await?,
            "Telegram is not authorized; run login interactively first"
        );
        let me = telegram.rpc(telegram.0.client.get_me()).await?;
        ensure!(
            !me.is_bot(),
            "MTProto session must belong to a personal user account"
        );
        ensure!(
            me.id().bare_id() == Some(config.owner_id),
            "Telegram account does not match owner_id"
        );
        let relay = telegram.validate_group(config.relay_chat_id).await?;
        telegram
            .0
            .relay
            .set(relay)
            .map_err(|_| anyhow!("relay already initialized"))?;
        Ok(telegram)
    }

    /// Only this explicit CLI operation performs terminal interaction.
    pub async fn login(config: &Config, paths: &Paths) -> Result<()> {
        let telegram = Self::open(config, paths).await?;
        let result = async {
            if !telegram.rpc(telegram.0.client.is_authorized()).await? {
                let phone = rpassword::prompt_password("Phone number (with country code): ")
                    .context("read phone")?;
                let token = telegram
                    .rpc(
                        telegram
                            .0
                            .client
                            .request_login_code(phone.trim(), &config.api_hash),
                    )
                    .await?;
                drop(phone);
                let code = rpassword::prompt_password("Telegram login code: ")
                    .context("read login code")?;
                match telegram
                    .rpc(telegram.0.client.sign_in(&token, code.trim()))
                    .await
                {
                    Ok(_) => {}
                    Err(error) => match error.downcast::<SignInError>() {
                        Ok(SignInError::PasswordRequired(token)) => {
                            let password =
                                rpassword::prompt_password("Two-step verification password: ")
                                    .context("read password")?;
                            telegram
                                .rpc(telegram.0.client.check_password(token, password.as_bytes()))
                                .await?;
                        }
                        Ok(other) => return Err(other.into()),
                        Err(other) => return Err(other),
                    },
                }
            }
            let me = telegram.rpc(telegram.0.client.get_me()).await?;
            ensure!(
                !me.is_bot() && me.id().bare_id() == Some(config.owner_id),
                "logged-in personal account must match owner_id"
            );
            Ok(())
        }
        .await;
        telegram.close().await;
        result
    }

    /// Stops all clones. Call after cancelling/joining application workers.
    /// SQLite writes are committed by the library; raw live updates have no saved cursor.
    pub async fn close(&self) {
        self.0.handle.quit();
        let mut task = self.0.runner.lock().await;
        if let Some(mut runner) = task.take()
            && timeout(Duration::from_secs(5), &mut runner).await.is_err()
        {
            runner.abort();
            let _ = runner.await;
        }
    }

    // A timeout invalidates the whole pool: do not accumulate cancelled RPCs in
    // the library's unbounded queues by repeatedly timing out and submitting more.
    async fn rpc<T, E>(&self, future: impl Future<Output = std::result::Result<T, E>>) -> Result<T>
    where
        E: Into<anyhow::Error>,
    {
        // No request has been polled/enqueued while waiting, so cancellation here
        // is safe and does not need to invalidate the connection.
        loop {
            let deadline = *COOLDOWN.lock().map_err(|_| anyhow!("cooldown poisoned"))?;
            match deadline {
                Some(deadline) if deadline > tokio::time::Instant::now() => {
                    tokio::time::sleep_until(deadline).await
                }
                _ => break,
            }
        }
        self.rpc_now(future).await
    }

    // Protocol Ping bypasses application FloodWait cooldown; it sends no content.
    async fn rpc_now<T, E>(
        &self,
        future: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<T>
    where
        E: Into<anyhow::Error>,
    {
        let mut cancellation = RpcCancellation {
            abort: self.0.abort.clone(),
            armed: true,
        };
        match timeout(RPC_TIMEOUT, future).await {
            Ok(result) => {
                cancellation.armed = false;
                let result = result.map_err(Into::into);
                if let Err(error) = &result {
                    let invocation = error.downcast_ref::<InvocationError>().or_else(|| {
                        match error.downcast_ref::<SignInError>() {
                            Some(SignInError::Other(invocation)) => Some(invocation),
                            _ => None,
                        }
                    });
                    if let Some(InvocationError::Rpc(rpc)) = invocation
                        && rpc.code == 420
                        && let Some(seconds) = rpc.value
                    {
                        let until =
                            tokio::time::Instant::now() + Duration::from_secs(u64::from(seconds));
                        let mut cooldown =
                            COOLDOWN.lock().map_err(|_| anyhow!("cooldown poisoned"))?;
                        *cooldown = Some(cooldown.map_or(until, |previous| previous.max(until)));
                    }
                }
                // Report the original failure, never blindly retry the forward.
                result
            }
            Err(_) => bail!("Telegram request timed out; connection must be replaced"),
        }
    }

    /// Single consumer, raw live-only delivery. No MessageBoxes/getDifference,
    /// no message-history fetch, no per-message spawned tasks or forwarding await.
    /// The caller owns the pump task and must abort/join it before reconnecting.
    pub async fn pump(&self, tx: mpsc::Sender<Event>) -> Result<()> {
        let mut updates = self
            .0
            .updates
            .lock()
            .await
            .take()
            .context("Telegram pump already taken")?;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut dropped = 0_u64;
        loop {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                item = updates.recv() => match item.context("Telegram update channel closed")? {
                    UpdatesLike::ConnectionClosed => bail!("Telegram connection closed; discard this live generation"),
                    UpdatesLike::MalformedUpdates => bail!("Telegram malformed updates; discard this live generation"),
                    UpdatesLike::Updates(batch) => {
                        match batch {
                            tl::enums::Updates::Updates(batch) => self.deliver(batch.updates, batch.chats, &tx, &mut dropped)?,
                            tl::enums::Updates::Combined(batch) => self.deliver(batch.updates, batch.chats, &tx, &mut dropped)?,
                            tl::enums::Updates::UpdateShort(batch) => self.deliver(vec![batch.update], Vec::new(), &tx, &mut dropped)?,
                            tl::enums::Updates::TooLong => bail!("Telegram reported an update gap; restart without catch-up"),
                            // Short user/basic-group messages are outside the supergroup scope.
                            _ => {},
                        }
                    }
                    _ => {},
                },
                _ = heartbeat.tick() => {
                    if dropped != 0 {
                        tracing::warn!(dropped, "Telegram event queue overflow; new events discarded");
                        dropped = 0;
                    }
                    self.rpc_now(self.0.client.invoke(&tl::functions::Ping { ping_id: next_random_id() })).await?;
                },
            }
        }
    }

    fn deliver(
        &self,
        updates: Vec<tl::enums::Update>,
        chats: Vec<tl::enums::Chat>,
        tx: &mpsc::Sender<Event>,
        dropped: &mut u64,
    ) -> Result<()> {
        let mut sources = self
            .0
            .sources
            .lock()
            .map_err(|_| anyhow!("source registry poisoned"))?;
        // Refresh only already selected sources; never accumulate arbitrary peers.
        for chat in chats {
            if let tl::enums::Chat::Channel(channel) = chat
                && let Some(source) = sources.get_mut(&channel.id)
            {
                if !channel.megagroup || channel.broadcast || channel.left {
                    sources.remove(&channel.id);
                    continue;
                }
                source.protected = channel.noforwards;
                if !channel.min
                    && let Some(hash) = channel.access_hash
                {
                    source.peer.auth = PeerAuth::from_hash(hash);
                }
            }
        }
        for update in updates {
            let tl::enums::Update::NewChannelMessage(update) = update else {
                continue;
            };
            let tl::enums::Message::Message(message) = update.message else {
                continue;
            };
            if message.out || message.post || message.id <= 0 {
                continue;
            }
            let tl::enums::Peer::Channel(channel) = message.peer_id else {
                continue;
            };
            if self.0.relay.get().and_then(|peer| peer.id.bare_id()) == Some(channel.channel_id) {
                continue;
            }
            let Some(tl::enums::Peer::User(user)) = message.from_id else {
                continue;
            };
            if user.user_id <= 0 {
                continue;
            }
            let Some(source) = sources.get(&channel.channel_id) else {
                continue;
            };
            let event = Event {
                chat_id: channel.channel_id,
                user_id: user.user_id,
                message_id: message.id,
                date: i64::from(message.date),
                protected: source.protected || message.noforwards,
                peer: source.peer,
            };
            match tx.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => *dropped = dropped.saturating_add(1),
                Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
            }
        }
        Ok(())
    }

    /// Accepts a bare positive channel ID or its Bot API -100... representation.
    /// A finite dialog scan is required: ID alone is not enough to obtain access_hash.
    pub async fn validate_group(&self, id: i64) -> Result<PeerRef> {
        let wanted = group_id(id)?;
        ensure!(
            self.0.relay.get().is_none_or(|relay| relay.id != wanted),
            "relay cannot be used as a watched source"
        );
        let _guard = self.0.rpc_gate.lock().await;
        let mut dialogs = self.0.client.iter_dialogs().limit(DIALOG_SCAN_LIMIT);
        while let Some(dialog) = self.rpc(dialogs.next()).await? {
            if dialog.peer_id() != wanted {
                continue;
            }
            let source = source_from_peer(dialog.peer(), dialog.peer_ref())
                .context("source must be a joined, accessible supergroup, not a basic group or broadcast channel")?;
            let mut sources = self
                .0
                .sources
                .lock()
                .map_err(|_| anyhow!("source registry poisoned"))?;
            let bare = wanted.bare_id().context("missing group ID")?;
            ensure!(
                sources.contains_key(&bare) || sources.len() < self.0.source_limit,
                "validated source registry is full; remove unused source or restart"
            );
            sources.insert(bare, source);
            return Ok(source.peer);
        }
        bail!(
            "supergroup not found within first {DIALOG_SCAN_LIMIT} dialogs; ensure personal account joined it"
        )
    }

    /// Call after deleting the last watch for a source; never removes the relay.
    pub fn forget_group(&self, id: i64) {
        if self.0.relay.get().and_then(|peer| peer.id.bare_id()) == Some(id) {
            return;
        }
        if let Ok(mut sources) = self.0.sources.lock() {
            sources.remove(&id);
        }
    }

    /// Pages are one-based. At most 20 entries, 1000 scanned dialogs, 50 pages.
    pub async fn groups(&self, page: usize) -> Result<String> {
        ensure!(
            (1..=DIALOG_SCAN_LIMIT / PAGE_SIZE).contains(&page),
            "group page must be in 1..=50"
        );
        let _guard = self.0.rpc_gate.lock().await;
        let mut dialogs = self.0.client.iter_dialogs().limit(DIALOG_SCAN_LIMIT);
        let mut skipped = 0;
        let mut shown = 0;
        let mut output = format!("超级群第 {page} 页（最多扫描 {DIALOG_SCAN_LIMIT} 个会话）\n");
        while let Some(dialog) = self.rpc(dialogs.next()).await? {
            if source_from_peer(dialog.peer(), dialog.peer_ref()).is_none() {
                continue;
            }
            if skipped < (page - 1) * PAGE_SIZE {
                skipped += 1;
                continue;
            }
            let title: String = dialog
                .peer()
                .name()
                .unwrap_or("未命名")
                .chars()
                .filter(|c| !c.is_control())
                .take(60)
                .collect();
            output.push_str(&format!(
                "{} {}\n",
                dialog.peer_id().bare_id().unwrap_or_default(),
                title
            ));
            shown += 1;
            if shown == PAGE_SIZE {
                break;
            }
        }
        if shown == 0 {
            output.push_str("此扫描范围内没有更多超级群。\n");
        }
        Ok(output)
    }

    pub async fn resolve_user(&self, value: &str) -> Result<i64> {
        let value = value.trim();
        if let Ok(id) = value.parse::<i64>() {
            ensure!(
                PeerId::user(id).is_some(),
                "user ID must be a valid positive personal user ID"
            );
            return Ok(id);
        }
        let username = value.strip_prefix('@').unwrap_or(value);
        ensure!(
            !username.is_empty()
                && username.len() <= 32
                && username
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "expected positive user ID or @username"
        );
        let _guard = self.0.rpc_gate.lock().await;
        match self.rpc(self.0.client.resolve_username(username)).await? {
            Some(Peer::User(user)) => user.id().bare_id().context("resolved user has no ID"),
            _ => bail!("username does not resolve to a User; channels and groups are unsupported"),
        }
    }

    pub async fn forward(&self, source: PeerRef, message: i32) -> Result<i32> {
        ensure!(
            message > 0 && source.id.kind() == PeerKind::Channel,
            "invalid source message"
        );
        let registered = self
            .0
            .sources
            .lock()
            .map_err(|_| anyhow!("source registry poisoned"))?
            .get(&source.id.bare_id().context("missing source ID")?)
            .copied()
            .context("source is not a validated supergroup")?;
        ensure!(
            !registered.protected,
            "protected content cannot be forwarded"
        );
        let relay = *self.0.relay.get().context("relay is not initialized")?;
        ensure!(
            source.id != relay.id,
            "relay cannot be used as a watched source"
        );
        let _guard = self.0.rpc_gate.lock().await;
        let random_id = next_random_id();
        // High-level forward_messages hardcodes silent=false in 0.11.0.
        let result = self
            .rpc(
                self.0
                    .client
                    .invoke(&tl::functions::messages::ForwardMessages {
                        silent: true,
                        background: false,
                        with_my_score: false,
                        drop_author: false,
                        drop_media_captions: false,
                        from_peer: registered.peer.into(),
                        id: vec![message],
                        random_id: vec![random_id],
                        to_peer: relay.into(),
                        top_msg_id: None,
                        reply_to: None,
                        schedule_date: None,
                        schedule_repeat_period: None,
                        send_as: None,
                        noforwards: false,
                        quick_reply_shortcut: None,
                        allow_paid_floodskip: false,
                        effect: None,
                        video_timestamp: None,
                        allow_paid_stars: None,
                        suggested_post: None,
                        from_ephemeral: false,
                    }),
            )
            .await?;
        forwarded_id(result, random_id)
    }

    pub async fn delete_relay(&self, id: i32) -> Result<()> {
        ensure!(id > 0, "invalid relay message ID");
        let relay = *self.0.relay.get().context("relay is not initialized")?;
        let _guard = self.0.rpc_gate.lock().await;
        self.rpc(self.0.client.delete_messages(relay, &[id]))
            .await?;
        Ok(())
    }
}

fn group_id(id: i64) -> Result<PeerId> {
    // PeerId::channel uses eager then_some arithmetic even for invalid IDs.
    // Bound the bare input ourselves to prevent i64 overflow in that API.
    ensure!(
        id <= 997_852_516_352,
        "invalid supergroup ID (monoforums unsupported)"
    );
    let peer = if id > 0 {
        PeerId::channel(id)
    } else {
        PeerId::from_bot_api_dialog_id(id)
    }
    .context("invalid supergroup ID")?;
    ensure!(
        peer.kind() == PeerKind::Channel,
        "basic group IDs are unsupported"
    );
    Ok(peer)
}

fn source_from_peer(peer: &Peer, reference: PeerRef) -> Option<Source> {
    let Peer::Group(group) = peer else {
        return None;
    };
    let tl::enums::Chat::Channel(channel) = &group.raw else {
        return None;
    };
    (channel.megagroup && !channel.broadcast && !channel.left && !channel.min).then_some(Source {
        peer: reference,
        protected: channel.noforwards,
    })
}

fn next_random_id() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut previous = RANDOM_ID.load(Ordering::Relaxed);
    loop {
        let candidate = previous.max(now).wrapping_add(1);
        match RANDOM_ID.compare_exchange_weak(
            previous,
            candidate,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return candidate as i64,
            Err(current) => previous = current,
        }
    }
}

fn forwarded_id(result: tl::enums::Updates, random_id: i64) -> Result<i32> {
    let updates = match result {
        tl::enums::Updates::Updates(updates) => updates.updates,
        tl::enums::Updates::Combined(updates) => updates.updates,
        tl::enums::Updates::UpdateShort(update) => vec![update.update],
        _ => bail!(
            "forward response has no message mapping; delivery outcome is uncertain; do not blindly retry"
        ),
    };
    updates.into_iter().find_map(|update| match update {
        tl::enums::Update::MessageId(mapping) if mapping.random_id == random_id && mapping.id > 0 => Some(mapping.id),
        _ => None,
    }).context("forward response lacks matching random_id; delivery outcome is uncertain; do not blindly retry")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_id_domains_are_not_mixed() {
        assert_eq!(
            group_id(123).unwrap(),
            group_id(-1_000_000_000_123).unwrap()
        );
        for id in [0, -123, i64::MIN, i64::MAX] {
            assert!(group_id(id).is_err());
        }
    }

    #[test]
    fn forward_result_requires_exact_random_id() {
        let result = tl::enums::Updates::UpdateShort(tl::types::UpdateShort {
            update: tl::types::UpdateMessageId {
                random_id: 71,
                id: 42,
            }
            .into(),
            date: 0,
        });
        assert_eq!(forwarded_id(result.clone(), 71).unwrap(), 42);
        assert!(forwarded_id(result, 72).is_err());
        assert!(forwarded_id(tl::enums::Updates::TooLong, 71).is_err());
    }

    #[test]
    fn random_ids_are_distinct() {
        assert_ne!(next_random_id(), next_random_id());
    }
}
