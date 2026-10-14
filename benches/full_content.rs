//! Benchmarks of the `full-content` plugin: how long it takes over an entry, run
//! through Kiki's WebAssembly runner, with pages served from memory rather than the
//! network, so that what is measured is the plugin's own work.
//!
//! ```text
//! cargo bench --bench full_content
//! ```
//!
//! Needs the `default-plugins` feature (on by default), which builds the plugin.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use kiki_rss::fetcher::page::FetchedPage;
use kiki_rss::plugins::PluginManifest;
use kiki_rss::scripting::wasm::WasmScriptRunner;
use kiki_rss::scripting::{
    FeedEntry, FeedInfo, ScriptRunner, ScriptServices, ScriptSource, ServiceCall, ServiceReply,
};
use serde_json::Value;
use std::fmt::Write;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const MANIFEST: &str = include_str!("../plugins/full-content/manifest.toml");
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/full-content.wasm"));

/// Serves pages and stored entries from memory.
#[derive(Default)]
struct Web {
    /// Page bodies by URL; any URL starting with a key is served that body.
    pages: RwLock<Vec<(String, Arc<String>)>>,
    /// Stored entries' content.
    stored: RwLock<Option<String>>,
}

impl ScriptServices for Web {
    fn call(&self, _: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        match call {
            ServiceCall::GetFeed { feed_id } => Ok(ServiceReply::Feed(Some(FeedInfo {
                id: feed_id,
                url: Some("https://example.com/feed".to_string()),
                title: "Feed".to_string(),
            }))),
            ServiceCall::GetEntry { feed_id, guid } => {
                let stored = self.stored.read().unwrap().clone();
                Ok(ServiceReply::Entry(stored.map(|content| {
                    let mut e = entry(&guid, &content);
                    e.id = Some(1);
                    e.feed_id = feed_id;
                    Box::new(e)
                })))
            }
            ServiceCall::FetchPage { url } => {
                let pages = self.pages.read().unwrap();
                let body = pages
                    .iter()
                    .find(|(prefix, _)| url.starts_with(prefix.as_str()))
                    .map(|(_, body)| String::clone(body))
                    .ok_or("the server answered 404 Not Found")?;
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

fn plugin(web: &Arc<Web>) -> WasmScriptRunner {
    let manifest = PluginManifest::parse(MANIFEST).expect("the manifest is valid");
    let source = ScriptSource {
        name: "full-content".to_string(),
        config: Value::Object(manifest.config.clone()).to_string(),
        time_budget: manifest.time_budget(),
        permissions: manifest.permissions.clone(),
        ..ScriptSource::new(WASM.to_vec())
    };
    let services = Some(web.clone() as Arc<dyn ScriptServices>);
    WasmScriptRunner::from_sources_with(&[source], services).expect("the plugin loads")
}

fn entry(guid: &str, content: &str) -> FeedEntry {
    FeedEntry {
        id: None,
        feed_id: 1,
        syndication_format: "rss".to_string(),
        guid: guid.to_string(),
        published_at: None,
        title: "title".to_string(),
        url: Some(format!("https://example.com/posts/{guid}")),
        content: Some(content.to_string()),
        authors: vec![],
        categories: vec![],
        tags: vec![],
        cache_assets: true,
    }
}

const STUB: &str = "<p>The first paragraph of the article, and a link to the rest.</p>";

/// `n` paragraphs of article text, with links, images (some lazily loaded), and the odd
/// script and event handler.
fn article(n: usize) -> String {
    let mut html = String::new();
    for i in 0..n {
        match i % 4 {
            0 => write!(html, r#"<p>Lorem ipsum dolor sit amet, <a href="/related/{i}" onclick="t({i})">consectetur</a> adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.</p>"#),
            1 => write!(html, r#"<figure><img src="data:image/gif;base64,R0lGOD" data-src="/img/{i}.jpg" alt="Photo {i}" loading="lazy"><figcaption>Figure {i}</figcaption></figure>"#),
            2 => write!(html, r#"<p>Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat &amp; more.<script>track({i})</script></p>"#),
            _ => write!(html, r#"<blockquote cite="q/{i}"><p>Duis aute irure dolor in <a href="https://other.example/{i}">reprehenderit</a> in voluptate velit esse cillum dolore.</p></blockquote>"#),
        }
        .unwrap();
    }
    html
}

/// A news site's page: a head with scripts and styles, a navigation menu of `nav` links, the
/// article's `paragraphs` in `wrapper` (an open tag and close tag), a sidebar and a footer.
fn page(nav: usize, paragraphs: usize, wrapper: (&str, &str)) -> String {
    let mut html =
        String::from("<!doctype html><html><head><meta charset=utf-8><title>Post</title>");
    for i in 0..8 {
        write!(
            html,
            r#"<script src="/js/{i}.js"></script><style>.c{i}{{color:red}}</style>"#
        )
        .unwrap();
    }
    html.push_str(r#"</head><body><header><nav class="menu"><ul>"#);
    for i in 0..nav {
        write!(
            html,
            r#"<li class="item"><a href="/section/{i}">Section {i}</a></li>"#
        )
        .unwrap();
    }
    html.push_str("</ul></nav></header><div class=\"layout\">");
    html.push_str(wrapper.0);
    html.push_str("<h1>Post</h1>");
    html.push_str(&article(paragraphs));
    html.push_str(wrapper.1);
    html.push_str(r#"<aside class="sidebar">"#);
    for i in 0..nav / 4 {
        write!(
            html,
            r#"<div class="widget"><h3>Popular {i}</h3><p>Another post's teaser text.</p></div>"#
        )
        .unwrap();
    }
    html.push_str("</aside></div><footer><p>© Example</p></footer></body></html>");
    html
}

/// Entries that aren't stubs, and stubs whose article is already stored: no page fetched.
fn no_fetch(c: &mut Criterion) {
    let web = Arc::new(Web::default());
    let runner = plugin(&web);
    let mut group = c.benchmark_group("full-content/no-fetch");
    for paragraphs in [4, 32, 256] {
        let content = article(paragraphs);
        group.throughput(Throughput::Bytes(content.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("full-entry", format!("{}B", content.len())),
            &content,
            |b, content| {
                b.iter_batched(
                    || entry("a", content),
                    |e| black_box(runner.dispatch_transform_entry(e)),
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }
    for paragraphs in [32, 256] {
        let content = article(paragraphs);
        *web.stored.write().unwrap() = Some(content.clone());
        group.bench_function(
            BenchmarkId::new("stub-already-stored", format!("{}B", content.len())),
            |b| {
                b.iter_batched(
                    || entry("a", STUB),
                    |e| black_box(runner.dispatch_transform_entry(e)),
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

/// Stubs whose article is fetched and extracted.
fn fetch(c: &mut Criterion) {
    let web = Arc::new(Web::default());
    let runner = plugin(&web);
    let counter = AtomicU64::new(0);
    let mut group = c.benchmark_group("full-content/fetch");
    group.measurement_time(Duration::from_secs(10));
    let cases: &[(&str, usize, usize, (&str, &str))] = &[
        // Found by the first selector.
        (
            "articleBody",
            100,
            32,
            (
                r#"<article><div itemprop="articleBody">"#,
                "</div></article>",
            ),
        ),
        (
            "articleBody",
            400,
            128,
            (
                r#"<article><div itemprop="articleBody">"#,
                "</div></article>",
            ),
        ),
        (
            "articleBody",
            2000,
            512,
            (
                r#"<article><div itemprop="articleBody">"#,
                "</div></article>",
            ),
        ),
        // Found only by `main`, the last selector: every other one runs over the page first.
        ("main-only", 100, 32, ("<main>", "</main>")),
        ("main-only", 400, 128, ("<main>", "</main>")),
        ("main-only", 2000, 512, ("<main>", "</main>")),
        // No selector matches: the page is searched for nothing, and the URL remembered.
        ("no-article", 400, 128, ("<div class=\"story\">", "</div>")),
        ("no-article", 2000, 512, ("<div class=\"story\">", "</div>")),
    ];
    for &(name, nav, paragraphs, wrapper) in cases {
        let body = Arc::new(page(nav, paragraphs, wrapper));
        *web.pages.write().unwrap() =
            vec![("https://example.com/posts/".to_string(), body.clone())];
        group.throughput(Throughput::Bytes(body.len() as u64));
        group.bench_function(
            BenchmarkId::new(name, format!("{}KB", body.len() / 1024)),
            |b| {
                b.iter_batched(
                    // A new URL each time, as a page that held no article isn't fetched again.
                    || entry(&counter.fetch_add(1, Ordering::Relaxed).to_string(), STUB),
                    |e| black_box(runner.dispatch_transform_entry(e)),
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(benches, no_fetch, fetch);
criterion_main!(benches);
