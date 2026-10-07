//! Compares the `filter` plugin in Lua (`plugins/filter`) with its port to Rust and
//! WebAssembly (`plugins/filter-wasm`).
//!
//! Both run in this process, through the same [`ScriptRunner`] calls the server makes, on
//! the same synthetic entries, so the numbers measure the engines and the plugins: not the
//! script host's IPC, which costs the same whichever engine is behind it. Run it with
//! optimizations:
//!
//! ```text
//! cargo run --profile profiling --example filter_bench [-- --entries N --rounds N --wasm PATH]
//! ```
//!
//! For each config, it reports how long the plugin takes to load, to pass an entry
//! through `entry.ingest`, and to rescan stored entries (as it does when its rules
//! change), and checks that both versions hide the same entries.

use kiki_rss::scripting::lua::LuaScriptRunner;
use kiki_rss::scripting::wasm::WasmScriptRunner;
use kiki_rss::scripting::{
    Event, EventPayload, FeedEntry, FeedInfo, ScriptRunner, ScriptServices, ScriptSource,
    ServiceCall, ServiceReply, WasmComponent,
};
use serde_json::{json, Value};
use std::hint::black_box;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const MAIN: &str = include_str!("../plugins/filter/main.lua");
const WASM: &[u8] = include_bytes!("../plugins/filter-wasm/plugin.wasm");

/// The WebAssembly plugin compared: `plugin.wasm`, or the build given with `--wasm`.
static WASM_BYTES: OnceLock<Vec<u8>> = OnceLock::new();

fn wasm() -> &'static [u8] {
    WASM_BYTES.get_or_init(|| WASM.to_vec())
}

/// The id `StartScan` answers with.
const SCAN: u64 = 1;

/// How many stored entries a scan hands the runner at a time.
const SCAN_BATCH: usize = 100;

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    Lua,
    Wasm,
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Engine::Lua => "Lua",
            Engine::Wasm => "WASM",
        }
    }
}

/// Answers the plugin's calls as a server with feeds 1 to 50 and an empty store would.
struct Server;

impl ScriptServices for Server {
    fn call(&self, _plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        Ok(match call {
            ServiceCall::StoreGet { .. } => ServiceReply::Value(None),
            ServiceCall::StoreSet { .. } => ServiceReply::Done,
            ServiceCall::StartScan { .. } => ServiceReply::ScanStarted(SCAN),
            ServiceCall::GetFeed { feed_id } => ServiceReply::Feed(Some(FeedInfo {
                id: feed_id,
                url: Some(format!("https://example.com/feed{feed_id}.xml")),
                title: format!("Feed {feed_id}"),
            })),
            other => return Err(format!("unexpected call {other:?}")),
        })
    }
}

fn load(engine: Engine, config: &Value) -> Box<dyn ScriptRunner> {
    let mut source = match engine {
        Engine::Lua => ScriptSource::new(MAIN),
        Engine::Wasm => ScriptSource::wasm(wasm().to_vec()),
    };
    source.name = "filter".to_string();
    source.config = config.to_string();
    let services = Some(Arc::new(Server) as Arc<dyn ScriptServices>);
    match engine {
        Engine::Lua => Box::new(LuaScriptRunner::from_sources_with(&[source], services).unwrap()),
        Engine::Wasm => Box::new(WasmScriptRunner::from_sources_with(&[source], services).unwrap()),
    }
}

/// A small deterministic generator, so that every run filters the same entries.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, words: &[&'a str]) -> &'a str {
        words[self.below(words.len())]
    }
}

const WORDS: &[&str] = &[
    "the",
    "of",
    "and",
    "a",
    "to",
    "in",
    "is",
    "you",
    "that",
    "it",
    "he",
    "was",
    "for",
    "on",
    "are",
    "as",
    "with",
    "his",
    "they",
    "at",
    "be",
    "this",
    "have",
    "from",
    "or",
    "one",
    "release",
    "kernel",
    "compiler",
    "server",
    "update",
    "security",
    "performance",
    "memory",
    "network",
    "database",
    "browser",
    "language",
    "library",
    "framework",
    "design",
    "garden",
    "climate",
    "policy",
    "election",
    "market",
    "science",
    "research",
    "museum",
    "recipe",
    "Rust",
    "Python",
    "Linux",
    "café",
    "naïve",
    "Zürich",
    "東京",
    "données",
];

const TITLE_EXTRAS: &[&str] = &["Sponsored:", "Webinar:", "Ask HN:", "Show HN:", "[video]"];

