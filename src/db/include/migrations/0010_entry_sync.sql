-- Support for clients that sync entries incrementally. Keep in sync with
-- src/db/include/init.sql.
--
-- `ingested_at` records when Kiki first stored each entry, as a Unix
-- timestamp. It cannot default to the current time on a column added to an
-- existing table, so the entry insert sets it explicitly; entries stored
-- before this migration get their publication time (or now, if that is in
-- the future) as the closest estimate available.
ALTER TABLE entries ADD COLUMN ingested_at INTEGER NOT NULL DEFAULT 0;
UPDATE entries SET ingested_at = MIN(published_at, unixepoch());
CREATE INDEX idx_entry_ingested_at ON entries(ingested_at);

-- The highest entry id ever used. Without AUTOINCREMENT, which cannot be
-- added to an existing table, SQLite reuses the id of the newest entry once
-- it is deleted, and clients that sync by id would take a new entry for one
-- they had already seen. New entries take the next id after this instead;
-- see `upsert_entry` in src/tasks/processing.rs.
CREATE TABLE entry_id_high_water (id INTEGER NOT NULL);
INSERT INTO entry_id_high_water (id) SELECT COALESCE(MAX(id), 0) FROM entries;
CREATE TRIGGER entries_id_high_water AFTER INSERT ON entries
WHEN NEW.id > (SELECT id FROM entry_id_high_water)
BEGIN
    UPDATE entry_id_high_water SET id = NEW.id;
END;
