//! Tests for the `auto-tag` plugin shipped in `plugins/auto-tag/`, built from
//! the crate in that directory by `build.rs`.

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

const MANIFEST: &str = include_str!("../../plugins/auto-tag/manifest.toml");
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/auto-tag.wasm"));

fn source(config: &Value) -> ScriptSource {
    ScriptSource {
        name: "auto-tag".to_string(),
        config: config.to_string(),
        ..ScriptSource::new(WASM.to_vec())
    }
}

/// The plugin, loaded with `config`.
fn auto_tag(config: Value) -> Result<WasmScriptRunner, String> {
    WasmScriptRunner::from_sources_with(&[source(&config)], None).map_err(|e| e.to_string())
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

fn tags(runner: &WasmScriptRunner, entry: FeedEntry) -> Vec<String> {
    runner
        .dispatch_transform_entry(entry)
        .unwrap()
        .unwrap()
        .tags
}

/// Answers `get-feed` for feed `n` in 1..=3 with the URL
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

/// The plugin, loaded with `config`, looking feeds up in `feeds`.
fn auto_tag_with_feeds(config: Value, feeds: &Arc<Feeds>) -> WasmScriptRunner {
    WasmScriptRunner::from_sources_with(
        &[source(&config)],
        Some(feeds.clone() as Arc<dyn ScriptServices>),
    )
    .unwrap()
}

#[test]
fn the_manifest_is_valid_and_has_no_rules() {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.name, "auto-tag");
    assert_eq!(manifest.config["rules"], json!([]));
    let runner = auto_tag(Value::Object(manifest.config)).unwrap();
    assert!(tags(&runner, entry(1, "anything")).is_empty());
}

/// The manifest describes the rules, so the web UI can edit them, and the
/// config API accepts rules written as the plugin documents them.
#[test]
fn the_manifest_describes_the_rules() {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    let names: Vec<_> = manifest.settings.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["rules", "rescan"]);
    let config = json!({
        "rules": [
            {"tag": "security", "feeds": [1, "https://example.com/feed.xml"]},
            {"tag": "rust", "fields": ["title"], "pattern": "rust", "flags": "i"},
            {"tag": "both", "pattern": "x", "feeds": [2], "fields": []},
        ],
        "rescan": false,
    });
    crate::plugins::settings::check_config(&manifest.settings, config.as_object().unwrap())
        .unwrap();
    let runner = auto_tag(config).unwrap();
    assert_eq!(tags(&runner, entry(1, "RUST")), ["security", "rust"]);

    // A rule without a tag is rejected by the settings.
    let config = json!({"rules": [{"pattern": "x"}]});
    assert!(crate::plugins::settings::check_config(
        &manifest.settings,
        config.as_object().unwrap()
    )
    .is_err());
}

#[test]
fn pattern_rules_tag_matching_entries() {
    let runner = auto_tag(json!({
        "rules": [
            {"fields": "title", "pattern": r"zero-day|in the wild", "flags": "i", "tag": "urgent"},
            {"fields": ["categories"], "pattern": "^rust$", "tag": "rust"},
        ],
    }))
    .unwrap();
    assert_eq!(tags(&runner, entry(1, "Zero-Day in Chrome")), ["urgent"]);
    assert!(tags(&runner, entry(1, "quiet news")).is_empty());
    let mut e = entry(2, "exploited in the wild");
    e.categories = vec!["security".into(), "rust".into()];
    assert_eq!(tags(&runner, e), ["urgent", "rust"]);

    // By default patterns match the title and content.
    let runner = auto_tag(json!({"rules": [{"pattern": "body", "tag": "t"}]})).unwrap();
    assert_eq!(tags(&runner, entry(1, "title")), ["t"]);
}

#[test]
fn feed_rules_tag_every_entry_from_their_feeds() {
    let feeds = Arc::new(Feeds::default());
    let runner = auto_tag_with_feeds(
        json!({
            "rules": [
                {"tag": "news", "feeds": [1, "https://example.com/feed3"]},
                // An empty pattern is no pattern.
                {"tag": "two", "feeds": [2], "pattern": ""},
            ],
        }),
        &feeds,
    );
    assert_eq!(tags(&runner, entry(1, "anything")), ["news"]);
    assert_eq!(tags(&runner, entry(2, "anything")), ["two"]);
    assert_eq!(tags(&runner, entry(3, "anything")), ["news"]);
    assert!(tags(&runner, entry(9, "anything")).is_empty());
}

#[test]
fn rules_with_a_pattern_and_feeds_need_both_to_match() {
    let feeds = Arc::new(Feeds::default());
    let runner = auto_tag_with_feeds(
        json!({"rules": [{
            "tag": "rust",
            "fields": ["title"],
            "pattern": "rust",
            "feeds": ["https://example.com/feed2"],
        }]}),
        &feeds,
    );
    assert_eq!(tags(&runner, entry(2, "rust news")), ["rust"]);
    assert!(tags(&runner, entry(2, "go news")).is_empty());
    assert!(tags(&runner, entry(1, "rust news")).is_empty());
}

#[test]
fn feed_urls_are_looked_up_once() {
    let feeds = Arc::new(Feeds::default());
    let runner = auto_tag_with_feeds(
        json!({"rules": [
            {"tag": "a", "feeds": ["https://example.com/feed1", 2]},
            {"tag": "b", "pattern": "x", "feeds": [3]},
        ]}),
        &feeds,
    );
    assert_eq!(tags(&runner, entry(1, "t")), ["a"]);
    assert_eq!(tags(&runner, entry(2, "t")), ["a"]);
    assert_eq!(tags(&runner, entry(1, "t")), ["a"]);
    assert!(tags(&runner, entry(3, "t")).is_empty());
    // Feed 2 is named by id, so only feeds 1 and 3 are looked up.
    assert_eq!(feeds.lookups.load(Ordering::SeqCst), 2);

    // A removed feed's id may be reused, so it is looked up again.
    runner.dispatch_observe(
        Event::FeedRemoved,
        EventPayload::Feed {
            id: 1,
            url: "https://example.com/feed1".to_string(),
            title: "f".to_string(),
        },
    );
    assert_eq!(tags(&runner, entry(1, "t")), ["a"]);
    assert_eq!(feeds.lookups.load(Ordering::SeqCst), 3);
}

