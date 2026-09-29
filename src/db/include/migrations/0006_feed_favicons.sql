-- The website each feed belongs to, and the favicons fetched from it.
-- See src/tasks/favicons.rs.
ALTER TABLE feeds ADD COLUMN site_url VARCHAR;

CREATE TABLE feed_favicons (
    feed_id     INTEGER PRIMARY KEY,
    asset_id    INTEGER,
    checked_at  INTEGER NOT NULL DEFAULT (unixepoch()),

    FOREIGN KEY (feed_id) REFERENCES feeds(id) ON DELETE CASCADE,
    FOREIGN KEY (asset_id) REFERENCES feed_assets(id) ON DELETE CASCADE
);
CREATE INDEX idx_feed_favicons_asset ON feed_favicons(asset_id);
