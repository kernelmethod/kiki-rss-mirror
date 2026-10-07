//! Tests for the `filter` plugin shipped in `plugins/filter/`, built from
//! `plugins/filter-src/` by `tools/build-filter-plugin.sh`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use crate::scripting::wasm::WasmScriptRunner;
use crate::scripting::{
    Event, EventPayload, FeedEntry, FeedInfo, ScriptRunner, ScriptServices, ScriptSource,
    ServiceCall, ServiceReply,
};
use crate::test::TestBuilder;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MANIFEST: &str = include_str!("../../plugins/filter/manifest.toml");
const WASM: &[u8] = include_bytes!("../../plugins/filter/plugin.wasm");

/// The filter, loaded with `config`, looking feeds up in `services`.
fn load(
    config: Value,
    services: Option<Arc<dyn ScriptServices>>,
) -> Result<WasmScriptRunner, String> {
    let source = ScriptSource {
        name: "filter".to_string(),
        config: config.to_string(),
        ..ScriptSource::wasm(WASM.to_vec())
    };
    WasmScriptRunner::from_sources_with(&[source], services).map_err(|e| e.to_string())
}

/// The filter, loaded with `config`.
fn filter(config: Value) -> Result<WasmScriptRunner, String> {
    load(config, None)
}

fn entry(feed_id: i64, title: &str) -> FeedEntry {
    FeedEntry {
        id: None,
        feed_id,
        syndication_format: "rss".to_string(),
        guid: title.to_string(),
        published_at: None,
        title: title.to_string(),
        url: Some(format!("https://example.com/{title}")),
        content: Some("<p>body</p>".to_string()),
        authors: vec![],
        categories: vec![],
        tags: vec![],
        cache_assets: true,
    }
}

fn hidden(runner: &WasmScriptRunner, entry: FeedEntry) -> bool {
    tags(runner, entry).iter().any(|t| t == "system:hidden")
}

fn tags(runner: &WasmScriptRunner, entry: FeedEntry) -> Vec<String> {
    runner
        .dispatch_transform_entry(entry)
        .unwrap()
        .unwrap()
        .tags
}

/// Answers `kiki.feeds.get` for feed `n` in 1..=3 with the URL
/// `https://example.com/feed{n}`, counting the lookups.
#[derive(Default)]
struct Feeds {
    lookups: AtomicUsize,
}

impl ScriptServices for Feeds {
    fn call(&self, _plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        match call {
            ServiceCall::GetFeed { feed_id } => {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                Ok(ServiceReply::Feed((1..=3).contains(&feed_id).then(|| {
                    FeedInfo {
                        id: feed_id,
                        url: Some(format!("https://example.com/feed{feed_id}")),
                        title: "f".to_string(),
                    }
                })))
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }
}

/// The filter, loaded with `config`, looking feeds up in `feeds`.
fn filter_with_feeds(config: Value, feeds: &Arc<Feeds>) -> WasmScriptRunner {
    load(config, Some(feeds.clone() as Arc<dyn ScriptServices>)).unwrap()
}

#[test]
fn the_manifest_is_valid_and_has_empty_rules() {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.name, "filter");
    assert_eq!(manifest.config["exclude"], json!([]));
    let runner = filter(Value::Object(manifest.config)).unwrap();
    assert!(!hidden(&runner, entry(1, "anything")));
}

#[test]
fn exclude_rules_hide_matching_entries() {
    let runner = filter(json!({
        "exclude": [
            {"fields": "title", "pattern": r"\bsponsored\b", "flags": "i"},
            {"fields": ["url"], "pattern": "/ads/"},
        ],
    }))
    .unwrap();
    assert!(hidden(&runner, entry(1, "A SPONSORED post")));
    assert!(!hidden(&runner, entry(1, "unsponsored")));
    let mut ad = entry(1, "ok");
    ad.url = Some("https://example.com/ads/1".to_string());
    assert!(hidden(&runner, ad));
    // By default rules match the title and content.
    let runner = filter(json!({"exclude": [{"pattern": "body"}]})).unwrap();
    assert!(hidden(&runner, entry(1, "t")));
}

#[test]
fn include_rules_hide_entries_that_match_none_of_them() {
    let runner = filter(json!({
        "include": [
            {"fields": "title", "pattern": "rust", "feeds": [1]},
            {"fields": "categories", "pattern": "^lang$", "feeds": [1]},
        ],
    }))
    .unwrap();
    assert!(!hidden(&runner, entry(1, "rust news")));
    assert!(hidden(&runner, entry(1, "go news")));
    let mut categorized = entry(1, "go news");
    categorized.categories = vec!["misc".into(), "lang".into()];
    assert!(!hidden(&runner, categorized));
    // Feeds without include rules are left alone.
    assert!(!hidden(&runner, entry(2, "go news")));
}

#[test]
fn exclude_rules_win_over_include_rules() {
    let runner = filter(json!({
        "include": [{"fields": "title", "pattern": "rust"}],
        "exclude": [{"fields": "authors", "pattern": "^Spammer$"}],
    }))
    .unwrap();
    let mut e = entry(1, "rust news");
    e.authors = vec!["Spammer".into()];
    assert!(hidden(&runner, e));
}

/// The manifest describes the rules, so the web UI can edit them, and the
/// config API accepts rules written as the plugin documents them.
#[test]
fn the_manifest_describes_the_rules() {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    let names: Vec<_> = manifest.settings.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["exclude", "include", "rescan"]);
    let config = json!({
        "exclude": [{"fields": ["title"], "pattern": "x", "flags": "i"}],
        "include": [{"pattern": "rust", "feeds": [3, "https://example.com/feed.xml"]}],
        "rescan": false,
    });
    crate::plugins::settings::check_config(&manifest.settings, config.as_object().unwrap())
        .unwrap();
    let runner = filter(config).unwrap();
    assert!(hidden(&runner, entry(1, "x")));
}

