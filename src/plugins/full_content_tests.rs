//! Tests for the `full-content` plugin shipped in `plugins/full-content/`, built from
//! the crate in that directory by `build.rs`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use crate::fetcher::page::FetchedPage;
use crate::plugins::{Permission, PluginManifest};
use crate::scripting::wasm::WasmScriptRunner;
use crate::scripting::{
    FeedEntry, FeedInfo, ScriptRunner, ScriptServices, ScriptSource, ServiceCall, ServiceReply,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const MANIFEST: &str = include_str!("../../plugins/full-content/manifest.toml");
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/full-content.wasm"));

/// A paragraph of `n` characters of text, none of them whitespace.
fn text(n: usize) -> String {
    "x".repeat(n)
}

/// Answers the plugin's calls: feeds' URLs, the entries stored, and the pages on the web,
/// recording the pages it is asked for.
#[derive(Default)]
struct Web {
    /// Pages by URL: the body, or why it can't be had.
    pages: Mutex<HashMap<String, Result<String, String>>>,
    /// Stored entries' content, by guid.
    stored: Mutex<HashMap<String, String>>,
    fetched: Mutex<Vec<String>>,
}

impl Web {
    fn page(&self, url: &str, body: Result<&str, &str>) {
        let body = body.map(str::to_string).map_err(str::to_string);
        self.pages.lock().unwrap().insert(url.to_string(), body);
    }

    fn fetched(&self) -> Vec<String> {
        std::mem::take(&mut *self.fetched.lock().unwrap())
    }
}

impl ScriptServices for Web {
    fn call(&self, plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        assert_eq!(plugin, "full-content");
        match call {
            ServiceCall::GetFeed { feed_id } => Ok(ServiceReply::Feed(Some(FeedInfo {
                id: feed_id,
                url: Some(format!("https://example.com/feed/{feed_id}")),
                title: "Feed".to_string(),
            }))),
            ServiceCall::GetEntry { feed_id, guid } => {
                let content = self.stored.lock().unwrap().get(&guid).cloned();
                Ok(ServiceReply::Entry(content.map(|content| {
                    let mut stored = entry(&guid, Some(&content));
                    stored.id = Some(1);
                    stored.feed_id = feed_id;
                    Box::new(stored)
                })))
            }
            ServiceCall::FetchPage { url } => {
                self.fetched.lock().unwrap().push(url.clone());
                let body = self.pages.lock().unwrap().get(&url).cloned();
                let body =
                    body.unwrap_or_else(|| Err("the server answered 404 Not Found".into()))?;
                Ok(ServiceReply::Page(Box::new(FetchedPage {
                    url,
                    content_type: "text/html".to_string(),
                    body,
                    truncated: false,
                })))
            }
            other => Err(format!("unexpected call {other:?}")),
        }
    }
}

/// The plugin, loaded with `config` applied over its defaults, calling `web`.
fn plugin(config: Value, web: &Arc<Web>) -> Result<WasmScriptRunner, String> {
    let manifest = PluginManifest::parse(MANIFEST).unwrap();
    let time_budget = manifest.time_budget();
    let mut merged = manifest.config;
    if let Value::Object(overrides) = config {
        merged.extend(overrides);
    }
    let source = ScriptSource {
        name: "full-content".to_string(),
        config: Value::Object(merged).to_string(),
        time_budget,
        ..ScriptSource::new(WASM.to_vec())
    };
    WasmScriptRunner::from_sources_with(&[source], Some(web.clone() as Arc<dyn ScriptServices>))
        .map_err(|e| e.to_string())
}

/// The plugin, with entries of less than 50 characters taken for stubs.
fn default_plugin(web: &Arc<Web>) -> WasmScriptRunner {
    plugin(json!({"min_length": 50}), web).unwrap()
}

/// An entry of feed 1, linking to `https://example.com/<guid>`.
fn entry(guid: &str, content: Option<&str>) -> FeedEntry {
    FeedEntry {
        id: None,
        feed_id: 1,
        syndication_format: "rss".to_string(),
        guid: guid.to_string(),
        published_at: None,
        title: "title".to_string(),
        url: Some(format!("https://example.com/posts/{guid}")),
        content: content.map(str::to_string),
        authors: vec![],
        categories: vec![],
        tags: vec![],
        cache_assets: true,
    }
}

/// The content `entry` comes out of `runner` with.
fn content(runner: &dyn ScriptRunner, entry: FeedEntry) -> Option<String> {
    runner
        .dispatch_transform_entry(entry)
        .unwrap()
        .unwrap()
        .content
}

#[test]
fn the_manifest_is_valid() {
    let manifest = PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.name, "full-content");
    assert_eq!(manifest.permissions, [Permission::Network]);
    let names: Vec<_> = manifest.settings.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["feeds", "exclude", "min_length", "selectors"]);
    crate::plugins::settings::check_config(&manifest.settings, &manifest.config).unwrap();
    // It runs before the plugins that clean what it fetches.
    assert!("full-content" < "privacy" && "full-content" < "sanitize");
}

