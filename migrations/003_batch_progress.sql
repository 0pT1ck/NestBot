CREATE TABLE IF NOT EXISTS batch_progress (
 batch_id TEXT NOT NULL,
 seq INTEGER NOT NULL,
 completed INTEGER NOT NULL CHECK(completed IN (0,1)),
 PRIMARY KEY(batch_id,seq),
 FOREIGN KEY(batch_id,seq) REFERENCES entries(batch_id,seq) ON DELETE CASCADE
);
INSERT OR IGNORE INTO schema_version VALUES (3);
