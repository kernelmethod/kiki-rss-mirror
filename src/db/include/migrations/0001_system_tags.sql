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
