//! Tests for the `retention` plugin shipped in `plugins/retention/`, and for
//! moving the config file's retired `[retention]` setting into its config.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use super::runtime::PluginRuntime;
use super::{migrate_retention_setting, Permission, PluginManifest, PluginSource};
use crate::config::ConfigStore;
use crate::db::Db;
use crate::scripting::lua::LuaScriptRunner;
use crate::scripting::{
    DeleteFilter, Event, EventPayload, ScriptRunner, ScriptRunnerHandle, ScriptSource, ServiceCall,
    ServiceReply,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const MANIFEST: &str = include_str!("../../plugins/retention/manifest.toml");
const MAIN: &str = include_str!("../../plugins/retention/main.lua");

const DAY: i64 = 86_400;

/// Records the deletions the plugin asks for, deleting nothing.
#[derive(Default)]
struct FakeServices {
    deletes: Mutex<Vec<DeleteFilter>>,
}

impl crate::scripting::ScriptServices for FakeServices {
    fn call(&self, plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        assert_eq!(plugin, "retention");
        match call {
            ServiceCall::DeleteEntries { filter } => {
                self.deletes.lock().unwrap().push(filter);
                Ok(ServiceReply::Deleted(0))
            }
            other => Err(format!("unexpected call {other:?}")),
        }
    }
}

/// The plugin's defaults, with `overrides` applied.
fn config(overrides: Value) -> Value {
    let mut config = PluginManifest::parse(MANIFEST).unwrap().config;
    config.extend(overrides.as_object().unwrap().clone());
    Value::Object(config)
}

/// The plugin, loaded with `overrides`, answering its calls with `services`.
fn plugin(
    overrides: Value,
    services: Arc<FakeServices>,
) -> Result<LuaScriptRunner, crate::scripting::lua::ScriptError> {
    let mut source = ScriptSource::new(MAIN);
    source.name = "retention".to_string();
    source.config = config(overrides).to_string();
    LuaScriptRunner::from_sources_with(&[source], Some(services))
}

#[test]
fn the_manifest_asks_for_the_delete_permission() {
    let manifest = PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.permissions, [Permission::EntriesDelete]);
    assert_eq!(manifest.config["max_age_days"], 0);
    crate::plugins::settings::check_config(&manifest.settings, &manifest.config).unwrap();
}

#[test]
fn by_default_nothing_is_deleted() {
    let services = Arc::new(FakeServices::default());
    let runner = plugin(json!({}), services.clone()).unwrap();
    assert!(!runner.handles(Event::PluginLoad));
    assert!(!runner.handles(Event::Timer));
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    assert!(services.deletes.lock().unwrap().is_empty());
}

#[test]
fn entries_dropped_long_enough_ago_are_deleted_on_load_and_then_hourly() {
    let services = Arc::new(FakeServices::default());
    let runner = plugin(json!({"max_age_days": 30}), services.clone()).unwrap();
    assert!(runner.handles(Event::Timer));

    let now = chrono::Utc::now().timestamp();
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    let deletes = services.deletes.lock().unwrap().clone();
    assert_eq!(deletes.len(), 1);
    let cutoff = now - 30 * DAY;
    assert!((cutoff - 5..=cutoff + 5).contains(&deletes[0].dropped_before));
    assert_eq!(
        deletes[0],
        DeleteFilter {
            dropped_before: deletes[0].dropped_before,
            feed_id: None,
            published_before: None,
            keep_tagged: vec!["system:saved".into()],
        }
    );
}

#[test]
fn entries_with_the_tags_it_is_given_are_kept() {
    for (keep_tags, keep_tagged) in [
        (json!([]), vec![]),
        (
            json!(["keep", "system:saved"]),
            vec!["keep", "system:saved"],
        ),
        // A config without `keep_tags` keeps saved entries.
        (Value::Null, vec!["system:saved"]),
    ] {
        let services = Arc::new(FakeServices::default());
        let runner = plugin(
            json!({"max_age_days": 30, "keep_tags": keep_tags}),
            services.clone(),
        )
        .unwrap();
        runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
        assert_eq!(
            services.deletes.lock().unwrap()[0].keep_tagged,
            keep_tagged,
            "{keep_tags}"
        );
    }
}

#[test]
fn bad_configs_fail_to_load() {
    for overrides in [
        json!({"max_age_days": -1}),
        json!({"max_age_days": 1.5}),
        json!({"max_age_days": "30"}),
        json!({"max_age_days": 36501}),
        json!({"keep_tags": "system:saved"}),
        json!({"keep_tags": [1]}),
        json!({"keep_tags": [""]}),
        json!({"keep_tags": {"tag": "keep"}}),
    ] {
        let result = plugin(overrides.clone(), Arc::new(FakeServices::default()));
        assert!(result.is_err(), "{overrides} should fail to load");
    }
}

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

/// Copies the bundled retention plugin into the system plugins directory
/// inside `plugins_dir`.
fn install_retention(plugins_dir: &Path) {
    let dir = PluginSource::System.dir(plugins_dir).join("retention");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("main.lua"), MAIN).unwrap();
}

/// Stores an entry of feed `feed_id`, dropped from it `days` days ago, or
/// still in it when `days` is `None`, and returns its id.
fn insert_entry(db: &Db, feed_id: i64, guid: &str, days: Option<i64>) -> i64 {
    db.write_blocking(|conn| {
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url,
                                  dropped_at)
             VALUES (?1, 'rss', ?2, 0, ?2, 'u', unixepoch() - ?3 * 86400)",
            rusqlite::params![feed_id, guid, days],
        )
        .unwrap();
        conn.last_insert_rowid()
    })
    .unwrap()
}

fn guids(db: &Db) -> Vec<String> {
    db.read_blocking(|conn| {
        conn.prepare("SELECT guid FROM entries ORDER BY guid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    })
    .unwrap()
}

/// The plugin, installed and loaded as the server loads it, deletes stored
/// entries with the permission its manifest asks for, while a plugin that
/// does not ask for it cannot.
#[test]
fn the_installed_plugin_deletes_old_entries() {
    let (db, dir) = db();
    let plugins_dir = dir.join("plugins");
    install_retention(&plugins_dir);
    let sneaky = PluginManifest {
        permissions: vec![],
        ..PluginManifest::parse("name = 'sneaky'\nversion = '1.0.0'\nengine = 'lua'\n").unwrap()
    };
    super::install(
        &PluginSource::User.dir(&plugins_dir),
        &sneaky,
        r#"kiki.on("plugin.load", function()
            kiki.entries.delete_where { dropped_before = os.time() + 86400 }
        end)"#,
    )
    .unwrap();

    let feed_id = db
        .write_blocking(|conn| {
            conn.execute("INSERT INTO feeds (title) VALUES ('f')", [])
                .unwrap();
            conn.last_insert_rowid()
        })
        .unwrap();
    insert_entry(&db, feed_id, "current", None);
    insert_entry(&db, feed_id, "recent", Some(1));
    insert_entry(&db, feed_id, "old", Some(10));
    db.write_blocking(|conn| {
        let overrides = json!({"max_age_days": 7});
        crate::db::plugins::set_config_overrides(conn, "retention", overrides.as_object().unwrap())
    })
    .unwrap()
    .unwrap();

    let runtime = PluginRuntime::start(
        plugins_dir,
        db.clone(),
        Arc::new(crate::metrics::Metrics::new().unwrap()),
        ScriptRunnerHandle::empty(),
        None,
        CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(runtime.current().plugins.len(), 2);
    // `sneaky` was refused, or "recent" would be gone too.
    assert_eq!(guids(&db), ["current", "recent"]);
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
