-- Adaptive fetching moved out of the server into the adaptive-fetch plugin
-- (plugins/adaptive-fetch). Each feed's level goes to the plugin's store,
-- under the key the plugin reads it from, and the feeds it was turned off
-- for go to the plugin's `exclude` setting. A feed turned on for itself
-- needs nothing: the plugin backs off from every feed unless told
-- otherwise.
INSERT INTO plugin_store (plugin, key, value)
SELECT 'adaptive-fetch', 'level:' || id, CAST(adaptive_fetch_level AS TEXT)
FROM feeds
WHERE adaptive_fetch_level > 0 AND adaptive_fetch IS NOT 0
ON CONFLICT (plugin, key) DO UPDATE SET value = excluded.value;

INSERT INTO plugins (name, config)
SELECT 'adaptive-fetch', json_object('exclude', json_group_array(id))
FROM feeds
WHERE adaptive_fetch = 0
HAVING count(*) > 0
ON CONFLICT (name) DO UPDATE SET
    config = json_set(config, '$.exclude', json(json_extract(excluded.config, '$.exclude'))),
    updated_at = unixepoch();

ALTER TABLE feeds DROP COLUMN adaptive_fetch;
ALTER TABLE feeds DROP COLUMN adaptive_fetch_level;