/// An empty `fields` list, which the manifest's settings allow, matches
/// the default fields, as the manifest says.
#[test]
fn an_empty_fields_list_means_the_default_fields() {
    let config = json!({"exclude": [{"pattern": "body", "fields": []}]});
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    crate::plugins::settings::check_config(&manifest.settings, config.as_object().unwrap())
        .unwrap();
    let runner = filter(config).unwrap();
    // The entry's content is "<p>body</p>".
    assert!(hidden(&runner, entry(1, "title")));
}

#[test]
fn rules_can_name_feeds_by_url() {
    let feeds = Arc::new(Feeds::default());
    let runner = filter_with_feeds(
        json!({
            "include": [{
                "fields": "title",
                "pattern": "rust",
                "feeds": ["https://example.com/feed1", 2],
            }],
        }),
        &feeds,
    );
    assert!(hidden(&runner, entry(1, "go news")));
    assert!(!hidden(&runner, entry(1, "rust news")));
    assert!(hidden(&runner, entry(2, "go news")));
    assert!(!hidden(&runner, entry(3, "go news")));
    assert!(!hidden(&runner, entry(9, "go news")));
    // Feed 2 is named by id, so only feeds 1, 3 and 9 are looked up, once
    // each.
    assert!(hidden(&runner, entry(1, "go news")));
    assert!(!hidden(&runner, entry(3, "go news")));
    assert_eq!(feeds.lookups.load(Ordering::SeqCst), 3);

    // A removed feed's id may be reused, so it is looked up again.
    runner.dispatch_observe(
        Event::FeedRemoved,
        EventPayload::Feed {
            id: 1,
            url: "https://example.com/feed1".to_string(),
            title: "f".to_string(),
        },
    );
    assert!(hidden(&runner, entry(1, "go news")));
    assert_eq!(feeds.lookups.load(Ordering::SeqCst), 4);
}

#[test]
fn rules_without_feed_urls_look_up_no_feeds() {
    let feeds = Arc::new(Feeds::default());
    let runner = filter_with_feeds(
        json!({"exclude": [{"pattern": "x"}, {"pattern": "y", "feeds": [2]}]}),
        &feeds,
    );
    assert!(hidden(&runner, entry(1, "x")));
    assert!(!hidden(&runner, entry(1, "y")));
    assert_eq!(feeds.lookups.load(Ordering::SeqCst), 0);
}

/// Tagging is left to the auto-tag plugin: `tag` rules, which versions
/// before 3.0.0 took, are ignored rather than failing the plugin.
#[test]
fn tag_rules_are_ignored() {
    let runner = filter(json!({
        "exclude": [{"fields": "title", "pattern": "webinar"}],
        "tag": [
            {"fields": "title", "pattern": "zero-day", "tag": "urgent"},
            // Not even checked.
            {"pattern": "("},
        ],
    }))
    .unwrap();
    assert!(tags(&runner, entry(1, "zero-day in Chrome")).is_empty());
    assert_eq!(
        tags(&runner, entry(1, "zero-day webinar")),
        ["system:hidden"]
    );
    // Tags already there are kept.
    let mut e = entry(1, "zero-day");
    e.tags = vec!["urgent".into()];
    assert_eq!(tags(&runner, e), ["urgent"]);
}

#[test]
fn entries_are_hidden_once() {
    let runner = filter(json!({"exclude": [{"pattern": "x"}]})).unwrap();
    let mut e = entry(1, "x");
    e.tags = vec!["system:hidden".into()];
    let e = runner.dispatch_transform_entry(e).unwrap().unwrap();
    assert_eq!(e.tags, ["system:hidden"]);
}

