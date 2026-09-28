-- Per-plugin state kept by Kiki. Plugins themselves live on disk (see
-- src/plugins/mod.rs); this table holds what the API changes about them,
-- keyed by plugin name so that it survives a plugin being upgraded or its
-- directory renamed.
--
-- `config` holds the plugin's config overrides, a JSON object whose keys
-- replace those of the defaults in the plugin's manifest.
CREATE TABLE plugins (
    name        VARCHAR PRIMARY KEY NOT NULL,
    config      VARCHAR NOT NULL DEFAULT '{}'
                CHECK (json_valid(config) AND json_type(config) = 'object'),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