#[test]
fn full_entries_are_left_alone() {
    let web = Arc::new(Web::default());
    let runner = default_plugin(&web);
    let full = format!("<p>{}</p>", text(60));
    assert_eq!(content(&runner, entry("a", Some(&full))), Some(full));
    // Markup and whitespace aren't text.
    let spaced = format!("<p>{}</p><script>{}</script>", "x ".repeat(30), text(60));
    assert_eq!(content(&runner, entry("b", Some(&spaced))).unwrap(), spaced);
    assert_eq!(web.fetched(), ["https://example.com/posts/b"]);
}

#[test]
fn stubs_are_filled_in_with_the_article() {
    let web = Arc::new(Web::default());
    let page = format!(
        r##"<html><head><script>track()</script></head><body>
        <nav><a href="/">Home</a></nav>
        <article><p>Too short</p></article>
        <div class="entry-content"><p onclick="x()">{}</p>
        <a href="/about">About</a> <a href="javascript:x()">Bad</a> <a href="#n">Note</a>
        <img src="data:image/gif;base64,R0lG" data-src="img/a.png">
        <script>alert(1)</script><iframe src="https://ads.example"></iframe>
        </div></body></html>"##,
        text(60)
    );
    web.page("https://example.com/posts/a", Ok(&page));
    let runner = default_plugin(&web);

    let filled = content(&runner, entry("a", Some("<p>Read more…</p>"))).unwrap();
    assert_eq!(
        filled,
        format!(
            r##"<p>{}</p>
        <a href="https://example.com/about">About</a> <a>Bad</a> <a href="#n">Note</a>
        <img src="https://example.com/posts/img/a.png" data-src="img/a.png">"##,
            text(60)
        )
    );
    assert_eq!(web.fetched(), ["https://example.com/posts/a"]);

    // An entry with no content at all is a stub too.
    let filled = content(&runner, entry("a", None)).unwrap();
    assert!(filled.contains(&text(60)), "{filled}");
}

#[test]
fn articles_fetched_before_are_kept() {
    let web = Arc::new(Web::default());
    let article = format!("<p>{}</p>", text(60));
    web.stored
        .lock()
        .unwrap()
        .insert("a".into(), article.clone());
    web.stored
        .lock()
        .unwrap()
        .insert("b".into(), "<p>stub</p>".into());
    web.page(
        "https://example.com/posts/b",
        Ok(&format!("<main>{article}</main>")),
    );
    let runner = default_plugin(&web);

    assert_eq!(
        content(&runner, entry("a", Some("<p>stub</p>"))),
        Some(article.clone())
    );
    assert!(web.fetched().is_empty());
    // A stub stored before is fetched again.
    assert_eq!(
        content(&runner, entry("b", Some("<p>stub</p>"))),
        Some(article)
    );
    assert_eq!(web.fetched(), ["https://example.com/posts/b"]);
}

