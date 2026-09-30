-- New entries whose assets have yet to be cached, so that caching them
-- survives a full task queue, a failed task, or a restart. See
-- src/db/pending_assets.rs. Keep in sync with src/db/include/init.sql.
CREATE TABLE pending_entry_assets (
    entry_id         INTEGER PRIMARY KEY,
    attempts         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at  INTEGER NOT NULL,

    FOREIGN KEY (entry_id) REFERENCES entries(id) ON DELETE CASCADE
);
CREATE INDEX idx_pending_entry_assets_next ON pending_entry_assets(next_attempt_at);
