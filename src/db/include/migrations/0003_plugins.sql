-- Scripts are now plugins: directories in Kiki's home with a manifest,
-- discovered on disk rather than stored in the database, so the tables
-- scripts were kept in are dropped. See src/plugins/mod.rs.
DROP TABLE IF EXISTS feed_scripts;
DROP INDEX IF EXISTS idx_scripts_kind;
DROP TABLE IF EXISTS scripts;
