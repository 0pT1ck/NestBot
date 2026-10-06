CREATE TABLE IF NOT EXISTS claims (id TEXT PRIMARY KEY, body BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS job_media_seen (
 job_id TEXT NOT NULL, media_id TEXT NOT NULL, PRIMARY KEY(job_id,media_id)
);
INSERT OR IGNORE INTO schema_version VALUES (2);
CREATE TABLE IF NOT EXISTS search_selection (
 job_id TEXT NOT NULL, batch_id TEXT NOT NULL, seq INTEGER NOT NULL,
 PRIMARY KEY(job_id,batch_id,seq)
);
CREATE TABLE IF NOT EXISTS bot_callbacks (
 id TEXT PRIMARY KEY, owner INTEGER NOT NULL, expires INTEGER NOT NULL, body BLOB NOT NULL
);
