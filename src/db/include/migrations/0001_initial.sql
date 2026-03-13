-- Migration 0001_initial
-- Transition from schema_version tracking to migrations tracking.
-- For existing databases: drops the old schema_version table.
-- For fresh databases: this is a no-op since init.sql already created
-- the migrations table and schema_version doesn't exist.
DROP TABLE IF EXISTS schema_version;
