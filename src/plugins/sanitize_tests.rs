//! Tests for the `sanitize` plugin shipped in `plugins/sanitize/`, built from
//! the crate in that directory by `build.rs`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use crate::plugins::PluginManifest;
use crate::scripting::composite::CompositeRunner;
use crate::scripting::wasm::WasmScriptRunner;
use crate::scripting::{FeedEntry, ScriptRunner, ScriptSource, TimeBudget};
use serde_json::{json, Value};

const MANIFEST: &str = include_str!("../../plugins/sanitize/manifest.toml");
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/sanitize.wasm"));

/// The plugin's source, with `config` applied over its defaults.
fn source(config: Value) -> ScriptSource {
    let manifest = PluginManifest::parse(MANIFEST).unwrap();
    let time_budget = manifest.time_budget();
    let mut merged = manifest.config;
    if let Value::Object(overrides) = config {
        merged.extend(overrides);
    }
    ScriptSource {
        name: "sanitize".to_string(),
        config: Value::Object(merged).to_string(),
        time_budget,
        ..ScriptSource::wasm(WASM.to_vec())
    }
}

/// The plugin, loaded with `config` applied over its defaults.
fn plugin(config: Value) -> Result<WasmScriptRunner, String> {
    WasmScriptRunner::from_sources_with(&[source(config)], None).map_err(|e| e.to_string())
}

fn default_plugin() -> WasmScriptRunner {
    plugin(json!({})).unwrap()
}

fn entry(content: Option<&str>) -> FeedEntry {
    FeedEntry {
        id: None,
        feed_id: 1,
        syndication_format: "rss".to_string(),
        guid: "guid".to_string(),
        published_at: None,
        title: "title".to_string(),
        url: Some("https://example.com/post".to_string()),
        content: content.map(str::to_string),
        authors: vec![],
        categories: vec![],
        tags: vec![],
        cache_assets: true,
    }
}

/// The content `content` becomes, going through `runner`.
fn sanitize(runner: &dyn ScriptRunner, content: &str) -> String {
    runner
        .dispatch_transform_entry(entry(Some(content)))
        .unwrap()
        .unwrap()
        .content
        .unwrap()
}

/// Every new entry passes through the plugin, so it is never cut short.
#[test]
fn has_an_unlimited_time_budget() {
    let manifest = PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.time_budget(), TimeBudget::Unlimited);
}

#[test]
fn the_manifest_is_valid() {
    let manifest = PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.name, "sanitize");
    let names: Vec<_> = manifest.settings.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["elements", "drop", "attributes", "url_schemes"]);
    crate::plugins::settings::check_config(&manifest.settings, &manifest.config).unwrap();
}

#[test]
fn formatting_is_kept() {
    let runner = default_plugin();
    for html in [
        "<p>Some <em>text</em> and <strong>more</strong>.</p><ul><li>One</li></ul>",
        r#"<figure><img src="https://example.com/a.png" alt="A" width="640" height="480"><figcaption>A</figcaption></figure>"#,
        r#"<table><thead><tr><th scope="col">A</th></tr></thead><tbody><tr><td colspan="2">1</td></tr></tbody></table>"#,
        r#"<blockquote cite="https://example.com/q"><p lang="fr" title="t">Bonjour</p></blockquote>"#,
        r#"<pre><code>if a &lt; b &amp;&amp; c { }</code></pre>"#,
        r#"<p><a href="/relative">a</a> <a href="https://example.com/?a=1&amp;b=2">b</a> <a href="mailto:a@example.com">c</a></p>"#,
    ] {
        assert_eq!(sanitize(&runner, html), html);
    }
}

#[test]
fn entries_without_content_are_left_alone() {
    let runner = default_plugin();
    let out = runner
        .dispatch_transform_entry(entry(None))
        .unwrap()
        .unwrap();
    assert_eq!(out.content, None);
}

