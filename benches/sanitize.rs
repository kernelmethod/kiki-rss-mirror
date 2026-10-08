//! Benchmarks of the default plugins written in Rust: how fast the `sanitize` plugin
//! sanitizes entries of typical sizes, run through Kiki's WebAssembly runner as the
//! server runs it.
//!
//! ```text
//! cargo bench --bench sanitize
//! ```
//!
//! Needs the `default-plugins` feature (on by default), which builds the plugin.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use kiki_rss::plugins::PluginManifest;
use kiki_rss::scripting::wasm::WasmScriptRunner;
use kiki_rss::scripting::{FeedEntry, ScriptRunner, ScriptSource};
use serde_json::Value;
use std::fmt::Write;
use std::hint::black_box;
use std::time::Duration;

const MANIFEST: &str = include_str!("../plugins/sanitize/manifest.toml");
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/sanitize.wasm"));

/// The plugin, with its default config, as `kiki init` installs it.
fn plugin() -> WasmScriptRunner {
    let manifest = PluginManifest::parse(MANIFEST).expect("the manifest is valid");
    let source = ScriptSource {
        name: "sanitize".to_string(),
        config: Value::Object(manifest.config.clone()).to_string(),
        time_budget: manifest.time_budget(),
        ..ScriptSource::new(WASM.to_vec())
    };
    WasmScriptRunner::from_sources_with(&[source], None).expect("the plugin loads")
}

fn entry(content: &str) -> FeedEntry {
    FeedEntry {
        id: None,
        feed_id: 1,
        syndication_format: "rss".to_string(),
        guid: "guid".to_string(),
        published_at: None,
        title: "title".to_string(),
        url: Some("https://example.com/post".to_string()),
        content: Some(content.to_string()),
        authors: vec![],
        categories: vec![],
        tags: vec![],
        cache_assets: true,
    }
}

/// A post of about `paragraphs` paragraphs, mixing what the plugin keeps
/// with what it removes, as feeds' HTML does: links with tracking
/// attributes, images with classes and `srcset`, scripts, styles, iframes,
/// comments, tables and code with character references.
fn post(paragraphs: usize) -> String {
    let mut html = String::from(r#"<div class="post" style="margin:0" data-id="42">"#);
    for i in 0..paragraphs {
        match i % 8 {
            0 => write!(
                html,
                r#"<h2 id="s{i}" class="heading">Section {i}</h2><p class="lead">Lorem ipsum dolor sit amet, <a href="https://example.com/{i}?utm_source=rss&amp;id={i}" target="_blank" rel="noopener" onclick="track({i})">consectetur</a> adipiscing elit, sed do <em>eiusmod</em> tempor <strong>incididunt</strong> ut labore et dolore magna aliqua.</p>"#
            ),
            1 => write!(
                html,
                r#"<p>Ut enim ad minim veniam, quis <code>nostrud</code> exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in <a href="/relative/{i}" title="Link {i}">reprehenderit</a> in voluptate velit esse cillum dolore eu fugiat nulla pariatur.<span style="display:none">hidden</span></p>"#
            ),
            2 => write!(
                html,
                r#"<figure class="wide"><picture><source srcset="/img/{i}.webp" type="image/webp"><img src="https://cdn.example.com/img/{i}.jpg" srcset="/img/{i}.jpg 1x, /img/{i}@2x.jpg 2x" alt="Photo {i}" class="photo" width="800" height="600" loading="lazy" onerror="fallback(this)"></picture><figcaption>Figure {i}: a photo</figcaption></figure>"#
            ),
            3 => write!(
                html,
                r#"<script async src="https://ads.example/ad.js"></script><!-- ad slot {i} --><div id="ad{i}" class="ad"><iframe src="https://ads.example/frame?{i}" width="300" height="250"></iframe></div><style>.ad {{ display: none }}</style>"#
            ),
            4 => write!(
                html,
                r#"<pre class="code"><code class="language-rust">fn main() {{ if a &lt; b &amp;&amp; c &gt; d {{ println!("&quot;{i}&quot;"); }} }}</code></pre>"#
            ),
            5 => write!(
                html,
                r#"<table class="data"><thead><tr><th scope="col" style="width:50%">Name</th><th scope="col">Value</th></tr></thead><tbody><tr><td>Alpha</td><td colspan="1" class="n">{i}</td></tr><tr><td>Beta</td><td>{i}.5</td></tr></tbody></table>"#
            ),
            6 => write!(
                html,
                r#"<blockquote cite="https://example.com/q/{i}"><p lang="fr">Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt <a href="javascript:alert({i})">mollit</a> anim id est laborum.</p></blockquote><ul class="list"><li>One</li><li>Two <b>bold</b></li><li><a href="mailto:a{i}@example.com">Mail</a></li></ul>"#
            ),
            _ => write!(
                html,
                r#"<p><font color="red"><center>Sed ut perspiciatis unde omnis iste natus error sit voluptatem accusantium doloremque laudantium, totam rem aperiam &amp; eaque ipsa quae ab illo inventore.</center></font><img src="https://tracker.example/p.gif?{i}" width="1" height="1" style="border:0"><button onclick="share()">Share</button></p>"#
            ),
        }
        .unwrap();
    }
    html.push_str("</div>");
    html
}

fn sanitize(c: &mut Criterion) {
    let runner = plugin();
    let mut group = c.benchmark_group("sanitize");
    // Enough for 100 samples of the larger entries.
    group.measurement_time(Duration::from_secs(10));
    for paragraphs in [2, 16, 128, 1024] {
        let html = post(paragraphs);
        group.throughput(Throughput::Bytes(html.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}B", html.len())),
            &html,
            |b, html| {
                b.iter_batched(
                    || entry(html),
                    |entry| black_box(runner.dispatch_transform_entry(entry)),
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(benches, sanitize);
criterion_main!(benches);