const CATEGORIES: &[&str] = &[
    "tech", "rust", "linux", "science", "politics", "promo", "food", "travel", "security",
];

/// `n` entries, from 50 feeds, with titles, authors and categories, and HTML content of
/// a few kilobytes, as a news or blog feed's would be.
fn entries(n: usize) -> Vec<FeedEntry> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..n)
        .map(|i| {
            let mut title = String::new();
            if rng.below(10) == 0 {
                title.push_str(rng.pick(TITLE_EXTRAS));
                title.push(' ');
            }
            for w in 0..6 + rng.below(8) {
                if w > 0 {
                    title.push(' ');
                }
                title.push_str(rng.pick(WORDS));
            }
            let mut content = String::new();
            for _ in 0..3 + rng.below(12) {
                content.push_str("<p>");
                for w in 0..40 + rng.below(80) {
                    if w > 0 {
                        content.push(' ');
                    }
                    if rng.below(40) == 0 {
                        content.push_str(r#"<a href="https://example.org/x">"#);
                        content.push_str(rng.pick(WORDS));
                        content.push_str("</a>");
                    } else {
                        content.push_str(rng.pick(WORDS));
                    }
                }
                content.push_str("</p>\n");
            }
            let feed_id = 1 + rng.below(50) as i64;
            let path = if rng.below(20) == 0 { "ads" } else { "posts" };
            FeedEntry {
                id: Some(i as i64 + 1),
                feed_id,
                syndication_format: "rss".to_string(),
                guid: format!("https://example.com/feed{feed_id}/{i}"),
                published_at: Some(1_700_000_000 + i as i64),
                title,
                url: Some(format!("https://example.com/{path}/{i}")),
                content: Some(content),
                authors: (0..1 + rng.below(3))
                    .map(|_| format!("Author {}", rng.below(200)))
                    .collect(),
                categories: (0..rng.below(5))
                    .map(|_| rng.pick(CATEGORIES).to_string())
                    .collect(),
                tags: Vec::new(),
                cache_assets: true,
            }
        })
        .collect()
}

/// The configs compared: no rules, as the filter is installed; a handful, as most users
/// would write; and many, matching the content of every entry.
fn configs() -> Vec<(&'static str, Value)> {
    let heavy_words = [
        "bitcoin",
        "crypto",
        "nft",
        "casino",
        "giveaway",
        "discount",
        "coupon",
        "horoscope",
        "celebrity",
        "gossip",
        "lottery",
        "diet",
        "keto",
        "influencer",
        "clickbait",
        "you won't believe",
        "doctors hate",
        "one weird trick",
        "limited time",
        "act now",
    ];
    let mut heavy_exclude: Vec<Value> = heavy_words
        .iter()
        .map(|w| json!({"pattern": format!(r"\b{w}\b"), "flags": "i"}))
        .collect();
    heavy_exclude.push(json!({"fields": ["authors"], "pattern": r"^Author 1\d\d$"}));
    let heavy_include: Vec<Value> = (1..=5)
        .map(|n| {
            json!({
                "fields": ["title", "categories"],
                "pattern": "(?i)rust|linux|security",
                "feeds": [format!("https://example.com/feed{n}.xml")],
            })
        })
        .collect();
    vec![
        ("no rules", json!({"exclude": [], "include": []})),
        (
            "typical (4 rules)",
            json!({
                "exclude": [
                    {"fields": ["title"], "pattern": r"\b(sponsored|webinar)\b", "flags": "i"},
                    {"fields": ["url"], "pattern": "/ads?/"},
                    {"fields": ["categories"], "pattern": "^(promo|advert)$", "flags": "i"},
                ],
                "include": [
                    {"fields": ["title", "categories"], "pattern": "(?i)rust", "feeds": [3]},
                ],
            }),
        ),
        (
            "heavy (26 rules, content)",
            json!({"exclude": heavy_exclude, "include": heavy_include}),
        ),
    ]
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

struct Measurement {
    load: Duration,
    ingest: Duration,
    scan: Option<Duration>,
    hidden: Vec<bool>,
}

fn is_hidden(entry: &FeedEntry) -> bool {
    entry.tags.iter().any(|t| t == "system:hidden")
}

fn measure(engine: Engine, config: &Value, entries: &[FeedEntry], rounds: usize) -> Measurement {
    let load_time = median(
        (0..rounds)
            .map(|_| {
                let start = Instant::now();
                black_box(load(engine, config));
                start.elapsed()
            })
            .collect(),
    );

    let runner = load(engine, config);
    // Warm up, and record what the plugin hides.
    let hidden: Vec<bool> = entries
        .iter()
        .map(|e| {
            let out = runner.dispatch_transform_entry(e.clone()).unwrap();
            out.as_ref().is_some_and(is_hidden)
        })
        .collect();
    let ingest = median(
        (0..rounds)
            .map(|_| {
                // Cloning is the caller's cost, so is left out of the time.
                let batch = entries.to_vec();
                let start = Instant::now();
                for entry in batch {
                    black_box(runner.dispatch_transform_entry(entry).unwrap());
                }
                start.elapsed() / entries.len() as u32
            })
            .collect(),
    );

    // With an empty store, loading starts a rescan if there are rules to apply.
    let scan_time = |runner: &dyn ScriptRunner| -> Option<Duration> {
        runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
        let mut scan_hidden = Vec::with_capacity(entries.len());
        let mut elapsed = Duration::ZERO;
        for chunk in entries.chunks(SCAN_BATCH) {
            let mut rest = chunk.to_vec();
            while !rest.is_empty() {
                let start = Instant::now();
                let out = runner.dispatch_scan(SCAN, rest.clone()).unwrap()?;
                elapsed += start.elapsed();
                scan_hidden.extend(out.iter().map(|e| e.as_ref().is_some_and(is_hidden)));
                rest.drain(..out.len());
            }
        }
        assert_eq!(scan_hidden, hidden, "the rescan hid different entries");
        Some(elapsed / entries.len() as u32)
    };
    let scan = (0..rounds)
        .map(|_| scan_time(&*load(engine, config)))
        .collect::<Option<Vec<_>>>()
        .map(median);

    Measurement {
        load: load_time,
        ingest,
        scan,
        hidden,
    }
}

fn arg(args: &[String], name: &str, default: usize) -> usize {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|v| v.parse().expect("expected a number"))
        .unwrap_or(default)
}

