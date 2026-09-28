//! Tests for the `filter` plugin shipped in `plugins/filter/`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use crate::scripting::lua::LuaScriptRunner;
use crate::scripting::{FeedEntry, ScriptRunner, ScriptSource};
use crate::test::TestBuilder;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const MANIFEST: &str = include_str!("../../plugins/filter/manifest.toml");
const MAIN: &str = include_str!("../../plugins/filter/main.lua");

/// The filter, loaded with `config`.
fn filter(config: Value) -> Result<LuaScriptRunner, crate::scripting::lua::ScriptError> {
    let mut source = ScriptSource::new(MAIN);
    source.name = "filter".to_string();
    source.config = config.to_string();
    LuaScriptRunner::from_sources(&[source])
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
    }
}

fn hidden(runner: &LuaScriptRunner, entry: FeedEntry) -> bool {
    let entry = runner.dispatch_transform_entry(entry).unwrap().unwrap();
    entry.tags.iter().any(|t| t == "system:hidden")
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
            json!({"include": [{"pattern": "x", "feeds": ["a"]}]}),
            "feed ids",
        ),
        (
            json!({"exclude": [{"pattern": "x", "flags": "z"}]}),
            "exclude[1]",
        ),
    ] {
        let err = filter(config.clone()).err().unwrap().to_string();
        assert!(err.contains(message), "{config}: {err}");
    }
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
    let dir = tc.plugins_dir().join("filter");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("manifest.toml"), MANIFEST)?;
    std::fs::write(dir.join("main.lua"), MAIN)?;
    let overrides = json!({"exclude": [{"fields": "title", "pattern": "sponsored"}]});
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
        .json(&json!({"exclude": [{"fields": "title", "pattern": "webinar"}]}))
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    wait_until_hidden(ids[2]).await?;
    assert!(!is_hidden(ids[1])?);
    assert!(!is_hidden(ids[0])?);

    Ok(())
}
