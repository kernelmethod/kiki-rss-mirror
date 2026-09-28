-- Distinguish user-managed tags from built-in system tags (read, saved,
-- hidden). See src/db/tags.rs.
ALTER TABLE tags
    ADD COLUMN kind VARCHAR NOT NULL DEFAULT 'user' CHECK (kind IN ('user', 'system'));

-- The 'system:' prefix is now reserved for system tags; move any existing
-- user tags out of the way so the system tags can be seeded.
UPDATE tags SET name = 'user:' || name WHERE name LIKE 'system:%';

INSERT INTO tags (name, kind) VALUES
    ('system:read', 'system'),
    ('system:saved', 'system'),
    ('system:hidden', 'system');

-- entry_tags and feed_tags had no uniqueness constraint, so the
-- `INSERT OR IGNORE`s that link tags to entries and feeds could add the same
-- link many times (e.g. once per refresh of a feed with a tagging script).
-- Remove the duplicates and prevent new ones.
DELETE FROM entry_tags WHERE rowid NOT IN (
    SELECT MIN(rowid) FROM entry_tags GROUP BY entry_id, tag_id
);
DELETE FROM feed_tags WHERE rowid NOT IN (
    SELECT MIN(rowid) FROM feed_tags GROUP BY feed_id, tag_id
);

-- The unique index serves lookups by entry_id, replacing the old index.
DROP INDEX idx_entry_tags_entry_id;
CREATE UNIQUE INDEX idx_entry_tags_unique ON entry_tags(entry_id, tag_id);
CREATE UNIQUE INDEX idx_feed_tags_unique ON feed_tags(feed_id, tag_id);
