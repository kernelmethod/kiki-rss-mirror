-- Each plugin's key-value store, which its code reads and writes with
-- `kiki.store.get` and `kiki.store.set`. Keyed by plugin name, like the
-- `plugins` table, so a plugin's data survives it being upgraded. `value`
-- is JSON.
CREATE TABLE plugin_store (
    plugin      VARCHAR NOT NULL,
    key         VARCHAR NOT NULL,
    value       VARCHAR NOT NULL CHECK (json_valid(value)),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (plugin, key)
);
