//! Per-plugin state kept in the database: each plugin's config overrides,
//! and its key-value store.
//!
//! Plugins themselves live on disk (see [`crate::plugins`]); the `plugins`
//! table holds only what Kiki changes about them, keyed by plugin name. A
//! plugin's config is the defaults in its manifest with its overrides
//! applied over them, key by key. A plugin with no row has no overrides.
use crate::plugins::MAX_CONFIG_BYTES;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value};
use std::collections::HashMap;
use thiserror::Error;

/// A plugin's config overrides: the keys that replace its defaults.
pub type ConfigOverrides = Map<String, Value>;

/// Errors raised while reading or writing plugin config overrides.
#[derive(Debug, Error)]
pub enum PluginConfigError {
    /// The overrides are too large to save.
    #[error("config is too large: saved as JSON, it must be at most {limit} bytes")]
    TooLarge { limit: u64 },

    /// The overrides stored for a plugin are not a JSON object.
    #[error("the config stored for plugin {name:?} is invalid")]
    Corrupt {
        name: String,
        #[source]
        source: serde_json::Error,
    },

    /// A database operation failed.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
}

/// Returns the config overrides of the plugin named `name`, which are empty
/// if it has none.
///
/// # Errors
///
/// Returns an error if the database cannot be queried, or if the stored
/// overrides are not a JSON object.
///
/// # Examples
///
/// ```
/// use kiki_rss::db::{plugins, ConnectionBuilder};
/// use serde_json::json;
///
/// let conn = ConnectionBuilder::default().in_memory().create().build().unwrap();
/// assert!(plugins::get_config_overrides(&conn, "hello").unwrap().is_empty());
///
/// let overrides = json!({"greeting": "hi"});
/// plugins::set_config_overrides(&conn, "hello", overrides.as_object().unwrap()).unwrap();
/// assert_eq!(plugins::get_config_overrides(&conn, "hello").unwrap()["greeting"], "hi");
/// ```
pub fn get_config_overrides(
    conn: &Connection,
    name: &str,
) -> Result<ConfigOverrides, PluginConfigError> {
    let text: Option<String> = conn
        .query_row(
            "SELECT config FROM plugins WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    match text {
        Some(text) => parse(name, &text),
        None => Ok(Map::new()),
    }
}

/// Returns the config overrides of every plugin that has any, keyed by
/// plugin name. Plugins that are no longer installed are included.
///
/// # Errors
///
/// Returns an error if the database cannot be queried, or if any stored
/// overrides are not a JSON object.
pub fn all_config_overrides(
    conn: &Connection,
) -> Result<HashMap<String, ConfigOverrides>, PluginConfigError> {
    let mut stmt = conn.prepare("SELECT name, config FROM plugins")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut all = HashMap::new();
    for row in rows {
        let (name, text) = row?;
        let overrides = parse(&name, &text)?;
        all.insert(name, overrides);
    }
    Ok(all)
}

/// Replaces the config overrides of the plugin named `name` with
/// `overrides`. Empty overrides remove the plugin's row, restoring its
/// defaults.
///
/// The plugin need not be installed: overrides are kept by name, and apply
/// whenever a plugin of that name is.
///
/// # Errors
///
/// Returns [`PluginConfigError::TooLarge`] if the overrides serialize to
/// more than [`MAX_CONFIG_BYTES`], and an error if the database cannot be
/// written.
pub fn set_config_overrides(
    conn: &Connection,
    name: &str,
    overrides: &ConfigOverrides,
) -> Result<(), PluginConfigError> {
    if overrides.is_empty() {
        conn.execute("DELETE FROM plugins WHERE name = ?1", [name])?;
        return Ok(());
    }

    let text = Value::Object(overrides.clone()).to_string();
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(PluginConfigError::TooLarge {
            limit: MAX_CONFIG_BYTES,
        });
    }
    conn.execute(
        "INSERT INTO plugins (name, config) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE
         SET config = excluded.config, updated_at = unixepoch()",
        [name, &text],
    )?;
    Ok(())
}

