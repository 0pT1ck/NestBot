pub mod vault;

use crate::domain::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use vault::Vault;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    pub vault: Arc<Vault>,
    max_queue: u32,
}

const JOB_COLUMNS: &str =
    "id,kind,status,created_at,updated_at,completed,total,phase,error_code,retry_at";

fn summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobSummary> {
    Ok(JobSummary {
        id: row.get(0)?,
        kind: row.get(1)?,
        status: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        completed: row.get(5)?,
        total: row.get(6)?,
        phase: row.get(7)?,
        error_code: row.get(8)?,
        retry_at: row.get(9)?,
    })
}

impl Store {
    pub fn open(
        path: &Path,
        vault: Arc<Vault>,
        cache_kib: u32,
        max_queue: u32,
    ) -> anyhow::Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(&format!("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA cache_size=-{cache_kib}; PRAGMA temp_store=FILE; PRAGMA wal_autocheckpoint=256;"))?;
        connection.execute_batch(include_str!("../../migrations/001_initial.sql"))?;
        connection.execute_batch(include_str!("../../migrations/002_behavior.sql"))?;
        connection.execute_batch(include_str!("../../migrations/003_batch_progress.sql"))?;
        connection.execute_batch(include_str!("../../migrations/004_resource_bounds.sql"))?;
        let grouped = connection
            .prepare("PRAGMA table_info(claim_inbox)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|s| s == "group_id");
        if !grouped {
            connection.execute("ALTER TABLE claim_inbox ADD COLUMN group_id INTEGER", [])?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            vault,
            max_queue,
        })
    }

    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| anyhow::anyhow!("database_unavailable"))?;
            f(&mut connection)
        })
        .await?
    }

    pub async fn enqueue(
        &self,
        payload: JobPayload,
        reply_chat: Option<i64>,
        update: Option<i32>,
    ) -> anyhow::Result<String> {
        payload.validate()?;
        let id = uuid::Uuid::new_v4().to_string();
        let serialized = Zeroizing::new(serde_json::to_vec(&payload)?);
        anyhow::ensure!(serialized.len() <= 256 * 1024, "job_too_large");
        let encrypted = self.vault.encrypt(&format!("job:{id}"), &serialized)?;
        let kind = payload.kind();
        let max = self.max_queue;
        self.call(move |connection| {
            let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if let Some(update) = update
                && tx.query_row("SELECT 1 FROM bot_updates WHERE update_id=?1", [update], |_| Ok(())).optional()?.is_some() {
                    return Ok(String::new());
                }
            let count: u32 = tx.query_row("SELECT count(*) FROM jobs WHERE status IN ('queued','running','waiting','interrupted','review','cancelling')", [], |r| r.get(0))?;
            anyhow::ensure!(count < max, "queue_full");
            let now = unix_time();
            tx.execute("INSERT INTO jobs(id,kind,status,payload,reply_chat,created_at,updated_at) VALUES(?1,?2,'queued',?3,?4,?5,?5)", params![id,kind,encrypted,reply_chat,now])?;
            if let Some(update) = update { tx.execute("INSERT OR IGNORE INTO bot_updates VALUES(?1,?2)", params![update,now])?; }
            tx.commit()?;
            Ok(id)
        }).await
    }

    pub async fn recover(&self) -> anyhow::Result<()> {
        self.call(|connection| {
            let tx = connection.transaction()?;
            let now = unix_time();
            tx.execute("UPDATE jobs SET status=CASE WHEN EXISTS(SELECT 1 FROM transfers WHERE transfers.job_id=jobs.id AND transfers.status='sending') THEN 'review' WHEN status='cancelling' THEN 'cancelled' ELSE 'queued' END,phase=CASE WHEN EXISTS(SELECT 1 FROM transfers WHERE transfers.job_id=jobs.id AND transfers.status='sending') THEN 'review' WHEN status='cancelling' THEN 'cancelled' ELSE 'recovered' END,retry_at=NULL,updated_at=?1 WHERE status IN ('running','cancelling','interrupted')", [now])?;
            // Historical completed jobs cannot be retried; all other dedupe state is durable.
            // A sending transfer defensively preserves even a completed job's recovery state.
            tx.execute("DELETE FROM claim_inbox WHERE job_id IN (SELECT id FROM jobs WHERE status='completed' AND NOT EXISTS(SELECT 1 FROM transfers WHERE transfers.job_id=jobs.id AND transfers.status='sending'))", [])?;
            tx.execute("DELETE FROM job_media_seen WHERE job_id IN (SELECT id FROM jobs WHERE status='completed' AND NOT EXISTS(SELECT 1 FROM transfers WHERE transfers.job_id=jobs.id AND transfers.status='sending'))", [])?;
            tx.execute("DELETE FROM bot_callbacks WHERE expires < ?1", [now])?;
            tx.execute("DELETE FROM bot_updates WHERE updated_at < ?1", [now - 7 * 86400])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn enqueue_album(
        &self,
        payload: JobPayload,
        owner: i64,
        group: &str,
        update: i32,
    ) -> anyhow::Result<String> {
        payload.validate()?;
        let album_key = self.vault.index("album", &format!("{owner}:{group}"));
        let vault = self.vault.clone();
        let max = self.max_queue;
        self.call(move|c| {
            let tx=c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if tx.query_row("SELECT 1 FROM bot_updates WHERE update_id=?1",[update],|_|Ok(())).optional()?.is_some(){return Ok(String::new());}
            let prior=tx.query_row("SELECT id,payload FROM jobs WHERE album_key=?1 AND status='waiting' AND phase='album'",[&album_key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?))).optional()?;
            let id=if let Some((id,body))=prior {
                let mut previous:JobPayload=serde_json::from_slice(&vault.decrypt(&format!("job:{id}"),&body)?)?;
                if let (JobPayload::Incoming{message_ids:old,caption:old_caption,caption_message:old_carrier,..},JobPayload::Incoming{message_ids,caption,caption_message,..})=(&mut previous,&payload) {
                    for id in message_ids {if !old.contains(id){old.push(*id);}}
                    old.sort_unstable();
                    anyhow::ensure!(old.len()<=10,"invalid_album");
                    if old_caption.as_deref().is_none_or(str::is_empty) {*old_caption=caption.clone();*old_carrier = *caption_message;}
                }
                let body=vault.encrypt(&format!("job:{id}"),&Zeroizing::new(serde_json::to_vec(&previous)?))?;
                tx.execute("UPDATE jobs SET payload=?2,retry_at=?3,updated_at=?4 WHERE id=?1",params![id,body,unix_time()+2,unix_time()])?;
                id
            } else {
                let count:u32=tx.query_row("SELECT count(*) FROM jobs WHERE status IN ('queued','waiting','running','review','interrupted')",[],|r|r.get(0))?;
                anyhow::ensure!(count<max,"queue_full");
                let id=uuid::Uuid::new_v4().to_string();
                let body=vault.encrypt(&format!("job:{id}"),&Zeroizing::new(serde_json::to_vec(&payload)?))?;
                tx.execute("INSERT INTO jobs(id,kind,status,payload,reply_chat,created_at,updated_at,phase,retry_at,album_key) VALUES(?1,?2,'waiting',?3,?4,?5,?5,'album',?6,?7)",params![id,payload.kind(),body,owner,unix_time(),unix_time()+2,album_key])?;
                id
            };
            tx.execute("INSERT INTO bot_updates VALUES(?1,?2)",params![update,unix_time()])?;
            tx.commit()?;Ok(id)
        }).await
    }

    pub async fn next_job(&self) -> anyhow::Result<Option<Job>> {
        self.next_job_lane(false).await
    }

    pub async fn next_job_lane(&self, fast: bool) -> anyhow::Result<Option<Job>> {
        let vault = self.vault.clone();
        self.call(move |connection| {
            let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let selected = tx.query_row(&format!("SELECT {JOB_COLUMNS},payload,reply_chat FROM jobs WHERE (status='queued' OR (status='waiting' AND retry_at<=?1)) AND {} ORDER BY created_at,rowid LIMIT 1",if fast{"kind='incoming_copy'"}else{"kind!='incoming_copy'"}), [unix_time()], |r| Ok((summary(r)?, r.get::<_,Vec<u8>>(10)?, r.get::<_,Option<i64>>(11)?))).optional()?;
            let Some((mut selected,encrypted,reply_chat)) = selected else { return Ok(None); };
            let payload = serde_json::from_slice(&vault.decrypt(&format!("job:{}", selected.id), &encrypted)?)?;
            tx.execute("UPDATE jobs SET status='running',phase='starting',attempts=attempts+1,updated_at=?2 WHERE id=?1", params![selected.id,unix_time()])?;
            tx.commit()?;
            selected.status = "running".into();
            selected.phase = "starting".into();
            Ok(Some(Job { summary: selected, payload, reply_chat }))
        }).await
    }

    pub async fn jobs(&self, limit: u32, offset: u32) -> anyhow::Result<Vec<JobSummary>> {
        self.call(move |connection| {
            let mut statement = connection.prepare(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY created_at DESC,rowid DESC LIMIT ?1 OFFSET ?2"))?;
            Ok(statement.query_map(params![limit.min(100),offset], summary)?.collect::<Result<Vec<_>,_>>()?)
        }).await
    }

    pub async fn job(&self, id: &str) -> anyhow::Result<Option<JobSummary>> {
        let id = id.to_owned();
        self.call(move |c| {
            Ok(c.query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id=?1"),
                [id],
                summary,
            )
            .optional()?)
        })
        .await
    }

    pub async fn progress(
        &self,
        id: &str,
        phase: &str,
        completed: u64,
        total: Option<u64>,
    ) -> anyhow::Result<()> {
        let (id, phase) = (id.to_owned(), phase.to_owned());
        self.call(move |c| {
            c.execute(
                "UPDATE jobs SET phase=?2,completed=?3,total=?4,updated_at=?5 WHERE id=?1",
                params![id, phase, completed, total, unix_time()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn finish(
        &self,
        id: &str,
        status: &str,
        error: Option<&str>,
        retry_at: Option<i64>,
    ) -> anyhow::Result<()> {
        let (id, status, error) = (id.to_owned(), status.to_owned(), error.map(str::to_owned));
        self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute("UPDATE jobs SET status=CASE WHEN status IN ('cancelled','cancelling') AND ?2!='review' THEN 'cancelled' ELSE ?2 END,phase=CASE WHEN status IN ('cancelled','cancelling') AND ?2!='review' THEN 'cancelled' ELSE ?2 END,error_code=CASE WHEN status IN ('cancelled','cancelling') AND ?2!='review' THEN NULL ELSE ?3 END,retry_at=CASE WHEN status IN ('cancelled','cancelling') THEN NULL ELSE ?4 END,updated_at=?5 WHERE id=?1", params![id,status,error,retry_at,unix_time()])?;
            // Read the persisted status: cancellation may have won the finish race.
            let cleanup: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND status='completed' AND NOT EXISTS(SELECT 1 FROM transfers WHERE job_id=?1 AND status='sending'))",
                [&id],
                |r| r.get(0),
            )?;
            if cleanup {
                tx.execute("DELETE FROM claim_inbox WHERE job_id=?1", [&id])?;
                tx.execute("DELETE FROM job_media_seen WHERE job_id=?1", [&id])?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn cancel(&self, id: &str) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.call(move |c| {
            c.execute("UPDATE jobs SET status=CASE WHEN status='running' THEN 'cancelling' ELSE 'cancelled' END,retry_at=NULL,updated_at=?2 WHERE id=?1 AND status IN ('queued','waiting','running','interrupted')", params![id,unix_time()])?;
            Ok(())
        }).await
    }

    pub async fn retry(&self, id: &str, allow_uncertain: bool) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.call(move |c| {
            let tx=c.transaction()?;
            let state: String=tx.query_row("SELECT status FROM jobs WHERE id=?1", [&id], |r|r.get(0))?;
            anyhow::ensure!(["failed","cancelled","interrupted","review","partial"].contains(&state.as_str()), "job_not_retryable");
            let uncertain: u32=tx.query_row("SELECT count(*) FROM transfers WHERE job_id=?1 AND status='sending'", [&id], |r|r.get(0))?;
            anyhow::ensure!(uncertain==0 || allow_uncertain, "transfer_uncertain");
            if allow_uncertain { tx.execute("DELETE FROM transfers WHERE job_id=?1 AND status='sending'", [&id])?; }
            tx.execute("UPDATE jobs SET status='queued',error_code=NULL,retry_at=NULL,attempts=0,updated_at=?2 WHERE id=?1", params![id,unix_time()])?;
            tx.commit()?; Ok(())
        }).await
    }

    pub async fn clear_queue(&self) -> anyhow::Result<u64> {
        self.call(|c| Ok(c.execute("UPDATE jobs SET status='cancelled',phase='cancelled',updated_at=?1 WHERE status IN ('queued','waiting','interrupted')", [unix_time()])? as u64)).await
    }

    pub async fn attempts(&self, id: &str) -> anyhow::Result<u32> {
        let id = id.to_owned();
        self.call(move |c| {
            Ok(c.query_row("SELECT attempts FROM jobs WHERE id=?1", [id], |r| r.get(0))?)
        })
        .await
    }

    pub async fn save_page(
        &self,
        keyword: &str,
        page: u32,
        message_id: Option<i32>,
        entries: Vec<Entry>,
    ) -> anyhow::Result<String> {
        let id = self.vault.index("keyword", keyword);
        let keyword = self
            .vault
            .encrypt(&format!("batch:{id}"), keyword.as_bytes())?;
        let vault = self.vault.clone();
        let mut rows = Vec::with_capacity(entries.len());
        for entry in entries {
            let hash = vault.index("payload", &entry.payload());
            let body = Zeroizing::new(serde_json::to_vec(&entry)?);
            rows.push((
                hash.clone(),
                vault.encrypt(&format!("entry:{id}:{hash}"), &body)?,
            ));
        }
        self.call(move |c| {
            let tx=c.transaction()?;
            // Keep cursor page and message from the same search response. A fresh
            // search may stop earlier; merged entries keep their stable sequences.
            tx.execute("INSERT INTO batches VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET page=CASE WHEN excluded.result_msg_id IS NULL AND batches.result_msg_id IS NOT NULL THEN batches.page ELSE excluded.page END,result_msg_id=COALESCE(excluded.result_msg_id,batches.result_msg_id),updated_at=excluded.updated_at", params![id,keyword,page,message_id,unix_time()])?;
            let mut seq:u32=tx.query_row("SELECT COALESCE(MAX(seq),0) FROM entries WHERE batch_id=?1", [&id], |r|r.get(0))?;
            for (hash,body) in rows {
                let inserted=tx.execute("INSERT OR IGNORE INTO entries VALUES(?1,?2,?3,?4)", params![id,seq+1,hash,body])?;
                if inserted!=0 { seq+=1; }
            }
            tx.commit()?; Ok(id)
        }).await
    }

    pub async fn batches(&self, limit: u32, offset: u32) -> anyhow::Result<Vec<BatchSummary>> {
        self.call(move |c| {
            let mut stmt=c.prepare("SELECT b.id,(SELECT count(*) FROM entries e WHERE e.batch_id=b.id),b.page,b.updated_at FROM batches b ORDER BY b.updated_at DESC,b.id LIMIT ?1 OFFSET ?2")?;
            Ok(stmt.query_map(params![limit.min(100),offset], |r|Ok(BatchSummary{id:r.get(0)?,entries:r.get(1)?,page:r.get(2)?,updated_at:r.get(3)?}))?.collect::<Result<Vec<_>,_>>()?)
        }).await
    }

    pub async fn batch_keyword(&self, id: &str) -> anyhow::Result<String> {
        let id = id.to_owned();
        let vault = self.vault.clone();
        self.call(move |c| {
            let body: Vec<u8> =
                c.query_row("SELECT keyword FROM batches WHERE id=?1", [&id], |r| {
                    r.get(0)
                })?;
            Ok(String::from_utf8(
                vault.decrypt(&format!("batch:{id}"), &body)?.to_vec(),
            )?)
        })
        .await
    }

    pub async fn resolve_batch(
        &self,
        reference: &str,
        legacy_password: Option<&str>,
    ) -> anyhow::Result<String> {
        let keyword_id = self.vault.index("keyword", reference);
        let alias = self.preference(&format!("batch_alias:{reference}")).await?;
        for id in [Some(reference.to_owned()), Some(keyword_id), alias]
            .into_iter()
            .flatten()
        {
            let candidate = id.clone();
            if self
                .call(move |c| {
                    Ok(
                        c.query_row("SELECT 1 FROM batches WHERE id=?1", [candidate], |_| Ok(()))
                            .optional()?
                            .is_some(),
                    )
                })
                .await?
            {
                return Ok(id);
            }
        }
        // Imports made before aliases were saved can still use Python's
        // HMAC(password, "name:" + keyword) filename without reading .bin files.
        if reference.starts_with("k_")
            && reference.ends_with(".bin")
            && let Some(password) = legacy_password
        {
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            let mut offset = 0;
            loop {
                let batches = self.batches(100, offset).await?;
                if batches.is_empty() {
                    break;
                }
                for batch in batches {
                    let keyword = self.batch_keyword(&batch.id).await?;
                    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(password.as_bytes())?;
                    mac.update(format!("name:{keyword}").as_bytes());
                    let name = format!("k_{}.bin", &hex::encode(mac.finalize().into_bytes())[..24]);
                    if name == reference {
                        self.set_preference(&format!("batch_alias:{reference}"), &batch.id)
                            .await?;
                        return Ok(batch.id);
                    }
                }
                offset += 100;
            }
        }
        anyhow::bail!("batch_not_found")
    }

    pub async fn batch_override(&self, batch: &str, seq: u32) -> anyhow::Result<Option<bool>> {
        let batch = batch.to_owned();
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT completed FROM batch_progress WHERE batch_id=?1 AND seq=?2",
                params![batch, seq],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await
    }

    pub async fn batch_entry_complete(
        &self,
        batch: &str,
        seq: u32,
        entry: &Entry,
    ) -> anyhow::Result<bool> {
        if let Some(completed) = self.batch_override(batch, seq).await? {
            return Ok(completed);
        }
        Ok(self
            .claim_record(&entry.payload())
            .await?
            .complete(entry.file_count))
    }

    pub async fn batch_next_pending(&self, batch: &str) -> anyhow::Result<Option<u32>> {
        let mut offset = 1;
        loop {
            let entries = self.entries(batch, offset, 64).await?;
            if entries.is_empty() {
                return Ok(None);
            }
            for (seq, entry) in entries {
                if !self.batch_entry_complete(batch, seq, &entry).await? {
                    return Ok(Some(seq));
                }
                offset = seq + 1;
            }
        }
    }

    pub async fn set_batch_progress(&self, batch: &str, completed: u32) -> anyhow::Result<()> {
        let batch = batch.to_owned();
        let vault = self.vault.clone();
        self.call(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let count:u32=tx.query_row("SELECT count(*) FROM entries WHERE batch_id=?1",[&batch],|r|r.get(0))?;
            anyhow::ensure!(count>0,"batch_not_found");
            anyhow::ensure!(completed<=count,"invalid_batch_progress");
            {
                let mut stmt=tx.prepare("SELECT id,payload FROM jobs WHERE kind='transfer' AND status IN ('running','cancelling')")?;
                for row in stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?)))? {
                    let (id,body)=row?;
                    let payload:JobPayload=serde_json::from_slice(&vault.decrypt(&format!("job:{id}"),&body)?)?;
                    anyhow::ensure!(!matches!(payload,JobPayload::Transfer {batch:Some(ref current),..} if current==&batch),"batch_busy");
                }
            }
            tx.execute("DELETE FROM batch_progress WHERE batch_id=?1",[&batch])?;
            tx.execute("INSERT INTO batch_progress(batch_id,seq,completed) SELECT batch_id,seq,CASE WHEN seq<=?2 THEN 1 ELSE 0 END FROM entries WHERE batch_id=?1",params![batch,completed])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn finish_batch_entry(
        &self,
        batch: &str,
        seq: u32,
        completed: bool,
    ) -> anyhow::Result<()> {
        let batch = batch.to_owned();
        self.call(move |c| {
            if completed {
                c.execute("DELETE FROM batch_progress WHERE batch_id=?1 AND seq=?2",params![batch,seq])?;
            } else {
                c.execute("INSERT INTO batch_progress VALUES(?1,?2,0) ON CONFLICT(batch_id,seq) DO UPDATE SET completed=0",params![batch,seq])?;
            }
            Ok(())
        }).await
    }

    pub async fn cursor(&self, keyword: &str) -> anyhow::Result<Option<(u32, Option<i32>)>> {
        let id = self.vault.index("keyword", keyword);
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT page,result_msg_id FROM batches WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await
    }

    pub async fn entries(
        &self,
        id: &str,
        start: u32,
        limit: u32,
    ) -> anyhow::Result<Vec<(u32, Entry)>> {
        let id = id.to_owned();
        let vault = self.vault.clone();
        self.call(move |c| {
            let mut stmt=c.prepare("SELECT seq,payload_hash,body FROM entries WHERE batch_id=?1 AND seq>=?2 ORDER BY seq LIMIT ?3")?;
            let rows=stmt.query_map(params![id,start.max(1),limit.min(100)], |r|Ok((r.get::<_,u32>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?)))?;
            let mut result=Vec::new();
            for row in rows { let (seq,hash,body)=row?; result.push((seq,serde_json::from_slice(&vault.decrypt(&format!("entry:{id}:{hash}"),&body)?)?)); }
            Ok(result)
        }).await
    }

    pub async fn preference(&self, name: &str) -> anyhow::Result<Option<String>> {
        let name = name.to_owned();
        let vault = self.vault.clone();
        self.call(move |c| {
            let body = c
                .query_row(
                    "SELECT value FROM preferences WHERE name=?1",
                    [&name],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            body.map(|body| {
                Ok(String::from_utf8(
                    vault.decrypt(&format!("pref:{name}"), &body)?.to_vec(),
                )?)
            })
            .transpose()
        })
        .await
    }

    pub async fn set_preference(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let encrypted = self
            .vault
            .encrypt(&format!("pref:{name}"), value.as_bytes())?;
        let name = name.to_owned();
        self.call(move |c| { c.execute("INSERT INTO preferences VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value", params![name,encrypted])?; Ok(()) }).await
    }

    pub async fn transfer_status(
        &self,
        scope: &str,
        media: &str,
    ) -> anyhow::Result<Option<(String, Option<i32>)>> {
        let (scope, media) = (scope.to_owned(), media.to_owned());
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT status,target_message FROM transfers WHERE scope=?1 AND media_id=?2",
                params![scope, media],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await
    }

    pub async fn transfer_intent(
        &self,
        scope: &str,
        media: &str,
        job: &str,
        redo: bool,
    ) -> anyhow::Result<()> {
        let (scope, media, job) = (scope.to_owned(), media.to_owned(), job.to_owned());
        self.call(move |c| {
            let tx=c.transaction()?;
            let previous=tx.query_row("SELECT status FROM transfers WHERE scope=?1 AND media_id=?2",params![scope,media],|r|r.get::<_,String>(0)).optional()?;
            anyhow::ensure!(previous.as_deref()!=Some("sending"),"transfer_uncertain");
            anyhow::ensure!(redo || previous.as_deref()!=Some("done"),"already_transferred");
            tx.execute("INSERT INTO transfers VALUES(?1,?2,?3,'sending',NULL,?4) ON CONFLICT(scope,media_id) DO UPDATE SET job_id=excluded.job_id,status='sending',target_message=NULL,updated_at=excluded.updated_at",params![scope,media,job,unix_time()])?;
            tx.commit()?; Ok(())
        }).await
    }

    pub async fn transfer_done(
        &self,
        scope: &str,
        media: &str,
        message: i32,
    ) -> anyhow::Result<()> {
        let (scope, media) = (scope.to_owned(), media.to_owned());
        self.call(move |c| { c.execute("UPDATE transfers SET status='done',target_message=?3,updated_at=?4 WHERE scope=?1 AND media_id=?2",params![scope,media,message,unix_time()])?; Ok(()) }).await
    }

    pub async fn has_uncertain(&self, job: &str) -> anyhow::Result<bool> {
        let job = job.to_owned();
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM transfers WHERE job_id=?1 AND status='sending')",
                [job],
                |r| r.get(0),
            )?)
        })
        .await
    }

    pub async fn backup(&self, path: &Path) -> anyhow::Result<()> {
        let path = path.to_owned();
        self.call(move |c| {
            anyhow::ensure!(!path.exists(), "backup_exists");
            c.backup("main", &path, None)?;
            Ok(())
        })
        .await
    }

    pub async fn reset_inbox(&self, job: &str, claim: &str) -> anyhow::Result<()> {
        let (job, claim) = (job.to_owned(), claim.to_owned());
        self.call(move |c| {
            c.execute(
                "DELETE FROM claim_inbox WHERE job_id=?1 AND claim=?2",
                params![job, claim],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn claim_record(&self, payload: &str) -> anyhow::Result<ClaimRecord> {
        use sha2::{Digest, Sha256};
        let id = self.vault.index("claim", payload);
        let legacy = format!("{:x}", Sha256::digest(payload.as_bytes()))[..32].to_owned();
        let vault = self.vault.clone();
        self.call(move |c| {
            if let Some(body) = c
                .query_row("SELECT body FROM claims WHERE id=?1", [&id], |r| {
                    r.get::<_, Vec<u8>>(0)
                })
                .optional()?
            {
                return Ok(serde_json::from_slice(
                    &vault.decrypt(&format!("claim:{id}"), &body)?,
                )?);
            }
            if let Some(body) = c
                .query_row(
                    "SELECT body FROM legacy_claims WHERE id=?1",
                    [&legacy],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()?
            {
                return Ok(serde_json::from_slice(
                    &vault.decrypt(&format!("legacy:{legacy}"), &body)?,
                )?);
            }
            Ok(ClaimRecord::default())
        })
        .await
    }

    pub async fn select_page(
        &self,
        job: &str,
        batch: &str,
        entries: &[Entry],
    ) -> anyhow::Result<()> {
        let (job, batch) = (job.to_owned(), batch.to_owned());
        let hashes = entries
            .iter()
            .map(|e| self.vault.index("payload", &e.payload()))
            .collect::<Vec<_>>();
        self.call(move|c| {let tx=c.transaction()?;for hash in hashes {
            tx.execute("INSERT OR IGNORE INTO search_selection SELECT ?1,batch_id,seq FROM entries WHERE batch_id=?2 AND payload_hash=?3",params![job,batch,hash])?;
        }tx.commit()?;Ok(())}).await
    }

    pub async fn selected_entries(
        &self,
        job: &str,
        batch: &str,
        start: u32,
        limit: u32,
    ) -> anyhow::Result<Vec<(u32, Entry)>> {
        let (job, batch) = (job.to_owned(), batch.to_owned());
        let vault = self.vault.clone();
        self.call(move|c|{let mut stmt=c.prepare("SELECT e.seq,e.payload_hash,e.body FROM entries e JOIN search_selection s ON s.batch_id=e.batch_id AND s.seq=e.seq WHERE s.job_id=?1 AND e.batch_id=?2 AND e.seq>=?3 ORDER BY e.seq LIMIT ?4")?;
            let rows=stmt.query_map(params![job,batch,start,limit.min(100)],|r|Ok((r.get::<_,u32>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?)))?;
            let mut out=vec![];for row in rows {let(seq,hash,body)=row?;out.push((seq,serde_json::from_slice(&vault.decrypt(&format!("entry:{batch}:{hash}"),&body)?)?));}Ok(out)
        }).await
    }

    pub async fn album_intent(
        &self,
        scope: &str,
        media: &[String],
        job: &str,
        redo: bool,
    ) -> anyhow::Result<()> {
        let (scope, media, job) = (scope.to_owned(), media.to_vec(), job.to_owned());
        self.call(move|c|{let tx=c.transaction()?;for id in media {
            let previous=tx.query_row("SELECT status FROM transfers WHERE scope=?1 AND media_id=?2",params![scope,id],|r|r.get::<_,String>(0)).optional()?;
            anyhow::ensure!(previous.as_deref()!=Some("sending"),"transfer_uncertain");
            anyhow::ensure!(redo || previous.as_deref()!=Some("done"),"already_transferred");
            tx.execute("INSERT INTO transfers VALUES(?1,?2,?3,'sending',NULL,?4) ON CONFLICT(scope,media_id) DO UPDATE SET job_id=excluded.job_id,status='sending',target_message=NULL,updated_at=excluded.updated_at",params![scope,id,job,unix_time()])?;
        }tx.commit()?;Ok(())}).await
    }
    pub async fn album_done(&self, scope: &str, sent: &[(String, i32)]) -> anyhow::Result<()> {
        let (scope, sent) = (scope.to_owned(), sent.to_vec());
        self.call(move|c|{let tx=c.transaction()?;for(id,target)in sent {
            tx.execute("UPDATE transfers SET status='done',target_message=?3,updated_at=?4 WHERE scope=?1 AND media_id=?2",params![scope,id,target,unix_time()])?;
        }tx.commit()?;Ok(())}).await
    }

    pub async fn selection_count(&self, job: &str) -> anyhow::Result<u32> {
        let job = job.to_owned();
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM search_selection WHERE job_id=?1",
                [job],
                |r| r.get(0),
            )?)
        })
        .await
    }
    pub async fn callback(&self, owner: i64, value: &serde_json::Value) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let body = self
            .vault
            .encrypt(&format!("callback:{id}"), &serde_json::to_vec(value)?)?;
        self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM bot_callbacks WHERE expires<?1", [unix_time()])?;
            tx.execute(
                "INSERT INTO bot_callbacks VALUES(?1,?2,?3,?4)",
                params![id, owner, unix_time() + 172800, body],
            )?;
            tx.commit()?;
            Ok(format!("C:{id}"))
        })
        .await
    }
    pub async fn callback_value(
        &self,
        owner: i64,
        id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let id = id.to_owned();
        let vault = self.vault.clone();
        self.call(move |c| {
            let body = c
                .query_row(
                    "SELECT body FROM bot_callbacks WHERE id=?1 AND owner=?2 AND expires>=?3",
                    params![id, owner, unix_time()],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            body.map(|body| {
                Ok(serde_json::from_slice(
                    &vault.decrypt(&format!("callback:{id}"), &body)?,
                )?)
            })
            .transpose()
        })
        .await
    }

    pub async fn save_claim(&self, payload: &str, record: &ClaimRecord) -> anyhow::Result<()> {
        self.save_claim_for_batch(payload, record, None).await
    }

    pub async fn save_claim_for_batch(
        &self,
        payload: &str,
        record: &ClaimRecord,
        batch: Option<(&str, u32)>,
    ) -> anyhow::Result<()> {
        let id = self.vault.index("claim", payload);
        let body = self
            .vault
            .encrypt(&format!("claim:{id}"), &serde_json::to_vec(record)?)?;
        let batch = batch
            .filter(|_| record.status == "done")
            .map(|(batch, seq)| (batch.to_owned(), seq));
        self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO claims VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
                params![id, body],
            )?;
            if let Some((batch, seq)) = batch {
                tx.execute(
                    "DELETE FROM batch_progress WHERE batch_id=?1 AND seq=?2",
                    params![batch, seq],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn media_seen(&self, job: &str, media: &str) -> anyhow::Result<bool> {
        let (job, media) = (job.to_owned(), media.to_owned());
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM job_media_seen WHERE job_id=?1 AND media_id=?2)",
                params![job, media],
                |r| r.get(0),
            )?)
        })
        .await
    }

    pub async fn mark_media_seen(&self, job: &str, media: &str) -> anyhow::Result<()> {
        let (job, media) = (job.to_owned(), media.to_owned());
        self.call(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO job_media_seen VALUES(?1,?2)",
                params![job, media],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn report(&self, job: &str) -> anyhow::Result<JobReport> {
        Ok(self
            .preference(&format!("report:{job}"))
            .await?
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default())
    }
    pub async fn save_report(&self, job: &str, report: &JobReport) -> anyhow::Result<()> {
        self.set_preference(&format!("report:{job}"), &serde_json::to_string(report)?)
            .await
    }
    pub async fn warning(&self, job: &str, code: &str) -> anyhow::Result<()> {
        let mut report = self.report(job).await?;
        if !report.warnings.iter().any(|s| s == code) {
            report.warnings.push(code.into());
        }
        self.save_report(job, &report).await
    }

    pub async fn inbox_push(
        &self,
        job: &str,
        claim: &str,
        message: i32,
        media: &str,
    ) -> anyhow::Result<bool> {
        self.inbox_push_group(job, claim, message, media, None)
            .await
    }

    pub async fn inbox_push_group(
        &self,
        job: &str,
        claim: &str,
        message: i32,
        media: &str,
        group: Option<i64>,
    ) -> anyhow::Result<bool> {
        let (job, claim, media) = (job.to_owned(), claim.to_owned(), media.to_owned());
        self.call(move|c|Ok(c.execute("INSERT OR IGNORE INTO claim_inbox(job_id,claim,message_id,media_id,group_id) VALUES(?1,?2,?3,?4,?5)",params![job,claim,message,media,group])?!=0)).await
    }

    pub async fn inbox_next(&self, job: &str, claim: &str) -> anyhow::Result<Vec<i32>> {
        let (job, claim) = (job.to_owned(), claim.to_owned());
        self.call(move|c|{let first=c.query_row("SELECT message_id,group_id FROM claim_inbox WHERE job_id=?1 AND claim=?2 AND processed=0 ORDER BY message_id LIMIT 1",params![job,claim],|r|Ok((r.get::<_,i32>(0)?,r.get::<_,Option<i64>>(1)?))).optional()?;
            let Some((id,group))=first else{return Ok(vec![]);};
            if let Some(group)=group {let mut stmt=c.prepare("SELECT message_id FROM claim_inbox WHERE job_id=?1 AND claim=?2 AND group_id=?3 AND processed=0 ORDER BY message_id LIMIT 10")?;return Ok(stmt.query_map(params![job,claim,group],|r|r.get(0))?.collect::<Result<Vec<_>,_>>()?);}
            Ok(vec![id])
        }).await
    }

    pub async fn inbox_pending(&self, job: &str, claim: &str) -> anyhow::Result<Vec<i32>> {
        let (job, claim) = (job.to_owned(), claim.to_owned());
        self.call(move|c|{let mut stmt=c.prepare("SELECT message_id FROM claim_inbox WHERE job_id=?1 AND claim=?2 AND processed=0 ORDER BY message_id LIMIT 10")?;Ok(stmt.query_map(params![job,claim],|r|r.get(0))?.collect::<Result<Vec<_>,_>>()?)}).await
    }

    pub async fn inbox_done(&self, job: &str, claim: &str, message: i32) -> anyhow::Result<()> {
        let (job, claim) = (job.to_owned(), claim.to_owned());
        self.call(move |c| {
            c.execute(
                "UPDATE claim_inbox SET processed=1 WHERE job_id=?1 AND claim=?2 AND message_id=?3",
                params![job, claim, message],
            )?;
            Ok(())
        })
        .await
    }
}