#[test]
fn dangerous_elements_are_removed_with_their_content() {
    let runner = default_plugin();
    let cases = [
        ("<p>a<script>alert(1)</script>b</p>", "<p>ab</p>"),
        ("<p>a<style>p { color: red }</style>b</p>", "<p>ab</p>"),
        (r#"<iframe src="https://evil.example"></iframe>x"#, "x"),
        (
            r#"<object data="x.swf"><param name="a" value="b"></object>x"#,
            "x",
        ),
        (r#"<embed src="x.swf">x"#, "x"),
        (
            "<svg><script>alert(1)</script><a href='javascript:x'>a</a></svg>x",
            "x",
        ),
        ("<math><mi>x</mi></math>y", "y"),
        (
            r#"<meta http-equiv="refresh" content="0;url=https://evil.example">x"#,
            "x",
        ),
        (r#"<base href="https://evil.example/">x"#, "x"),
        (r#"<link rel="stylesheet" href="x.css">x"#, "x"),
        ("<template><p>t</p></template>x", "x"),
    ];
    for (html, expected) in cases {
        assert_eq!(sanitize(&runner, html), expected, "{html}");
    }
}

/// Unwrapping an element whose content is raw text would turn that text
/// into markup, so such elements are removed outright, even when the
/// config asks to keep them.
#[test]
fn raw_text_elements_are_never_unwrapped_or_kept() {
    let runner =
        plugin(json!({ "elements": ["p", "script", "xmp", "noscript", "textarea", "title"] }))
            .unwrap();
    for html in [
        "<xmp><script>alert(1)</script></xmp>x",
        "<noscript><script>alert(1)</script></noscript>x",
        "<textarea><script>alert(1)</script></textarea>x",
        "<title><script>alert(1)</script></title>x",
        "<script>alert(1)</script>x",
    ] {
        assert_eq!(sanitize(&runner, html), "x", "{html}");
    }
}

#[test]
fn other_elements_are_unwrapped_or_dropped() {
    let runner = default_plugin();
    assert_eq!(
        sanitize(&runner, r#"<font color="red"><center>kept</center></font>"#),
        "kept"
    );
    assert_eq!(
        sanitize(&runner, r#"<form action="/x">text</form>"#),
        "text"
    );
    assert_eq!(
        sanitize(
            &runner,
            "<p>a<button>Click</button><video src='v.mp4'>Your browser</video>b</p>"
        ),
        "<p>ab</p>"
    );
    assert_eq!(sanitize(&runner, "<custom-element>c</custom-element>"), "c");

    // The lists can be changed.
    let runner = plugin(json!({ "elements": ["p", "video"], "drop": ["font"] })).unwrap();
    assert_eq!(
        sanitize(&runner, "<p><font>gone</font><video>v</video><b>b</b></p>"),
        "<p><video>v</video>b</p>"
    );
}

#[test]
fn attributes_not_allowed_are_removed() {
    let runner = default_plugin();
    assert_eq!(
        sanitize(
            &runner,
            r#"<p style="color:red" class="x" id="y" onclick="alert(1)" title="t">x</p>"#
        ),
        r#"<p title="t">x</p>"#
    );
    // Attributes allowed on one element are removed from the others.
    assert_eq!(
        sanitize(&runner, r#"<p href="/x" src="/y" alt="z">x</p>"#),
        "<p>x</p>"
    );
    assert_eq!(
        sanitize(
            &runner,
            r#"<img src="/a.png" onerror="alert(1)" srcset="/b.png 2x" loading="lazy">"#
        ),
        r#"<img src="/a.png">"#
    );
}

/// Browsers use the first of two attributes with the same name; neither
/// may get past the plugin unchecked.
#[test]
fn duplicate_attributes_are_all_checked() {
    let runner = default_plugin();
    assert_eq!(
        sanitize(&runner, r#"<p onclick="a()" title="t" ONCLICK="b()">x</p>"#),
        r#"<p title="t">x</p>"#
    );
    assert_eq!(
        sanitize(&runner, r#"<a href="javascript:x()" HREF="/ok">a</a>"#),
        r#"<a href="/ok">a</a>"#
    );
    assert_eq!(
        sanitize(&runner, r#"<a href="/ok" href="javascript:x()">a</a>"#),
        r#"<a href="/ok">a</a>"#
    );
}

#[test]
fn event_handlers_are_removed_even_when_configured() {
    let runner = plugin(json!({ "attributes": ["onclick", "p:onmouseover", "title"] })).unwrap();
    assert_eq!(
        sanitize(
            &runner,
            r#"<p onclick="a()" onmouseover="b()" title="t">x</p>"#
        ),
        r#"<p title="t">x</p>"#
    );
}

#[test]
fn urls_with_unsafe_schemes_are_removed() {
    let runner = default_plugin();
    for href in [
        "javascript:alert(1)",
        "JaVaScRiPt:alert(1)",
        " \t javascript:alert(1)",
        "\u{1}javascript:alert(1)",
        "java\tscr\nipt:alert(1)",
        "&#106;avascript:alert(1)",
        "&#106avascript:alert(1)",
        "&#x6A;avascript:alert(1)",
        "java&Tab;script:alert(1)",
        "javascript&colon;alert(1)",
        "vbscript:msgbox(1)",
        "data:text/html,<script>alert(1)</script>",
        "ftp://example.com/file",
    ] {
        let html = format!(r#"<a href="{href}" title="t">a</a>"#);
        assert_eq!(
            sanitize(&runner, &html),
            r#"<a title="t">a</a>"#,
            "{href:?}"
        );
    }
}

#[test]
fn relative_urls_and_allowed_schemes_are_kept() {
    let runner = default_plugin();
    for href in [
        "/about",
        "about",
        "../up",
        "?q=1",
        "#section",
        "//example.com/x",
        "https://example.com/",
        "HTTP://example.com/",
        "mailto:a@example.com",
        // No scheme: the colon comes after a slash.
        "./javascript:alert(1)",
    ] {
        let html = format!(r#"<a href="{href}">a</a>"#);
        assert_eq!(sanitize(&runner, &html), html, "{href:?}");
    }

    let runner = plugin(json!({ "url_schemes": ["https"] })).unwrap();
    assert_eq!(
        sanitize(&runner, r#"<a href="http://example.com/">a</a>"#),
        "<a>a</a>"
    );
}

#[test]
fn images_without_a_safe_source_are_removed() {
    let runner = default_plugin();
    assert_eq!(
        sanitize(
            &runner,
            r#"<p>a<img src="data:image/png;base64,AAAA" alt="x">b</p>"#
        ),
        "<p>ab</p>"
    );
    assert_eq!(
        sanitize(&runner, r#"<p>a<img src="javascript:alert(1)">b</p>"#),
        "<p>ab</p>"
    );
    assert_eq!(
        sanitize(&runner, r#"<p>a<img alt="no source">b</p>"#),
        "<p>ab</p>"
    );

    let runner = plugin(json!({ "url_schemes": ["http", "https", "data"] })).unwrap();
    assert_eq!(
        sanitize(&runner, r#"<img src="data:image/png;base64,AAAA">"#),
        r#"<img src="data:image/png;base64,AAAA">"#
    );
}

/// The attributes kept are written out from their decoded values, so what
/// a browser reads is what was checked.
#[test]
fn kept_attributes_are_written_out_again() {
    let runner = default_plugin();
    let cases = [
        (
            r#"<a href='/a?x=1&y=2'>a</a>"#,
            r#"<a href="/a?x=1&amp;y=2">a</a>"#,
        ),
        (
            r#"<a HREF=/a TITLE="it&#39;s">a</a>"#,
            r#"<a href="/a" title="it's">a</a>"#,
        ),
        (
            r#"<p title='say "hi"'>x</p>"#,
            r#"<p title="say &quot;hi&quot;">x</p>"#,
        ),
        // A reference the plugin leaves undecoded is escaped, so it can't
        // decode to anything else in a browser.
        (
            r#"<p title="&unknown;">x</p>"#,
            r#"<p title="&amp;unknown;">x</p>"#,
        ),
    ];
    for (html, expected) in cases {
        assert_eq!(sanitize(&runner, html), expected, "{html}");
    }
}

#[test]
fn comments_are_removed() {
    let runner = default_plugin();
    assert_eq!(
        sanitize(
            &runner,
            "a<!-- hidden -->b<!--[if IE]><script>x()</script><![endif]-->c"
        ),
        "abc"
    );
}

#[test]
fn srcset_is_checked_when_allowed() {
    let runner = plugin(json!({
        "attributes": ["img:src", "img:srcset"],
    }))
    .unwrap();
    assert_eq!(
        sanitize(
            &runner,
            r#"<img src="/a.png" srcset="/a.png 1x, https://example.com/b.png 2x">"#
        ),
        r#"<img src="/a.png" srcset="/a.png 1x, https://example.com/b.png 2x">"#
    );
    assert_eq!(
        sanitize(
            &runner,
            r#"<img src="/a.png" srcset="/a.png 1x, javascript:x 2x">"#
        ),
        r#"<img src="/a.png">"#
    );
}

#[test]
fn a_typical_post_is_cleaned_up() {
    let runner = default_plugin();
    let html = r#"<div class="post" style="margin:0"><p>Intro <a href="https://example.com/x" target="_blank" rel="noopener" onclick="track()">link</a>.</p>
<picture><source srcset="/a.webp" type="image/webp"><img src="/a.jpg" alt="Photo" class="wide" width="800" height="600" loading="lazy"></picture>
<script async src="https://ads.example/ad.js"></script><!-- ad slot --><div id="ad"><iframe src="https://ads.example/frame"></iframe></div>
<p>Outro<span style="display:none">hidden text</span></p></div>"#;
    assert_eq!(
        sanitize(&runner, html),
        r#"<div><p>Intro <a href="https://example.com/x">link</a>.</p>
<img src="/a.jpg" alt="Photo" width="800" height="600">
<div></div>
<p>Outro<span>hidden text</span></p></div>"#
    );
}

/// The plugin bounds the memory it uses: content it has no room to rewrite is reduced to
/// its text, cut short where the plugin runs out of room, rather than making it trap,
/// which would let the content through unsanitized.
#[test]
fn content_that_cannot_be_rewritten_is_reduced_to_text() {
    let runner = default_plugin();
    let text = "a".repeat(100);
    let unit = format!(r#"<p onclick="x()">{text} &amp; b<script>x()</script></p>"#);

    let html = unit.repeat(20_000);
    assert_eq!(
        sanitize(&runner, &html),
        format!("<p>{text} &amp; b</p>").repeat(20_000)
    );

    // About 11 MB, which with the rewritten HTML would need more than the plugin's 16 MiB.
    // Content this large only gets into the plugin when its memory has room for it in one
    // piece, as a fresh plugin's does: otherwise Kiki fails to hand it over, and the entry
    // passes through the plugin unchanged, as it did through the Lua plugin.
    let fresh = default_plugin();
    let html = unit.repeat(80_000);
    let out = sanitize(&fresh, &html);
    let as_text = format!("{text} &amp; bx()");
    assert!(out.starts_with(&as_text.repeat(2)), "{}", &out[..300]);
    assert!(!out.contains('<'));
    assert!(
        out.len() > 1024 * 1024 && out.len() < html.len(),
        "{}",
        out.len()
    );

    // The plugin still works afterwards.
    assert_eq!(sanitize(&fresh, "<p onclick=x>a</p>"), "<p>a</p>");
}

#[test]
fn invalid_configs_fail_to_load() {
    for config in [
        json!({ "elements": "p" }),
        json!({ "drop": [1] }),
        json!({ "attributes": [""] }),
        json!({ "url_schemes": {"a": 1} }),
    ] {
        let err = plugin(config.clone()).err().unwrap();
        assert!(err.to_string().contains("sanitize:"), "{config}: {err}");
    }
}

/// `privacy` runs before `sanitize`, in directory order, and `sanitize`
/// still cleans up what it leaves behind.
#[test]
fn sanitize_still_works_on_privacy_cleaned_content() {
    let manifest =
        PluginManifest::parse(include_str!("../../plugins/privacy/manifest.toml")).unwrap();
    let mut strip = ScriptSource::new(include_str!("../../plugins/privacy/main.lua"));
    strip.name = "privacy".to_string();
    strip.config = Value::Object(manifest.config).to_string();
    let runner = CompositeRunner::from_sources_with(&[strip, source(json!({}))], None).unwrap();

    assert_eq!(
        sanitize(
            &runner,
            r#"<p class="x"><a href='https://example.com/a?id=1&utm_source=rss'>a</a><img src="https://example.com/p.gif" width="1" height="1" style="border:0"></p>"#
        ),
        r#"<p><a href="https://example.com/a?id=1">a</a></p>"#
    );
}

/// How fast the plugin sanitizes entries of typical sizes. Run it with
///
/// ```text
/// cargo test --release --lib sanitize_bench -- --ignored --nocapture
/// ```
#[test]
#[ignore = "a benchmark; run it with --release --ignored --nocapture"]
fn sanitize_bench() {
    bench::run("sanitize", &default_plugin());
}

mod bench {
    #![allow(clippy::print_stdout)]

    use crate::scripting::ScriptRunner;
    use std::fmt::Write;
    use std::time::{Duration, Instant};

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

    /// Times `runner` sanitizing posts of several sizes, and prints how long
    /// one entry takes and the throughput, as the median of several runs.
    pub(super) fn run(label: &str, runner: &dyn ScriptRunner) {
        println!("\n{label}");
        println!(
            "{:>10} {:>10} {:>8} {:>12} {:>10}",
            "size", "out", "entries", "per entry", "MB/s"
        );
        for (paragraphs, entries) in [(2, 4000), (16, 1000), (128, 200), (1024, 25)] {
            let html = post(paragraphs);
            let out = runner
                .dispatch_transform_entry(super::entry(Some(&html)))
                .unwrap()
                .unwrap()
                .content
                .unwrap();
            // Warm up.
            for _ in 0..entries / 10 {
                runner
                    .dispatch_transform_entry(super::entry(Some(&html)))
                    .unwrap();
            }
            let mut runs: Vec<Duration> = (0..7)
                .map(|_| {
                    let start = Instant::now();
                    for _ in 0..entries {
                        runner
                            .dispatch_transform_entry(super::entry(Some(&html)))
                            .unwrap();
                    }
                    start.elapsed() / entries
                })
                .collect();
            runs.sort();
            let per_entry = runs[runs.len() / 2];
            let mb_per_sec = html.len() as f64 / per_entry.as_secs_f64() / 1e6;
            println!(
                "{:>9}B {:>9}B {:>8} {:>10.1}µs {:>10.1}",
                html.len(),
                out.len(),
                entries,
                per_entry.as_secs_f64() * 1e6,
                mb_per_sec
            );
        }
    }
}
