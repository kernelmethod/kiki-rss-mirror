-- A slim index of the entry columns that full-text searches sort and filter
-- by; see idx_entry_search in init.sql.
CREATE INDEX idx_entry_search
    ON entries(id, published_at, feed_id, ingested_at, unread_visible);
