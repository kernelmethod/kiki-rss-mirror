//! Tests for the `strip-tracking` plugin shipped in `plugins/strip-tracking/`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use crate::scripting::lua::LuaScriptRunner;
use crate::scripting::{FeedEntry, ScriptRunner, ScriptSource};
use serde_json::{json, Value};

const MANIFEST: &str = include_str!("../../plugins/strip-tracking/manifest.toml");
const MAIN: &str = include_str!("../../plugins/strip-tracking/main.lua");

/// The plugin, loaded with `config`.
fn plugin(config: Value) -> Result<LuaScriptRunner, crate::scripting::lua::ScriptError> {
    let mut source = ScriptSource::new(MAIN);
    source.name = "strip-tracking".to_string();
    source.config = config.to_string();
    LuaScriptRunner::from_sources(&[source])
}

/// The plugin, loaded with its default config.
fn default_plugin() -> LuaScriptRunner {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    plugin(Value::Object(manifest.config)).unwrap()
}

fn entry(url: Option<&str>, content: Option<&str>) -> FeedEntry {
    FeedEntry {
        id: None,
        feed_id: 1,
        syndication_format: "rss".to_string(),
        guid: "guid".to_string(),
        published_at: None,
        title: "title".to_string(),
        url: url.map(str::to_string),
        content: content.map(str::to_string),
        authors: vec![],
        categories: vec![],
        tags: vec![],
    }
}

fn ingest(runner: &LuaScriptRunner, entry: FeedEntry) -> FeedEntry {
    runner.dispatch_transform_entry(entry).unwrap().unwrap()
}

/// The entry URL `url` becomes, going through `runner`.
fn clean_url(runner: &LuaScriptRunner, url: &str) -> String {
    ingest(runner, entry(Some(url), None)).url.unwrap()
}

/// The content `content` becomes, going through `runner`.
fn clean_content(runner: &LuaScriptRunner, content: &str) -> String {
    ingest(runner, entry(None, Some(content))).content.unwrap()
}

#[test]
fn the_manifest_is_valid() {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.name, "strip-tracking");
    let names: Vec<_> = manifest.settings.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["params", "content"]);
    crate::plugins::settings::check_config(&manifest.settings, &manifest.config).unwrap();
}

#[test]
fn tracking_parameters_are_removed_from_entry_urls() {
    let runner = default_plugin();
    let cases = [
        (
            "https://example.com/a?utm_source=rss&utm_medium=feed&utm_campaign=x",
            "https://example.com/a",
        ),
        (
            "https://example.com/a?id=3&utm_source=rss&page=2",
            "https://example.com/a?id=3&page=2",
        ),
        (
            "https://example.com/a?fbclid=abc&id=3",
            "https://example.com/a?id=3",
        ),
        // Names are matched ignoring case.
        (
            "https://example.com/a?UTM_Source=rss&GCLID=1",
            "https://example.com/a",
        ),
        // Fragments written like query strings are cleaned too.
        (
            "https://example.com/a?id=1#xtor=RSS-1",
            "https://example.com/a?id=1",
        ),
        (
            "https://example.com/a?utm_source=rss#section-2",
            "https://example.com/a#section-2",
        ),
        (
            "https://example.com/a#utm_source=rss&x=1",
            "https://example.com/a#x=1",
        ),
        // Empty parts left behind are dropped.
        (
            "https://example.com/a?utm_source=rss&&id=1",
            "https://example.com/a?id=1",
        ),
        // Parameters with no value.
        ("https://example.com/a?utm_source", "https://example.com/a"),
    ];
    for (url, expected) in cases {
        assert_eq!(clean_url(&runner, url), expected, "cleaning {url}");
    }
}

#[test]
fn urls_without_tracking_parameters_are_untouched() {
    let runner = default_plugin();
    for url in [
        "https://example.com/a",
        "https://example.com/a?",
        "https://example.com/a?id=1&&page=2",
        "https://example.com/a?utm=1&my_utm_source=2&ref=feed",
        "https://example.com/a#top",
        "https://example.com/utm_source/?x=utm_source",
        "not a url",
    ] {
        assert_eq!(clean_url(&runner, url), url);
    }
    assert_eq!(ingest(&runner, entry(None, None)).url, None);
}

#[test]
fn links_in_content_are_cleaned() {
    let runner = default_plugin();
    let content = concat!(
        r#"<p>Read <a href="https://example.com/a?id=1&amp;utm_source=rss&amp;page=2">this</a>"#,
        r#" and <A HREF='https://example.com/b?utm_medium=feed'>that</A>"#,
        r#" and <a href=https://example.com/c?fbclid=1&x=2>more</a>.</p>"#,
        r#"<img src="https://example.com/i.png?utm_campaign=x" alt="utm_source=x">"#,
        r#"<img data-src="https://example.com/i.png?utm_campaign=x">"#,
        r#"<p>Plain text: https://example.com/?utm_source=rss</p>"#,
    );
    let expected = concat!(
        r#"<p>Read <a href="https://example.com/a?id=1&amp;page=2">this</a>"#,
        r#" and <A HREF='https://example.com/b'>that</A>"#,
        r#" and <a href=https://example.com/c?x=2>more</a>.</p>"#,
        r#"<img src="https://example.com/i.png" alt="utm_source=x">"#,
        r#"<img data-src="https://example.com/i.png?utm_campaign=x">"#,
        r#"<p>Plain text: https://example.com/?utm_source=rss</p>"#,
    );
    assert_eq!(clean_content(&runner, content), expected);
}

#[test]
fn content_can_be_left_alone() {
    let runner = plugin(json!({"params": ["utm_*"], "content": false})).unwrap();
    let content = r#"<a href="https://example.com/?utm_source=rss">x</a>"#;
    let e = ingest(
        &runner,
        entry(Some("https://example.com/?utm_source=rss"), Some(content)),
    );
    assert_eq!(e.url.as_deref(), Some("https://example.com/"));
    assert_eq!(e.content.as_deref(), Some(content));
}

#[test]
fn params_can_be_configured() {
    let runner = plugin(json!({"params": ["ref", "Track_*"]})).unwrap();
    assert_eq!(
        clean_url(
            &runner,
            "https://example.com/?ref=rss&track_id=1&TRACKING=2&utm_source=x"
        ),
        "https://example.com/?TRACKING=2&utm_source=x"
    );
    // With no params, nothing is removed.
    let runner = plugin(json!({"params": []})).unwrap();
    let url = "https://example.com/?utm_source=x";
    assert_eq!(clean_url(&runner, url), url);
}

#[test]
fn bad_params_fail_to_load() {
    for config in [
        json!({"params": "utm_*"}),
        json!({"params": [""]}),
        json!({"params": ["*"]}),
        json!({"params": [3]}),
    ] {
        let err = plugin(config.clone()).err().unwrap().to_string();
        assert!(err.contains("strip-tracking: "), "{config}: {err}");
    }
}
