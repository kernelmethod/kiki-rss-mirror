-- Per-script configuration: a JSON object handed to the script's top-level
-- chunk as its argument. See src/docs/scripting.md.
ALTER TABLE scripts ADD COLUMN config VARCHAR NOT NULL DEFAULT '{}';