/// Applies `edit` to the config overrides of the plugin named `name` and
/// saves the result, which it returns.
///
/// The read and the write happen in one immediate transaction, so
/// concurrent edits do not lose each other's changes.
///
/// # Errors
///
/// Returns an error, and changes nothing, if the stored overrides cannot be
/// read, or if the result cannot be saved (see [`set_config_overrides`]).
///
/// # Examples
///
/// ```
/// use kiki_rss::db::{plugins, ConnectionBuilder};
/// use serde_json::json;
///
/// let mut conn = ConnectionBuilder::default().in_memory().create().build().unwrap();
/// plugins::update_config_overrides(&mut conn, "hello", |o| {
///     o.insert("a".to_string(), json!(1));
/// }).unwrap();
/// let overrides = plugins::update_config_overrides(&mut conn, "hello", |o| {
///     o.insert("b".to_string(), json!(2));
/// }).unwrap();
/// assert_eq!(overrides, *json!({"a": 1, "b": 2}).as_object().unwrap());
/// ```
pub fn update_config_overrides<F>(
    conn: &mut Connection,
    name: &str,
    edit: F,
) -> Result<ConfigOverrides, PluginConfigError>
where
    F: FnOnce(&mut ConfigOverrides),
{
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut overrides = get_config_overrides(&tx, name)?;
    edit(&mut overrides);
    set_config_overrides(&tx, name, &overrides)?;
    tx.commit()?;
    Ok(overrides)
}

/// Longest key, in bytes, a plugin's store accepts.
pub const MAX_STORE_KEY_BYTES: usize = 256;

/// Largest value, serialized as JSON, a plugin's store accepts.
pub const MAX_STORE_VALUE_BYTES: usize = 64 * 1024;

/// Most keys one plugin's store may hold.
pub const MAX_STORE_KEYS: usize = 1024;

/// Errors raised while reading or writing a plugin's store.
#[derive(Debug, Error)]
pub enum PluginStoreError {
    /// The key is empty or longer than [`MAX_STORE_KEY_BYTES`].
    #[error("store keys must be between 1 and {MAX_STORE_KEY_BYTES} bytes long")]
    InvalidKey,

    /// The value is larger than [`MAX_STORE_VALUE_BYTES`].
    #[error("store values must be at most {MAX_STORE_VALUE_BYTES} bytes as JSON")]
    ValueTooLarge,

    /// The store already holds [`MAX_STORE_KEYS`] keys.
    #[error("a plugin's store may hold at most {MAX_STORE_KEYS} keys")]
    Full,

