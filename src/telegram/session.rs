use crate::storage::Store;
use grammers_session::{
    BoxFuture, Session, SessionData,
    types::{DcOption, PeerId, PeerInfo, UpdateState, UpdatesState},
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct SessionError;
impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session_storage_failed")
    }
}
impl std::error::Error for SessionError {}
impl From<anyhow::Error> for SessionError {
    fn from(_: anyhow::Error) -> Self {
        Self
    }
}
impl From<serde_json::Error> for SessionError {
    fn from(_: serde_json::Error) -> Self {
        Self
    }
}
impl From<&str> for SessionError {
    fn from(_: &str) -> Self {
        Self
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Header {
    home: i32,
    dcs: Vec<DcOption>,
    updates: UpdatesState,
}

pub struct EncryptedSession {
    store: Store,
    role: String,
    header: Mutex<Header>,
}

impl EncryptedSession {
    pub async fn open(store: Store, role: &str) -> anyhow::Result<Arc<Self>> {
        let default = SessionData::default();
        let header = match store.preference(&format!("session:{role}:header")).await? {
            Some(serialized) => serde_json::from_str(&serialized)?,
            None => Header {
                home: default.home_dc,
                dcs: default.dc_options.into_values().collect(),
                updates: default.updates_state,
            },
        };
        Ok(Arc::new(Self {
            store,
            role: role.into(),
            header: Mutex::new(header),
        }))
    }

    async fn save_header(&self) -> Result<(), SessionError> {
        let serialized =
            serde_json::to_string(&*self.header.lock().map_err(|_| "session_unavailable")?)?;
        self.store
            .set_preference(&format!("session:{}:header", self.role), &serialized)
            .await?;
        Ok(())
    }

    fn peer_key(&self, peer: PeerId) -> String {
        if peer == PeerId::self_user() {
            format!("session:{}:self", self.role)
        } else {
            format!(
                "session:{}:peer:{}",
                self.role,
                peer.bot_api_dialog_id_unchecked()
            )
        }
    }
}

impl Session for EncryptedSession {
    type Error = SessionError;

    fn home_dc_id(&self) -> Result<i32, Self::Error> {
        Ok(self.header.lock().map_err(|_| "session_unavailable")?.home)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            self.header.lock().map_err(|_| "session_unavailable")?.home = dc_id;
            self.save_header().await
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Self::Error> {
        Ok(self
            .header
            .lock()
            .map_err(|_| "session_unavailable")?
            .dcs
            .iter()
            .find(|dc| dc.id == dc_id)
            .cloned())
    }

    fn set_dc_option(&self, dc: &DcOption) -> BoxFuture<'_, Result<(), Self::Error>> {
        let dc = dc.clone();
        Box::pin(async move {
            {
                let mut header = self.header.lock().map_err(|_| "session_unavailable")?;
                header.dcs.retain(|d| d.id != dc.id);
                header.dcs.push(dc);
            }
            self.save_header().await
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Self::Error>> {
        Box::pin(async move {
            Ok(self
                .store
                .preference(&self.peer_key(peer))
                .await?
                .map(|s| serde_json::from_str(&s))
                .transpose()?)
        })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), Self::Error>> {
        let peer = peer.clone();
        Box::pin(async move {
            let mut merged = self.peer(peer.id()).await?.unwrap_or_else(|| peer.clone());
            merged.extend_info(&peer);
            let serialized = serde_json::to_string(&merged)?;
            let key = self.peer_key(peer.id());
            if self.store.preference(&key).await?.as_deref() != Some(&serialized) {
                self.store.set_preference(&key, &serialized).await?;
            }
            if matches!(
                peer,
                PeerInfo::User {
                    is_self: Some(true),
                    ..
                }
            ) {
                self.store
                    .set_preference(&self.peer_key(PeerId::self_user()), &serialized)
                    .await?;
            }
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Self::Error>> {
        Box::pin(async move {
            Ok(self
                .header
                .lock()
                .map_err(|_| "session_unavailable")?
                .updates
                .clone())
        })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            {
                let mut header = self.header.lock().map_err(|_| "session_unavailable")?;
                match update {
                    UpdateState::All(state) => header.updates = state,
                    UpdateState::Primary { pts, date, seq } => {
                        header.updates.pts = pts;
                        header.updates.date = date;
                        header.updates.seq = seq;
                    }
                    UpdateState::Secondary { qts } => header.updates.qts = qts,
                    UpdateState::Channel { id, pts } => {
                        header.updates.channels.retain(|c| c.id != id);
                        header
                            .updates
                            .channels
                            .push(grammers_session::types::ChannelState { id, pts });
                        if header.updates.channels.len() > 2048 {
                            header.updates.channels.remove(0);
                        }
                    }
                }
            }
            self.save_header().await
        })
    }
}
