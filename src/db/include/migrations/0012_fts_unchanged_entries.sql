-- Reindex an entry for full-text search only when its title, content or URL
-- has changed. Keep in sync with src/db/include/init.sql.
--
-- Every refresh of a feed rewrites the stored entries it still lists, which
-- names these columns whether or not their values differ, so without the
-- WHEN clause each refresh removed and re-added every one of those entries
-- in the full-text index.
DROP TRIGGER IF EXISTS entries_fts_au;
CREATE TRIGGER entries_fts_au AFTER UPDATE OF title, content, url ON entries
WHEN OLD.title IS NOT NEW.title
  OR OLD.content IS NOT NEW.content
  OR OLD.url IS NOT NEW.url
BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, title, content, url)
    VALUES ('delete', old.id, old.title, old.content, old.url);
    INSERT INTO entries_fts(rowid, title, content, url)
    VALUES (new.id, new.title, new.content, new.url);
END;
