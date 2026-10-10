//! Benchmarks of dispatching `entry.ingest` to plugins: how many entries a second go
//! through a chain of plugins, run in this process or in the sandboxed script host as
//! `kiki serve` runs them, from one thread or from several at once.
//!
//! ```text
//! cargo bench --bench plugin_dispatch
//! ```
//!
//! Two chains of four plugins are measured:
//!
//! * `fixture`: the test fixture four times over, each appending to the entry's title,
//!   so that what is measured is mostly the cost of getting the entry to each plugin and
//!   back;
//! * `defaults`: the default plugins that handle `entry.ingest` (`filter`, `auto-tag`,
//!   `privacy` and `sanitize`), each with a rule or two, as a server runs them.
//!
//! Needs the `default-plugins` feature (on by default), which builds the plugins, and the
//! `kiki` binary, which runs the script host.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use kiki_rss::plugins::PluginManifest;
use kiki_rss::process::script_host::{ScriptHost, SubprocessScriptRunner};
use kiki_rss::scripting::wasm::WasmScriptRunner;
use kiki_rss::scripting::{FeedEntry, ScriptRunner, ScriptSource};
use serde_json::{json, Value};
use std::fmt::Write;
use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FIXTURE: &[u8] = include_bytes!("../tests/wasm-fixture/fixture.wasm");

macro_rules! default_plugin {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("../plugins/", $name, "/manifest.toml")),
            include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/", $name, ".wasm")).as_slice(),
        )
    };
}

/// The test fixture four times over, each appending to the entry's title.
fn fixture_chain() -> Vec<ScriptSource> {
    (1..=4)
        .map(|i| ScriptSource {
            name: format!("fixture-{i}"),
            config: json!({ "suffix": "!" }).to_string(),
            ..ScriptSource::new(FIXTURE.to_vec())
        })
        .collect()
}

/// The default plugins that handle `entry.ingest`, each with its default config and a
/// rule or two.
fn default_chain() -> Vec<ScriptSource> {
    let plugins = [
        (
            default_plugin!("filter"),
            json!({ "exclude": [{ "fields": ["title"], "pattern": r"\b(sponsored|webinar)\b" }] }),
        ),
        (
            default_plugin!("auto-tag"),
            json!({ "rules": [{ "tag": "rust", "fields": ["title", "content"], "pattern": r"\brust\b", "flags": "i" }] }),
        ),
        (default_plugin!("privacy"), json!({})),
        (default_plugin!("sanitize"), json!({})),
    ];
    plugins
        .into_iter()
        .map(|((name, manifest, wasm), overrides)| {
            let manifest = PluginManifest::parse(manifest).expect("the manifest is valid");
            let mut config = manifest.config.clone();
            // Scans need the server, which the benchmark doesn't run.
            config.insert("rescan".to_string(), Value::Bool(false));
            if let Value::Object(overrides) = overrides {
                config.extend(overrides);
            }
            ScriptSource {
                name: name.to_string(),
                config: Value::Object(config).to_string(),
                time_budget: manifest.time_budget(),
                ..ScriptSource::new(wasm.to_vec())
            }
        })
        .collect()
}

/// A script host running `sources`, started from the `kiki` binary Cargo built.
fn script_host(sources: &[ScriptSource]) -> SubprocessScriptRunner {
    let exe = Path::new(env!("CARGO_BIN_EXE_kiki"));
    let host = ScriptHost::spawn_exe(exe, false, false).expect("the script host starts");
    host.reload(sources.to_vec()).expect("the plugins load");
    SubprocessScriptRunner::new(Arc::new(host))
}

fn in_process(sources: &[ScriptSource]) -> WasmScriptRunner {
    WasmScriptRunner::from_sources_with(sources, None).expect("the plugins load")
}

/// HTML of about `bytes` bytes, mixing what `sanitize` and `privacy` keep with what they
/// remove.
fn html(bytes: usize) -> String {
    let mut html = String::from(r#"<div class="post">"#);
    let mut i = 0;
    while html.len() < bytes {
        write!(
            html,
            r#"<p class="p{i}">Lorem ipsum dolor sit amet, <a href="https://example.com/{i}?utm_source=rss&amp;id={i}" onclick="track({i})">consectetur</a> adipiscing elit, about <em>Rust</em>.</p><img src="https://cdn.example.com/{i}.jpg" srcset="/{i}@2x.jpg 2x" alt="Photo {i}"><script>track({i})</script>"#
        )
        .unwrap();
        i += 1;
    }
    html.push_str("</div>");
    html
}

fn entry(content: &str) -> FeedEntry {
    FeedEntry {
        id: None,
        feed_id: 1,
        syndication_format: "rss".to_string(),
        guid: "https://example.com/post".to_string(),
        published_at: Some(1_700_000_000),
        title: "A post about Rust".to_string(),
        url: Some("https://example.com/post?utm_source=rss".to_string()),
        content: Some(content.to_string()),
        authors: vec!["Ada".to_string()],
        categories: vec!["programming".to_string()],
        tags: vec![],
        cache_assets: true,
    }
}

/// Dispatch `iters` copies of `entry` to `runner` from `threads` threads at once, and
/// return how long it took per `iters` entries.
fn run(runner: &dyn ScriptRunner, entry: &FeedEntry, threads: u64, iters: u64) -> Duration {
    let per_thread = iters.div_ceil(threads);
    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                for _ in 0..per_thread {
                    let mut runs = Vec::new();
                    let mut entry = entry.clone();
                    black_box(
                        runner
                            .dispatch_transform_entry_timed(&mut entry, &mut runs)
                            .unwrap(),
                    );
                    black_box(entry);
                }
            });
        }
    });
    let done = per_thread * threads;
    start.elapsed().mul_f64(iters as f64 / done as f64)
}

fn dispatch(c: &mut Criterion) {
    let chains = [("fixture", fixture_chain()), ("defaults", default_chain())];
    for (chain, sources) in &chains {
        let runners: [(&str, Box<dyn ScriptRunner>); 2] = [
            ("in_process", Box::new(in_process(sources))),
            ("script_host", Box::new(script_host(sources))),
        ];
        for (where_, runner) in &runners {
            let mut group = c.benchmark_group(format!("entry_ingest/{chain}/{where_}"));
            group.measurement_time(Duration::from_secs(4));
            group.warm_up_time(Duration::from_secs(1));
            group.throughput(Throughput::Elements(1));
            for bytes in [2 * 1024, 64 * 1024] {
                let entry = entry(&html(bytes));
                for threads in [1, 4] {
                    group.bench_with_input(
                        BenchmarkId::new(format!("{}KiB", bytes / 1024), format!("{threads}t")),
                        &entry,
                        |b, entry| {
                            b.iter_custom(|iters| run(runner.as_ref(), entry, threads, iters))
                        },
                    );
                }
            }
            group.finish();
        }
    }
}

criterion_group!(benches, dispatch);
criterion_main!(benches);
