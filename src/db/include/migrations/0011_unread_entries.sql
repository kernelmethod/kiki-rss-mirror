-- Which entries are unread and not hidden, as a column that partial indexes
-- can cover, so that listing and counting them no longer looks at every
-- entry. Keep in sync with src/db/include/init.sql.
--
-- `unread_visible` is 1 while an entry has neither `system:read` nor
-- `system:hidden`, and 0 otherwise. entry_tags remains the record of both:
-- the triggers below keep the column in step with it, however its rows are
-- added or removed.
ALTER TABLE entries ADD COLUMN unread_visible INTEGER NOT NULL DEFAULT 1;
UPDATE entries SET unread_visible = 0 WHERE id IN (
    SELECT et.entry_id FROM entry_tags et JOIN tags t ON t.id = et.tag_id
    WHERE t.name IN ('system:read', 'system:hidden')
);
CREATE INDEX idx_entry_unread_visible ON entries(published_at)
    WHERE unread_visible = 1;
CREATE INDEX idx_entry_feed_unread_visible ON entries(feed_id, published_at)
    WHERE unread_visible = 1;

CREATE TRIGGER entry_tags_unread_visible_ai AFTER INSERT ON entry_tags
WHEN NEW.tag_id IN (
    SELECT id FROM tags WHERE name IN ('system:read', 'system:hidden')
)
BEGIN
    UPDATE entries SET unread_visible = 0
    WHERE id = NEW.entry_id AND unread_visible = 1;
END;

CREATE TRIGGER entry_tags_unread_visible_ad AFTER DELETE ON entry_tags
WHEN OLD.tag_id IN (
    SELECT id FROM tags WHERE name IN ('system:read', 'system:hidden')
)
BEGIN
    UPDATE entries SET unread_visible = NOT EXISTS (
        SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
        WHERE et.entry_id = OLD.entry_id
          AND t.name IN ('system:read', 'system:hidden')
    )
    WHERE id = OLD.entry_id;
END;

CREATE TRIGGER entry_tags_unread_visible_au AFTER UPDATE ON entry_tags
BEGIN
    UPDATE entries SET unread_visible = NOT EXISTS (
        SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
        WHERE et.entry_id = entries.id
          AND t.name IN ('system:read', 'system:hidden')
    )
    WHERE id IN (OLD.entry_id, NEW.entry_id);
END;
