-- HTTP cache state moved out of `feeds` and `feed_assets` into one table,
-- `http_cache`, keyed by the URL it was fetched from. See init.sql for what
-- each column holds.
CREATE TABLE http_cache (
    url                     TEXT PRIMARY KEY,
    etag                    TEXT,
    last_modified           TEXT,
    expires_at              INTEGER,
    immutable_until         INTEGER,
    body_hash               TEXT,
    last_full_refresh_at    INTEGER
);

INSERT INTO http_cache
    (url, etag, last_modified, expires_at, immutable_until, body_hash, last_full_refresh_at)
SELECT url, header_etag, header_last_modified, header_expires, header_immutable_until,
       header_body_hash, last_full_refresh_at
FROM feeds
WHERE url IS NOT NULL
  AND (header_etag IS NOT NULL
       OR header_last_modified IS NOT NULL
       OR header_expires IS NOT NULL
       OR header_immutable_until IS NOT NULL
       OR header_body_hash IS NOT NULL
       OR last_full_refresh_at IS NOT NULL);

-- Several assets can share a URL; the first one stored wins.
INSERT INTO http_cache (url, etag, last_modified)
SELECT original_url, etag, last_modified
FROM feed_assets
WHERE etag IS NOT NULL OR last_modified IS NOT NULL
ORDER BY id
ON CONFLICT (url) DO NOTHING;

CREATE TRIGGER http_cache_feed_ad AFTER DELETE ON feeds
WHEN OLD.url IS NOT NULL
BEGIN
    DELETE FROM http_cache WHERE url = OLD.url
      AND NOT EXISTS (SELECT 1 FROM feed_assets WHERE original_url = OLD.url);
END;

CREATE TRIGGER http_cache_feed_au AFTER UPDATE OF url ON feeds
WHEN OLD.url IS NOT NULL AND OLD.url IS NOT NEW.url
BEGIN
    DELETE FROM http_cache WHERE url = OLD.url
      AND NOT EXISTS (SELECT 1 FROM feed_assets WHERE original_url = OLD.url);
END;

CREATE TRIGGER http_cache_asset_ad AFTER DELETE ON feed_assets
BEGIN
    DELETE FROM http_cache WHERE url = OLD.original_url
      AND NOT EXISTS (SELECT 1 FROM feeds WHERE url = OLD.original_url)
      AND NOT EXISTS (SELECT 1 FROM feed_assets WHERE original_url = OLD.original_url);
END;

ALTER TABLE feeds DROP COLUMN header_etag;
ALTER TABLE feeds DROP COLUMN header_last_modified;
ALTER TABLE feeds DROP COLUMN header_body_hash;
ALTER TABLE feeds DROP COLUMN last_full_refresh_at;
ALTER TABLE feeds DROP COLUMN header_expires;
ALTER TABLE feeds DROP COLUMN header_immutable_until;
ALTER TABLE feed_assets DROP COLUMN etag;
ALTER TABLE feed_assets DROP COLUMN last_modified;
