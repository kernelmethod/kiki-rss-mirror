//! Exporting the scripts that older versions of Kiki kept in the database.
use super::{install, PluginEngine, PluginManifest, PLUGINS_DIR_NAME};
use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Exports every script in the database's legacy `scripts` table to a
/// plugin in `plugins_dir`, returning the plugin directories created.
///
/// Each script becomes a plugin named `script-NNNN` after its ID, with
/// version `0.0.0`, its text as its entrypoint and its stored config as the
/// default config in its manifest. The zero-padded IDs keep the plugins
/// loading in the order the scripts used to run in.
///
/// Does nothing if the database has no `scripts` table.
///
/// # Errors
///
/// Returns an error if the table cannot be read, a script uses an engine
/// other than Lua or has an invalid config, or a plugin cannot be written —
/// including because its directory already exists with a different
/// entrypoint. (A directory holding an earlier export of the same script is
/// kept, so that an interrupted migration can be run again.)
pub fn export_legacy_scripts(conn: &Connection, plugins_dir: &Path) -> Result<Vec<PathBuf>> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'scripts')",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        return Ok(Vec::new());
    }

    let has_config: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('scripts') WHERE name = 'config')",
        [],
        |row| row.get(0),
    )?;
    let query = if has_config {
        "SELECT id, engine, text, config FROM scripts ORDER BY id"
    } else {
        "SELECT id, engine, text, '{}' FROM scripts ORDER BY id"
    };

    let mut stmt = conn.prepare(query)?;
    let scripts = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("unable to read scripts from the database")?;

    let mut exported = Vec::with_capacity(scripts.len());
    for (id, engine, text, config) in scripts {
        if engine != PluginEngine::Lua.name() {
            bail!("script {id} uses engine {engine:?}, which Kiki cannot export as a plugin");
        }
        let config = crate::scripting::parse_script_config(&config)
            .with_context(|| format!("script {id} has an invalid config"))?;
        let manifest = PluginManifest {
            name: format!("script-{id:04}"),
            version: "0.0.0".to_string(),
            engine: PluginEngine::Lua,
            entrypoint: None,
            description: Some(format!("Script {id}, exported from the database")),
            authors: vec![],
            license: None,
            homepage: None,
            enabled: true,
            config,
        };
        // An earlier run may have exported the script before failing to
        // migrate; its export can be kept as long as it is unchanged.
        let existing = plugins_dir.join(&manifest.name);
        if std::fs::read_to_string(existing.join(manifest.entrypoint())).is_ok_and(|t| t == text) {
            exported.push(existing);
            continue;
        }
        let dir = install(plugins_dir, &manifest, &text)
            .with_context(|| format!("unable to export script {id} to {PLUGINS_DIR_NAME}/"))?;
        exported.push(dir);
    }
    Ok(exported)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::plugins::discover;
    use tempfile::TempDir;

    #[test]
    fn scripts_become_plugins() -> Result<()> {
        let td = TempDir::new()?;
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE scripts (
                id INTEGER PRIMARY KEY, engine VARCHAR, text VARCHAR, kind VARCHAR,
                config VARCHAR NOT NULL DEFAULT '{}'
            );
            INSERT INTO scripts (id, engine, text, kind, config) VALUES
                (2, 'lua', '-- two', 'user', '{\"a\": 1}'),
                (10, 'lua', '-- ten', 'system', '{}');",
        )?;

        let exported = export_legacy_scripts(&conn, td.path())?;
        assert_eq!(exported.len(), 2);
        // Exporting again keeps the earlier export.
        assert_eq!(export_legacy_scripts(&conn, td.path())?, exported);

        let found = discover(td.path())?;
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        let plugins: Vec<_> = found
            .plugins
            .iter()
            .map(|p| {
                let source = p.load_source().unwrap();
                (p.manifest.name.clone(), source.text, source.config)
            })
            .collect();
        assert_eq!(
            plugins,
            [
                ("script-0002".into(), "-- two".into(), r#"{"a":1}"#.into()),
                ("script-0010".into(), "-- ten".into(), "{}".into()),
            ] as [(String, String, String); 2]
        );
        Ok(())
    }

    #[test]
    fn a_database_without_scripts_exports_nothing() -> Result<()> {
        let td = TempDir::new()?;
        let conn = Connection::open_in_memory()?;
        assert!(export_legacy_scripts(&conn, td.path())?.is_empty());
        assert!(!td.path().join("script-0001").exists());
        Ok(())
    }

    #[test]
    fn scripts_from_before_configs_are_exported() -> Result<()> {
        let td = TempDir::new()?;
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE scripts (id INTEGER PRIMARY KEY, engine VARCHAR, text VARCHAR, kind VARCHAR);
             INSERT INTO scripts (id, engine, text, kind) VALUES (1, 'lua', '-- one', 'user');",
        )?;
        assert_eq!(export_legacy_scripts(&conn, td.path())?.len(), 1);
        Ok(())
    }
}
