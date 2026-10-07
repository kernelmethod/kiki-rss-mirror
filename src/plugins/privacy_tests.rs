//! Tests for the `privacy` plugin shipped in `plugins/privacy/`, built from
//! the crate in that directory by `build.rs`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use crate::scripting::wasm::WasmScriptRunner;
use crate::scripting::{
    Event, EventPayload, FeedEntry, FeedInfo, ScriptRunner, ScriptServices, ScriptSource,
    ServiceCall, ServiceReply,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const MANIFEST: &str = include_str!("../../plugins/privacy/manifest.toml");
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/privacy.wasm"));

/// The plugin, loaded with `config`, looking feeds up in `services`.
fn load(
    config: Value,
    services: Option<Arc<dyn ScriptServices>>,
) -> Result<WasmScriptRunner, String> {
    let source = ScriptSource {
        name: "privacy".to_string(),
        config: config.to_string(),
        ..ScriptSource::wasm(WASM.to_vec())
    };
    WasmScriptRunner::from_sources_with(&[source], services).map_err(|e| e.to_string())
}

/// The plugin, loaded with `config`.
fn plugin(config: Value) -> Result<WasmScriptRunner, String> {
    load(config, None)
}

/// The plugin, loaded with its default config.
fn default_plugin() -> WasmScriptRunner {
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
        cache_assets: true,
    }
}

fn ingest(runner: &WasmScriptRunner, entry: FeedEntry) -> FeedEntry {
    runner.dispatch_transform_entry(entry).unwrap().unwrap()
}

/// The entry URL `url` becomes, going through `runner`.
fn clean_url(runner: &WasmScriptRunner, url: &str) -> String {
    ingest(runner, entry(Some(url), None)).url.unwrap()
}

/// The content `content` becomes, going through `runner`.
fn clean_content(runner: &WasmScriptRunner, content: &str) -> String {
    ingest(runner, entry(None, Some(content))).content.unwrap()
}

#[test]
fn the_manifest_is_valid() {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.name, "privacy");
    let names: Vec<_> = manifest.settings.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        ["params", "content", "pixels", "trackers", "skip_assets"]
    );
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
        let err = plugin(config.clone()).err().unwrap();
        assert!(err.contains("privacy: "), "{config}: {err}");
    }
}

#[test]
fn images_declared_one_pixel_or_smaller_are_removed() {
    let runner = default_plugin();
    let cases = [
        r#"<img src="https://example.com/p.gif" width="1" height="1">"#,
        r#"<img src="https://example.com/p.gif" width="1" height="1" />"#,
        r#"<IMG SRC='https://example.com/p.gif' HEIGHT='1' WIDTH='1'>"#,
        r#"<img src=https://example.com/p.gif width=1 height=1>"#,
        r#"<img width="0" height="0" src="https://example.com/p.gif">"#,
        r#"<img src="https://example.com/p.gif" width="1px" height=" 1 ">"#,
        r#"<img alt="a > b" width="1" height="1" src="https://example.com/p.gif">"#,
        "<img\n  src=\"https://example.com/p.gif\"\n  width=\"1\"\n  height=\"1\"\n>",
    ];
    for pixel in cases {
        let content = format!("<p>Before</p>{pixel}<p>After</p>");
        assert_eq!(
            clean_content(&runner, &content),
            "<p>Before</p><p>After</p>",
            "{pixel}"
        );
    }
}

#[test]
fn other_images_are_kept() {
    let runner = default_plugin();
    for image in [
        r#"<img src="https://example.com/photo.jpg">"#,
        r#"<img src="https://example.com/photo.jpg" width="640" height="480">"#,
        // A one-pixel-tall rule is not a tracker unless both sides are tiny.
        r#"<img src="https://example.com/rule.png" width="600" height="1">"#,
        r#"<img src="https://example.com/photo.jpg" width="1">"#,
        r#"<img src="https://example.com/photo.jpg" data-width="1" data-height="1">"#,
        r#"<img src="https://miro.medium.com/photo.jpg">"#,
        r#"<img src="https://medium.com/photo.jpg">"#,
        r#"<img src="https://notpixel.wp.com/b.gif">"#,
        r#"<img src="https://feeds.feedburner.com/~ff/Example?d=x">"#,
        r#"<img src="/b.gif?host=pixel.wp.com">"#,
        r#"<imgx width="1" height="1">"#,
        // Left unclosed, the tag is not touched.
        r#"<img width="1" height="1""#,
    ] {
        assert_eq!(clean_content(&runner, image), image);
    }
}

#[test]
fn images_from_trackers_are_removed() {
    let runner = default_plugin();
    for pixel in [
        r#"<img src="https://pixel.wp.com/b.gif?host=example.com&amp;blog=1" alt="">"#,
        r#"<img src="//stats.wordpress.com/b.gif">"#,
        r#"<img src="http://PIXEL.WP.COM./b.gif">"#,
        r#"<img src="https://pixel.wp.com:443/b.gif">"#,
        r#"<img src="https://feeds.feedburner.com/~r/Example/~4/abc123">"#,
        r#"<img src="https://medium.com/_/stat?event=post.clientViewed">"#,
        r#"<img src="https://www.google-analytics.com/collect?v=1">"#,
        r#"<img src="https://google-analytics.com/collect?v=1">"#,
        r#"<img src="https://sb.scorecardresearch.com/p?c1=2">"#,
    ] {
        assert_eq!(
            clean_content(&runner, &format!("a{pixel}b")),
            "ab",
            "{pixel}"
        );
    }
}

