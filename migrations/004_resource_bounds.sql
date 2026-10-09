CREATE INDEX IF NOT EXISTS transfers_job_status ON transfers(job_id,status);
CREATE INDEX IF NOT EXISTS bot_updates_updated_at ON bot_updates(updated_at);
CREATE INDEX IF NOT EXISTS bot_callbacks_expires ON bot_callbacks(expires);
INSERT OR IGNORE INTO schema_version VALUES (4);
