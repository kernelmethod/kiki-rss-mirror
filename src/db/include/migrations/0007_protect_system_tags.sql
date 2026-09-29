-- System tags cannot be deleted, whichever way the deletion is attempted:
-- through the API, a plugin, or SQL run directly against the database.
-- Keep in sync with the trigger in src/db/include/init.sql.
CREATE TRIGGER protect_system_tags
BEFORE DELETE ON tags
WHEN OLD.kind = 'system'
BEGIN
    SELECT RAISE(ABORT, 'system tags cannot be deleted');
END;