    /// A stored value is not valid JSON.
    #[error("the stored value is invalid: {0}")]
    Corrupt(#[from] serde_json::Error),

    /// A database operation failed.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
}

/// Returns the value stored under `key` by the plugin named `plugin`, if
/// there is one.
///
/// # Errors
///
/// Returns an error if the database cannot be queried or the stored value
/// is not valid JSON.
///
/// # Examples
///
/// ```
/// use kiki_rss::db::{plugins, ConnectionBuilder};
/// use serde_json::json;
///
/// let conn = ConnectionBuilder::default().in_memory().create().build().unwrap();
/// assert_eq!(plugins::store_get(&conn, "p", "k").unwrap(), None);
/// plugins::store_set(&conn, "p", "k", Some(&json!({"n": 1}))).unwrap();
/// assert_eq!(plugins::store_get(&conn, "p", "k").unwrap(), Some(json!({"n": 1})));
/// plugins::store_set(&conn, "p", "k", None).unwrap();
/// assert_eq!(plugins::store_get(&conn, "p", "k").unwrap(), None);
/// ```
pub fn store_get(
    conn: &Connection,
    plugin: &str,
    key: &str,
) -> Result<Option<Value>, PluginStoreError> {
    let text: Option<String> = conn
        .query_row(
            "SELECT value FROM plugin_store WHERE plugin = ?1 AND key = ?2",
            [plugin, key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(text.map(|t| serde_json::from_str(&t)).transpose()?)
}

/// Stores `value` under `key` for the plugin named `plugin`, or removes the
/// key when `value` is `None`.
///
/// # Errors
///
/// Returns an error if the key or value is too large, if the plugin's
/// store is full, or if the database cannot be written.
pub fn store_set(
    conn: &Connection,
    plugin: &str,
    key: &str,
    value: Option<&Value>,
) -> Result<(), PluginStoreError> {
    if key.is_empty() || key.len() > MAX_STORE_KEY_BYTES {
        return Err(PluginStoreError::InvalidKey);
    }
    let Some(value) = value else {
        conn.execute(
            "DELETE FROM plugin_store WHERE plugin = ?1 AND key = ?2",
            [plugin, key],
        )?;
        return Ok(());
    };
    let text = value.to_string();
    if text.len() > MAX_STORE_VALUE_BYTES {
        return Err(PluginStoreError::ValueTooLarge);
    }
    let (keys, exists): (i64, bool) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(key = ?2), 0) > 0 FROM plugin_store WHERE plugin = ?1",
        [plugin, key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if !exists && keys as usize >= MAX_STORE_KEYS {
        return Err(PluginStoreError::Full);
    }
    conn.execute(
        "INSERT INTO plugin_store (plugin, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(plugin, key) DO UPDATE
         SET value = excluded.value, updated_at = unixepoch()",
        [plugin, key, &text],
    )?;
    Ok(())
}

fn parse(name: &str, text: &str) -> Result<ConfigOverrides, PluginConfigError> {
    serde_json::from_str(text).map_err(|source| PluginConfigError::Corrupt {
        name: name.to_string(),
        source,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;
    use serde_json::json;

    fn conn() -> Connection {
        ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()
            .unwrap()
    }

    fn map(v: Value) -> ConfigOverrides {
        v.as_object().unwrap().clone()
    }

    fn row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM plugins", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn overrides_round_trip() {
        let conn = conn();
        let overrides = map(json!({"a": null, "b": [1, {"c": "d"}]}));
        set_config_overrides(&conn, "p", &overrides).unwrap();
        assert_eq!(get_config_overrides(&conn, "p").unwrap(), overrides);
        assert!(get_config_overrides(&conn, "other").unwrap().is_empty());

        // Replacing drops keys left out.
        set_config_overrides(&conn, "p", &map(json!({"x": 1}))).unwrap();
        assert_eq!(
            get_config_overrides(&conn, "p").unwrap(),
            map(json!({"x": 1}))
        );
    }

    #[test]
    fn empty_overrides_remove_the_row() {
        let conn = conn();
        set_config_overrides(&conn, "p", &map(json!({"a": 1}))).unwrap();
        assert_eq!(row_count(&conn), 1);
        set_config_overrides(&conn, "p", &Map::new()).unwrap();
        assert_eq!(row_count(&conn), 0);
    }

    #[test]
    fn all_config_overrides_lists_every_plugin() {
        let conn = conn();
        set_config_overrides(&conn, "p", &map(json!({"a": 1}))).unwrap();
        set_config_overrides(&conn, "q", &map(json!({"b": 2}))).unwrap();
        let all = all_config_overrides(&conn).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["q"], map(json!({"b": 2})));
    }

    #[test]
    fn oversized_overrides_are_rejected() {
        let conn = conn();
        let big = "x".repeat(MAX_CONFIG_BYTES as usize);
        assert!(matches!(
            set_config_overrides(&conn, "p", &map(json!({"a": big}))),
            Err(PluginConfigError::TooLarge { .. })
        ));
        assert_eq!(row_count(&conn), 0);
    }

    #[test]
    fn update_keeps_other_keys() {
        let mut conn = conn();
        set_config_overrides(&conn, "p", &map(json!({"a": 1, "b": 2}))).unwrap();
        let overrides = update_config_overrides(&mut conn, "p", |o| {
            o.remove("a");
            o.insert("c".to_string(), json!(3));
        })
        .unwrap();
        assert_eq!(overrides, map(json!({"b": 2, "c": 3})));
        assert_eq!(get_config_overrides(&conn, "p").unwrap(), overrides);
    }

    #[test]
    fn only_json_objects_can_be_stored() {
        let conn = conn();
        for bad in ["[1, 2]", "not json", "3"] {
            assert!(
                conn.execute("INSERT INTO plugins (name, config) VALUES ('p', ?1)", [bad])
                    .is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn stores_are_kept_per_plugin() {
        let conn = conn();
        store_set(&conn, "p", "k", Some(&json!("a"))).unwrap();
        store_set(&conn, "q", "k", Some(&json!("b"))).unwrap();
        store_set(&conn, "p", "k", Some(&json!("c"))).unwrap();
        assert_eq!(store_get(&conn, "p", "k").unwrap(), Some(json!("c")));
        assert_eq!(store_get(&conn, "q", "k").unwrap(), Some(json!("b")));
    }

    #[test]
    fn stores_are_bounded() {
        let conn = conn();
        assert!(matches!(
            store_set(&conn, "p", "", Some(&json!(1))),
            Err(PluginStoreError::InvalidKey)
        ));
        let big = json!("x".repeat(MAX_STORE_VALUE_BYTES));
        assert!(matches!(
            store_set(&conn, "p", "k", Some(&big)),
            Err(PluginStoreError::ValueTooLarge)
        ));
        for i in 0..MAX_STORE_KEYS {
            store_set(&conn, "p", &i.to_string(), Some(&json!(i))).unwrap();
        }
        assert!(matches!(
            store_set(&conn, "p", "one-more", Some(&json!(1))),
            Err(PluginStoreError::Full)
        ));
        // Overwriting a key that exists is still allowed.
        store_set(&conn, "p", "0", Some(&json!("new"))).unwrap();
    }
}