#[test]
fn the_best_candidate_wins_when_none_is_long_enough() {
    let web = Arc::new(Web::default());
    let page = format!("<article>{}</article><main>{}</main>", text(20), text(10));
    web.page("https://example.com/posts/a", Ok(&page));
    web.page("https://example.com/posts/b", Ok("<main>abc</main>"));
    let runner = default_plugin(&web);
    assert_eq!(content(&runner, entry("a", Some("stub"))), Some(text(20)));
    // Nothing longer than the stub: it stays.
    assert_eq!(
        content(&runner, entry("b", Some("stub"))).as_deref(),
        Some("stub")
    );
}

#[test]
fn pages_that_fail_are_not_tried_again_soon() {
    let web = Arc::new(Web::default());
    web.page("https://example.com/posts/a", Err("connection refused"));
    let runner = default_plugin(&web);
    assert_eq!(
        content(&runner, entry("a", Some("stub"))).as_deref(),
        Some("stub")
    );
    assert_eq!(
        content(&runner, entry("a", Some("stub"))).as_deref(),
        Some("stub")
    );
    assert_eq!(web.fetched(), ["https://example.com/posts/a"]);
    // A page with no article counts as failing too.
    web.page("https://example.com/posts/b", Ok("<p>nothing here</p>"));
    content(&runner, entry("b", Some("stub")));
    content(&runner, entry("b", Some("stub")));
    assert_eq!(web.fetched(), ["https://example.com/posts/b"]);
}

#[test]
fn the_plugin_stops_asking_once_refused() {
    let web = Arc::new(Web::default());
    let refusal = "fetch-page: network access for plugins is turned off; set allow_network \
                   = true under [plugins] in kiki.toml to turn it on";
    web.page("https://example.com/posts/a", Err(refusal));
    let runner = default_plugin(&web);
    assert_eq!(
        content(&runner, entry("a", Some("stub"))).as_deref(),
        Some("stub")
    );
    assert_eq!(
        content(&runner, entry("b", Some("stub"))).as_deref(),
        Some("stub")
    );
    assert_eq!(web.fetched(), ["https://example.com/posts/a"]);
}

#[test]
fn feeds_can_be_chosen() {
    let web = Arc::new(Web::default());
    let runner = plugin(json!({"min_length": 50, "feeds": [2]}), &web).unwrap();
    content(&runner, entry("a", Some("stub")));
    assert!(web.fetched().is_empty());
    let mut other = entry("a", Some("stub"));
    other.feed_id = 2;
    content(&runner, other);
    assert_eq!(web.fetched(), ["https://example.com/posts/a"]);

    let runner = plugin(
        json!({"min_length": 50, "exclude": ["https://example.com/feed/1"]}),
        &web,
    )
    .unwrap();
    content(&runner, entry("b", Some("stub")));
    assert!(web.fetched().is_empty());
}

#[test]
fn only_http_links_are_fetched() {
    let web = Arc::new(Web::default());
    let runner = default_plugin(&web);
    let mut local = entry("a", Some("stub"));
    local.url = Some("file:///etc/passwd".to_string());
    content(&runner, local);
    let mut none = entry("b", Some("stub"));
    none.url = None;
    content(&runner, none);
    assert!(web.fetched().is_empty());
}

#[test]
fn bad_configs_fail_to_load() {
    let web = Arc::new(Web::default());
    for config in [
        json!({"selectors": ["<<"]}),
        json!({"selectors": [""]}),
        json!({"selectors": "article"}),
        json!({"min_length": 0}),
        json!({"min_length": "long"}),
        json!({"feeds": [true]}),
    ] {
        assert!(plugin(config.clone(), &web).is_err(), "{config}");
    }
}
