-- A feed's URL identifies it: the OPML import and the feed-adding endpoints
-- look feeds up by it, and two feeds with the same URL store every entry
-- twice, each copy read and saved separately. Keep in sync with the index in
-- src/db/include/init.sql.
--
-- This fails on a database that already holds two feeds with the same URL;
-- delete the extra feeds first.
CREATE UNIQUE INDEX idx_feeds_url_unique ON feeds(url);
