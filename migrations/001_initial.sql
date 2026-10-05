CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);
INSERT OR IGNORE INTO schema_version VALUES (1);
CREATE TABLE IF NOT EXISTS jobs (
 id TEXT PRIMARY KEY, kind TEXT NOT NULL, status TEXT NOT NULL,
 payload BLOB NOT NULL, reply_chat INTEGER, created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL, completed INTEGER NOT NULL DEFAULT 0,
 total INTEGER, phase TEXT NOT NULL DEFAULT 'queued', error_code TEXT,
 retry_at INTEGER, attempts INTEGER NOT NULL DEFAULT 0, album_key TEXT
);
CREATE INDEX IF NOT EXISTS jobs_ready ON jobs(status, retry_at, created_at);
CREATE INDEX IF NOT EXISTS jobs_album ON jobs(album_key,status);
CREATE TABLE IF NOT EXISTS batches (
 id TEXT PRIMARY KEY, keyword BLOB NOT NULL, page INTEGER NOT NULL DEFAULT 0,
 result_msg_id INTEGER, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS entries (
 batch_id TEXT NOT NULL REFERENCES batches(id), seq INTEGER NOT NULL,
 payload_hash TEXT NOT NULL, body BLOB NOT NULL,
 PRIMARY KEY(batch_id,seq), UNIQUE(batch_id,payload_hash)
);
CREATE TABLE IF NOT EXISTS transfers (
 scope TEXT NOT NULL, media_id TEXT NOT NULL, job_id TEXT NOT NULL,
 status TEXT NOT NULL, target_message INTEGER, updated_at INTEGER NOT NULL,
 PRIMARY KEY(scope,media_id)
);
CREATE TABLE IF NOT EXISTS preferences (name TEXT PRIMARY KEY, value BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS bot_updates (update_id INTEGER PRIMARY KEY, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS legacy_claims (id TEXT PRIMARY KEY, body BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS claim_inbox (
 job_id TEXT NOT NULL, claim TEXT NOT NULL, message_id INTEGER NOT NULL,
 media_id TEXT NOT NULL, processed INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(job_id,claim,media_id)
);
CREATE INDEX IF NOT EXISTS claim_inbox_pending ON claim_inbox(job_id,claim,processed,message_id);
CREATE TABLE IF NOT EXISTS media_batches (
 owner INTEGER PRIMARY KEY, target BLOB NOT NULL, messages BLOB NOT NULL,
 caption BLOB NOT NULL, tags BLOB NOT NULL, updated_at INTEGER NOT NULL
);
