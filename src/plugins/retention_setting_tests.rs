//! Tests for moving the config file's retired `[retention]` setting into the
//! `retention` plugin's config.

#![allow(clippy::unwrap_used)]

use super::migrate_retention_setting;
use crate::config::ConfigStore;
use crate::db::Db;
use serde_json::{json, Value};

/// A database in a temporary directory, which is kept for the rest of the
/// test process.
fn db() -> (Db, std::path::PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("kiki.db");
    crate::db::ConnectionBuilder::default()
        .at_path(&path)
        .create()
        .build()
        .unwrap();
    let dir = td.keep();
    (Db::open(&path, Default::default()).unwrap(), dir)
}

/// A config file in a temporary directory holding `text`, and a database.
fn config_and_db(text: &str) -> (ConfigStore, Db) {
    let (db, dir) = db();
    let path = dir.join("kiki.toml");
    std::fs::write(&path, text).unwrap();
    (ConfigStore::open(&path).unwrap(), db)
}

fn retention_overrides(db: &Db) -> serde_json::Map<String, Value> {
    db.read_blocking(|conn| crate::db::plugins::get_config_overrides(conn, "retention"))
        .unwrap()
        .unwrap()
}

#[test]
fn the_retired_setting_moves_to_the_plugin() {
    let (config, db) =
        config_and_db("[retention]\nmax_age_days = 30\n\n[asset_cache]\nenabled = false\n");
    assert!(migrate_retention_setting(&config, &db).unwrap());
    assert_eq!(
        retention_overrides(&db),
        *json!({"max_age_days": 30}).as_object().unwrap()
    );

    let text = std::fs::read_to_string(config.path()).unwrap();
    assert!(!text.contains("retention"), "{text}");
    assert!(!config.current().asset_cache.enabled);
    // There is nothing left to move.
    assert!(!migrate_retention_setting(&config, &db).unwrap());
}

#[test]
fn the_plugins_own_setting_is_kept() {
    let (config, db) = config_and_db("[retention]\nmax_age_days = 30\n");
    db.write_blocking(|conn| {
        let overrides = json!({"max_age_days": 7, "keep_tags": []});
        crate::db::plugins::set_config_overrides(conn, "retention", overrides.as_object().unwrap())
    })
    .unwrap()
    .unwrap();
    assert!(migrate_retention_setting(&config, &db).unwrap());
    assert_eq!(retention_overrides(&db)["max_age_days"], 7);
    assert!(!std::fs::read_to_string(config.path())
        .unwrap()
        .contains("retention"));
}

#[test]
fn invalid_retired_settings_are_dropped() {
    for text in [
        "[retention]\nmax_age_days = 0\n",
        "[retention]\nmax_age_days = \"30\"\n",
    ] {
        let (config, db) = config_and_db(text);
        assert!(migrate_retention_setting(&config, &db).unwrap(), "{text}");
        assert!(retention_overrides(&db).is_empty(), "{text}");
        assert!(!std::fs::read_to_string(config.path())
            .unwrap()
            .contains("retention"));
    }
}

#[test]
fn nothing_is_moved_without_the_setting() {
    let (config, db) = config_and_db("[asset_cache]\nenabled = false\n");
    let before = std::fs::read_to_string(config.path()).unwrap();
    assert!(!migrate_retention_setting(&config, &db).unwrap());
    assert!(retention_overrides(&db).is_empty());
    assert_eq!(std::fs::read_to_string(config.path()).unwrap(), before);
}
