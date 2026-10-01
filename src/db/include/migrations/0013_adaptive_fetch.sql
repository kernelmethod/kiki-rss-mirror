-- Adaptive fetching: feeds whose short freshness hint keeps turning out
-- to be unchanged are fetched less and less often. Keep in sync with
-- src/db/include/init.sql; see src/tasks/adaptive.rs.
ALTER TABLE feeds ADD COLUMN adaptive_fetch INTEGER;
ALTER TABLE feeds ADD COLUMN adaptive_fetch_level INTEGER NOT NULL DEFAULT 0;