#[test]
fn tags_are_added_once() {
    let runner = auto_tag(json!({"rules": [
        {"tag": "t", "pattern": "x"},
        {"tag": "t", "feeds": [1]},
        {"tag": "system:saved", "feeds": [1]},
    ]}))
    .unwrap();
    assert_eq!(tags(&runner, entry(1, "x")), ["t", "system:saved"]);
    // A tag already there, from an earlier plugin, is not added again.
    let mut e = entry(1, "x");
    e.tags = vec!["system:saved".into()];
    assert_eq!(tags(&runner, e), ["system:saved", "t"]);
}

#[test]
fn bad_rules_fail_to_load() {
    for (config, message) in [
        (
            json!({"rules": [{"tag": "t"}]}),
            "needs a 'pattern', 'feeds'",
        ),
        (
            json!({"rules": [{"tag": "t", "pattern": "", "feeds": []}]}),
            "needs a 'pattern', 'feeds'",
        ),
        (json!({"rules": [{"pattern": "x"}]}), "'tag'"),
        (json!({"rules": [{"pattern": "x", "tag": ""}]}), "rules[1]"),
        (
            json!({"rules": [{"pattern": "x", "tag": "system:starred"}]}),
            "unknown system tag",
        ),
        (
            json!({"rules": [{"tag": "t", "fields": "titel", "pattern": "x"}]}),
            "unknown field",
        ),
        (json!({"rules": [{"tag": "t", "pattern": "("}]}), "rules[1]"),
        (
            json!({"rules": [{"tag": "t", "pattern": "x", "flags": "z"}]}),
            "rules[1]",
        ),
        (
            json!({"rules": [{"tag": "t", "feeds": [1.5]}]}),
            "feed ids or URLs",
        ),
        (json!({"rules": "x"}), "must be a list"),
    ] {
        let err = auto_tag(config.clone()).err().unwrap();
        assert!(err.contains(message), "{config}: {err}");
    }
}

/// End to end, through a server: stored entries are tagged when the plugin
/// is installed and whenever its rules change, but not again when they have
/// not changed.
#[tokio::test]
async fn stored_entries_are_tagged_when_the_rules_change() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let ids: Vec<i64> = {
        let conn = tc.database_conn()?;
        let mut ids = Vec::new();
        for (url, title) in [
            ("https://example.com/a", "release notes"),
            ("https://example.com/b", "rust 2.0"),
            ("https://example.com/b", "go 2.0"),
        ] {
            conn.execute(
                "INSERT OR IGNORE INTO feeds (title, url) VALUES (?1, ?1)",
                [url],
            )?;
            let feed: i64 =
                conn.query_row("SELECT id FROM feeds WHERE url = ?1", [url], |r| r.get(0))?;
            conn.execute(
                "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
                 VALUES (?1, 'rss', ?2, 0, ?2, 'u')",
                rusqlite::params![feed, title],
            )?;
            ids.push(conn.last_insert_rowid());
        }
        ids
    };
    let dir = tc.user_plugins_dir().join("auto-tag");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("manifest.toml"), MANIFEST)?;
    std::fs::write(dir.join("plugin.wasm"), WASM)?;
    let overrides = json!({"rules": [{"tag": "a", "feeds": ["https://example.com/a"]}]});
    crate::db::plugins::set_config_overrides(
        &tc.database_conn()?,
        "auto-tag",
        overrides.as_object().unwrap(),
    )?;

    let db = tc.database_path();
    let has_tag = move |id: i64, tag: &str| -> Result<bool> {
        let conn = crate::db::ConnectionBuilder::default()
            .at_path(&db)
            .read_write()
            .build()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
             WHERE et.entry_id = ?1 AND t.name = ?2)",
            rusqlite::params![id, tag],
            |row| row.get(0),
        )?)
    };
    let wait_until_tagged = |id: i64, tag: &'static str| {
        let has_tag = has_tag.clone();
        async move {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !has_tag(id, tag)? {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "entry {id} was never tagged {tag}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        }
    };

    let tc = tc.init_server()?;
    let client = tc.client()?;
    wait_until_tagged(ids[0], "a").await?;
    // The scan has tagged the first entry; give it time to reach the rest.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!has_tag(ids[1], "a")?);
    assert!(!has_tag(ids[2], "a")?);

    // Untag the entry, and reload with the same rules: it stays untagged.
    tc.database_conn()?
        .execute("DELETE FROM entry_tags WHERE entry_id = ?1", [ids[0]])?;
    let url = "http://localhost/v1/plugins/name/auto-tag/config";
    let resp = client
        .patch(url)
        .json(&json!({"rescan": true}))
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!has_tag(ids[0], "a")?);

    // Changing the rules applies them to stored entries.
    let resp = client
        .patch(url)
        .json(&json!({"rules": [{
            "tag": "rust",
            "fields": ["title"],
            "pattern": "rust",
            "feeds": ["https://example.com/b"],
        }]}))
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    wait_until_tagged(ids[1], "rust").await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!has_tag(ids[2], "rust")?);
    assert!(!has_tag(ids[0], "rust")?);
    assert!(!has_tag(ids[0], "a")?);

    Ok(())
}