#[test]
fn pixels_can_be_left_alone() {
    let runner = plugin(json!({"params": ["utm_*"], "pixels": false})).unwrap();
    let content = r#"<img src="https://pixel.wp.com/b.gif?utm_source=x" width="1" height="1">"#;
    assert_eq!(
        clean_content(&runner, content),
        r#"<img src="https://pixel.wp.com/b.gif" width="1" height="1">"#
    );

    // Pixels are removed even when links are left alone.
    let runner = plugin(json!({"params": [], "content": false})).unwrap();
    assert_eq!(
        clean_content(&runner, r#"x<img src="a.gif" width="1" height="1">"#),
        "x"
    );
}

#[test]
fn trackers_can_be_configured() {
    let runner = plugin(json!({
        "params": [],
        "trackers": ["Tracker.Example", "*.ads.example/px", "cdn.example/t/"],
    }))
    .unwrap();
    for (image, removed) in [
        (r#"<img src="https://tracker.example/x.gif">"#, true),
        (r#"<img src="https://www.tracker.example/x.gif">"#, false),
        (r#"<img src="https://ads.example/px?id=1">"#, true),
        (r#"<img src="https://eu.ads.example/px/1.gif">"#, true),
        (r#"<img src="https://eu.ads.example/photo.jpg">"#, false),
        (r#"<img src="https://cdn.example/t/1.gif">"#, true),
        (r#"<img src="https://cdn.example/T/1.gif">"#, false),
        (r#"<img src="https://pixel.wp.com/b.gif">"#, false),
    ] {
        let expected = if removed { "" } else { image };
        assert_eq!(clean_content(&runner, image), expected, "{image}");
    }
}

#[test]
fn bad_trackers_fail_to_load() {
    for config in [
        json!({"trackers": "pixel.wp.com"}),
        json!({"trackers": [""]}),
        json!({"trackers": ["*."]}),
        json!({"trackers": ["*"]}),
        json!({"trackers": ["https://pixel.wp.com/"]}),
        json!({"trackers": ["pixel.wp.com:443"]}),
        json!({"trackers": ["a*.example"]}),
        json!({"trackers": ["example.com/a b"]}),
        json!({"trackers": [3]}),
    ] {
        let err = plugin(config.clone()).err().unwrap();
        assert!(err.contains("privacy: "), "{config}: {err}");
    }
}

/// Whether `runner` lets Kiki cache the assets of an entry from feed
/// `feed_id`.
fn caches_assets(runner: &WasmScriptRunner, feed_id: i64) -> bool {
    let mut e = entry(None, Some("<img src=\"https://example.com/photo.jpg\">"));
    e.feed_id = feed_id;
    let e = ingest(runner, e);
    assert_eq!(
        e.content.as_deref(),
        Some("<img src=\"https://example.com/photo.jpg\">")
    );
    e.cache_assets
}

#[test]
fn assets_are_cached_by_default() {
    let runner = default_plugin();
    assert!(caches_assets(&runner, 1));
}

#[test]
fn assets_are_not_cached_for_feeds_given_by_id() {
    let runner = plugin(json!({"skip_assets": [2, 3]})).unwrap();
    assert!(caches_assets(&runner, 1));
    assert!(!caches_assets(&runner, 2));
    assert!(!caches_assets(&runner, 3));
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

#[test]
fn assets_are_not_cached_for_feeds_given_by_url() {
    let feeds = Arc::new(Feeds::default());
    let runner = load(
        json!({"skip_assets": ["https://example.com/feed2", 3]}),
        Some(feeds.clone() as Arc<dyn ScriptServices>),
    )
    .unwrap();
    let lookups = || feeds.lookups.load(Ordering::SeqCst);

    assert!(caches_assets(&runner, 1));
    assert!(!caches_assets(&runner, 2));
    assert!(!caches_assets(&runner, 3));
    assert!(caches_assets(&runner, 4));
    // Feed 3 is listed by id, so needs no lookup; the others are looked up
    // once each.
    assert_eq!(lookups(), 3);
    assert!(!caches_assets(&runner, 2));
    assert!(caches_assets(&runner, 1));
    assert_eq!(lookups(), 3);

    // A fetch may have moved the feed to a new URL, so it is looked up again.
    runner.dispatch_observe(
        Event::FetchSuccess,
        EventPayload::FetchSuccess {
            feed_id: 2,
            status: 200,
            url: "https://example.com/feed2".to_string(),
            content_length: None,
        },
    );
    assert!(!caches_assets(&runner, 2));
    assert_eq!(lookups(), 4);
}

#[test]
fn bad_skip_assets_fail_to_load() {
    for config in [
        json!({"skip_assets": 3}),
        json!({"skip_assets": [""]}),
        json!({"skip_assets": [1.5]}),
        json!({"skip_assets": [true]}),
    ] {
        let err = plugin(config.clone()).err().unwrap();
        assert!(err.contains("privacy: "), "{config}: {err}");
    }
}