fn fmt(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us >= 1000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{us:.2} µs")
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n = arg(&args, "--entries", 2000);
    let rounds = arg(&args, "--rounds", 7);
    if let Some(path) = args
        .iter()
        .position(|a| a == "--wasm")
        .and_then(|i| args.get(i + 1))
    {
        WASM_BYTES
            .set(std::fs::read(path).expect("reading --wasm"))
            .unwrap();
    }
    let entries = entries(n);
    let bytes: usize = entries
        .iter()
        .map(|e| e.title.len() + e.content.as_ref().map_or(0, String::len))
        .sum();
    println!(
        "{n} entries, {} KiB of titles and content (mean {:.1} KiB), median of {rounds} rounds\n",
        bytes / 1024,
        bytes as f64 / n as f64 / 1024.0
    );

    // The script host compiles each component to native code once, and later loads find
    // it compiled: so the loads measured below are the ones on every reload but the first.
    let start = Instant::now();
    kiki_rss::scripting::wasm::put_component("filter", &WasmComponent::new(wasm().to_vec()))
        .unwrap();
    println!(
        "WASM component compiled to native code ({} KiB, once per component): {}\n",
        wasm().len() / 1024,
        fmt(start.elapsed())
    );

    println!(
        "| config | engine | load | ingest / entry | ingest entries/s | rescan / entry | hidden |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|");
    for (name, config) in configs() {
        let lua = measure(Engine::Lua, &config, &entries, rounds);
        let wasm = measure(Engine::Wasm, &config, &entries, rounds);
        assert_eq!(
            lua.hidden, wasm.hidden,
            "{name}: the engines hid different entries"
        );
        for (engine, r) in [(Engine::Lua, &lua), (Engine::Wasm, &wasm)] {
            println!(
                "| {name} | {} | {} | {} | {:.0} | {} | {} |",
                engine.name(),
                fmt(r.load),
                fmt(r.ingest),
                1.0 / r.ingest.as_secs_f64(),
                r.scan.map_or("n/a".to_string(), fmt),
                r.hidden.iter().filter(|h| **h).count(),
            );
        }
        let ratio = |a: Duration, b: Duration| a.as_secs_f64() / b.as_secs_f64();
        println!(
            "| {name} | **WASM speedup** | {:.2}× | {:.2}× | | {} | |",
            ratio(lua.load, wasm.load),
            ratio(lua.ingest, wasm.ingest),
            match (lua.scan, wasm.scan) {
                (Some(l), Some(w)) => format!("{:.2}×", ratio(l, w)),
                _ => "n/a".to_string(),
            },
        );
    }
}