#[test]
fn bad_rules_fail_to_load() {
    for (config, message) in [
        (
            json!({"exclude": [{"fields": "titel", "pattern": "x"}]}),
            "unknown field",
        ),
        (json!({"exclude": [{"pattern": "("}]}), "exclude[1]"),
        (json!({"exclude": [{"fields": "title"}]}), "'pattern'"),
        (
            json!({"include": [{"pattern": "x", "feeds": [1.5]}]}),
            "feed ids or URLs",
        ),
        (
            json!({"exclude": [{"pattern": "x", "flags": "z"}]}),
            "exclude[1]",
        ),
    ] {
        let err = filter(config.clone()).err().unwrap();
        assert!(err.contains(message), "{config}: {err}");
    }
}

/// Installs the filter into the plugin directory `dir`.
fn install(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("manifest.toml"), MANIFEST)?;
    std::fs::write(dir.join("plugin.wasm"), WASM)?;
    Ok(())
}

/// End to end, through a server: stored entries are filtered when the
/// plugin is installed and whenever its rules change, but not again when
/// they have not changed.
#[tokio::test]
async fn stored_entries_are_filtered_when_the_rules_change() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let ids: Vec<i64> = {
        let conn = tc.database_conn()?;
        conn.execute("INSERT INTO feeds (title) VALUES ('f')", [])?;
        let feed = conn.last_insert_rowid();
        ["keep", "sponsored post", "webinar invite"]
            .iter()
            .map(|title| {
                conn.execute(
                    "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
                     VALUES (?1, 'rss', ?2, 0, ?2, 'u')",
                    rusqlite::params![feed, title],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .collect::<Result<_>>()?
    };
    let dir = tc.user_plugins_dir().join("filter");
    install(&dir)?;
    let overrides = json!({"exclude": [{"fields": ["title"], "pattern": "sponsored"}]});
    crate::db::plugins::set_config_overrides(
        &tc.database_conn()?,
        "filter",
        overrides.as_object().unwrap(),
    )?;

    let db = tc.database_path();
    let is_hidden = |id: i64| -> Result<bool> {
        let conn = crate::db::ConnectionBuilder::default()
            .at_path(&db)
            .read_write()
            .build()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
             WHERE et.entry_id = ?1 AND t.name = 'system:hidden')",
            [id],
            |row| row.get(0),
        )?)
    };
    let wait_until_hidden = |id: i64| async move {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !is_hidden(id)? {
            anyhow::ensure!(Instant::now() < deadline, "entry {id} was never hidden");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    };

    let tc = tc.init_server()?;
    let client = tc.client()?;
    wait_until_hidden(ids[1]).await?;
    assert!(!is_hidden(ids[0])?);
    assert!(!is_hidden(ids[2])?);

    // Unhide the entry, and reload with the same rules: it stays unhidden.
    tc.database_conn()?
        .execute("DELETE FROM entry_tags WHERE entry_id = ?1", [ids[1]])?;
    let url = "http://localhost/v1/plugins/name/filter/config";
    let resp = client
        .patch(url)
        .json(&json!({"rescan": true}))
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    // The plugins were reloaded, and a scan, had one started, would have
    // finished long since.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!is_hidden(ids[1])?);

    // Changing the rules applies them to stored entries.
    let resp = client
        .patch(url)
        .json(&json!({"exclude": [{"fields": ["title"], "pattern": "webinar"}]}))
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    wait_until_hidden(ids[2]).await?;
    assert!(!is_hidden(ids[1])?);
    assert!(!is_hidden(ids[0])?);

    Ok(())
}

/// End to end, through a server: rules recorded by a version before 3.0.0,
/// with tag rules, compare equal to the same rules without them, so
/// upgrading does not rescan and hide again the entries unhid by hand.
#[tokio::test]
async fn dropped_tag_rules_do_not_rescan() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let exclude = json!([{"fields": ["title"], "pattern": "sponsored"}]);
    let tag = json!([{"fields": ["title"], "pattern": "zero-day", "tag": "urgent"}]);
    let id = {
        let conn = tc.database_conn()?;
        conn.execute("INSERT INTO feeds (title) VALUES ('f')", [])?;
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (?1, 'rss', 'g', 0, 'sponsored zero-day', 'u')",
            [conn.last_insert_rowid()],
        )?;
        let id = conn.last_insert_rowid();
        // Left over from version 2: the overrides, and the rules it applied.
        let overrides = json!({"exclude": exclude, "tag": tag});
        crate::db::plugins::set_config_overrides(&conn, "filter", overrides.as_object().unwrap())?;
        let recorded = json!({"exclude": exclude, "include": [], "tag": tag});
        crate::db::plugins::store_set(&conn, "filter", "rules", Some(&recorded))?;
        id
    };
    let dir = tc.user_plugins_dir().join("filter");
    install(&dir)?;

    let tc = tc.init_server()?;
    // A scan, had one started, would have finished long since.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let tagged: i64 = tc.database_conn()?.query_row(
        "SELECT COUNT(*) FROM entry_tags WHERE entry_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    assert_eq!(tagged, 0);
    Ok(())
}
