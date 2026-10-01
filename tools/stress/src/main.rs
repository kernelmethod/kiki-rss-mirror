//! Out-of-process stress harness for `kiki serve`.
//!
//! Starts a local feed server and a Kiki server, then hammers Kiki's API
//! with a weighted mix of reads and writes while the fetch scheduler is
//! ingesting continuously. Reports per-endpoint latency, a per-second
//! timeline, and grabs gdb backtraces of Kiki when something stalls.
//!
//! Configured by environment variables; see `Config::from_env`.

use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    routing::get,
    Router,
};
use reqwest::{Client, Method};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BASE: &str = "http://kiki";

struct Config {
    kiki: PathBuf,
    home: PathBuf,
    out: PathBuf,
    feeds: usize,
    workers: usize,
    secs: u64,
    /// Feeds whose number is divisible by this respond slowly.
    slow_every: u64,
    /// Extra arguments passed to `kiki serve`.
    extra_args: Vec<String>,
    stall_ms: u128,
    /// Drive the web UI (`kiki web`) rather than the API (`kiki serve`).
    web: bool,
    /// Extra `filter` and `auto-tag` plugin rules (TOML) for web mode.
    filter_toml: Option<PathBuf>,
    autotag_toml: Option<PathBuf>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        Ok(Self {
            kiki: var("KIKI_BIN", "kiki").into(),
            home: std::env::var("STRESS_HOME").context("STRESS_HOME")?.into(),
            out: std::env::var("STRESS_OUT").context("STRESS_OUT")?.into(),
            feeds: var("FEEDS", "300").parse()?,
            workers: var("WORKERS", "64").parse()?,
            secs: var("SECS", "60").parse()?,
            slow_every: var("SLOW_EVERY", "20").parse()?,
            extra_args: var("KIKI_ARGS", "")
                .split_whitespace()
                .map(String::from)
                .collect(),
            stall_ms: var("STALL_MS", "5000").parse()?,
            web: match var("MODE", "api").as_str() {
                "api" => false,
                "web" => true,
                m => bail!("MODE must be `api` or `web`, not {m:?}"),
            },
            filter_toml: std::env::var_os("FILTER_TOML").map(PathBuf::from),
            autotag_toml: std::env::var_os("AUTOTAG_TOML").map(PathBuf::from),
        })
    }
}

// ---------------------------------------------------------------------------
// Feed server
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone)]
struct FeedState {
    addr: String,
    slow_every: u64,
}

type Reply = (StatusCode, [(header::HeaderName, &'static str); 1], Vec<u8>);

async fn feed(Path(name): Path<String>, State(st): State<FeedState>) -> Reply {
    let n: u64 = name.trim_end_matches(".xml").parse().unwrap_or(0);
    if st.slow_every > 0 && n.is_multiple_of(st.slow_every) {
        tokio::time::sleep(Duration::from_millis(2000 + (n * 377) % 6000)).await;
    }
    if n % 20 == 1 {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain")],
            b"boom".to_vec(),
        );
    }
    // Every ten seconds, five new items appear at the top of each feed.
    let count: u64 = if n % 20 == 2 { 1500 } else { 40 };
    let first = (now_secs() / 10) * 5;
    let mut s = String::with_capacity(count as usize * 600);
    let _ = write!(
        s,
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Stress {n}</title><link>http://{a}/</link><description>stress feed {n}</description>"#,
        a = st.addr
    );
    for i in (first..first + count).rev() {
        let _ = write!(
            s,
            r#"<item><title>Item {i} of feed {n}</title><link>http://{a}/post/{n}/{i}?utm_source=stress&amp;id={i}</link><guid>stress-{n}-{i}</guid><description><![CDATA[<p>Lorem ipsum dolor sit amet {i}, consectetur adipiscing elit. Feed {n} says hello.</p><img src="http://{a}/img/{k}.png"><a href="http://{a}/x?utm_medium=y&fbclid=z">more</a>]]></description></item>"#,
            a = st.addr,
            k = i % 400
        );
    }
    s.push_str("</channel></rss>");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/rss+xml")],
        s.into_bytes(),
    )
}

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

async fn img(Path(name): Path<String>) -> Reply {
    let k: u32 = name.trim_end_matches(".png").parse().unwrap_or(0);
    // Distinct bytes after IEND so each image hashes differently.
    let mut body = PNG.to_vec();
    body.extend_from_slice(&k.to_le_bytes());
    body.extend(std::iter::repeat_n(0u8, 2048));
    (StatusCode::OK, [(header::CONTENT_TYPE, "image/png")], body)
}

async fn start_feed_server(slow_every: u64) -> Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let app = Router::new()
        .route("/feed/{name}", get(feed))
        .route("/img/{name}", get(img))
        .route("/favicon.ico", get(|| img(Path("favicon.png".into()))))
        .route("/post/{a}/{b}", get(|| async { "post" }))
        .with_state(FeedState {
            addr: addr.clone(),
            slow_every,
        });
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(addr)
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Default)]
struct OpStats {
    lat_us: Vec<u64>,
    status: BTreeMap<String, u64>,
}

#[derive(Default, Clone)]
struct Bucket {
    done: u64,
    errors: u64,
    timeouts: u64,
    max_ms: u64,
    health_max_ms: u64,
    write_canary_max_ms: u64,
}

struct Shared {
    start: Instant,
    stop: AtomicBool,
    kill: AtomicBool,
    next_id: AtomicU64,
    stats: Mutex<BTreeMap<&'static str, OpStats>>,
    inflight: Mutex<HashMap<u64, (&'static str, Instant)>>,
    timeline: Mutex<Vec<Bucket>>,
    events: Mutex<Vec<String>>,
    feeds: Mutex<Vec<i64>>,
    entries: Mutex<Vec<i64>>,
    tags: Mutex<Vec<(i64, String)>>,
    read_tag: AtomicU64,
    dumps: AtomicU64,
    /// The process started: `kiki serve`, or `kiki web` in web mode.
    pid: u32,
    /// In web mode, the `kiki serve` child of `kiki web`.
    server_pid: Option<u32>,
    /// In web mode, the web UI's base URL, e.g. `http://127.0.0.1:1234`.
    web_base: String,
    /// In web mode, the tags whose pages are browsed.
    web_tags: Mutex<Vec<i64>>,
    /// In web mode, the `ETag` of each asset fetched so far, as a browser
    /// cache would keep it.
    etags: Mutex<HashMap<String, String>>,
    out: PathBuf,
    feed_base: String,
    stall_ms: u128,
}

impl Shared {
    fn bucket<F: FnOnce(&mut Bucket)>(&self, f: F) {
        let i = self.start.elapsed().as_secs() as usize;
        let mut tl = self.timeline.lock().unwrap();
        if tl.len() <= i {
            tl.resize(i + 1, Bucket::default());
        }
        f(&mut tl[i]);
    }

    fn event(&self, msg: String) {
        let line = format!("[t+{:6.1}s] {msg}", self.start.elapsed().as_secs_f64());
        eprintln!("{line}");
        self.events.lock().unwrap().push(line);
    }

    /// Attach gdb to Kiki and save every thread's backtrace.
    fn dump(&self, why: &str) {
        let n = self.dumps.fetch_add(1, Ordering::SeqCst);
        if n >= 4 {
            return;
        }
        let path = self.out.join(format!("stack-{n}.txt"));
        self.event(format!(
            "STALL: {why}; dumping threads to {}",
            path.display()
        ));
        let mut text = format!("# {why}\n");
        for pid in std::iter::once(self.pid).chain(self.server_pid) {
            let out = std::process::Command::new("gdb")
                .args(["-p", &pid.to_string(), "-batch", "-nx"])
                .args(["-ex", "set pagination off", "-ex", "thread apply all bt 40"])
                .output();
            match out {
                Ok(o) => {
                    let _ = writeln!(text, "## pid {pid}");
                    text.push_str(&String::from_utf8_lossy(&o.stdout));
                    text.push_str(&String::from_utf8_lossy(&o.stderr));
                }
                Err(e) => self.event(format!("gdb failed: {e}")),
            }
        }
        let _ = std::fs::write(&path, text);
    }

    fn pick<T: Clone>(v: &Mutex<Vec<T>>, r: u64) -> Option<T> {
        let v = v.lock().unwrap();
        if v.is_empty() {
            None
        } else {
            Some(v[(r as usize) % v.len()].clone())
        }
    }
}

/// Send one request to the API, recording its latency and outcome under
/// `op`, and return its JSON body if it succeeded.
async fn call(
    sh: &Shared,
    client: &Client,
    op: &'static str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Option<Value> {
    let mut rb = client.request(method, format!("{BASE}{path}"));
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let (status, _, bytes) = send(sh, op, path, rb).await;
    if status < 300 {
        bytes.and_then(|b| serde_json::from_slice(&b).ok())
    } else {
        None
    }
}

/// Send `rb`, recording its latency and outcome under `op`, and return
/// its status (`0` if there was no response), headers and body.
async fn send(
    sh: &Shared,
    op: &'static str,
    path: &str,
    rb: reqwest::RequestBuilder,
) -> (u16, reqwest::header::HeaderMap, Option<Vec<u8>>) {
    let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
    let t = Instant::now();
    sh.inflight.lock().unwrap().insert(id, (op, t));
    let (key, status, headers, bytes) = match rb.send().await {
        Ok(r) => {
            let s = r.status().as_u16();
            let headers = r.headers().clone();
            let bytes = r.bytes().await.ok().map(|b| b.to_vec());
            (s.to_string(), s, headers, bytes)
        }
        Err(e) if e.is_timeout() => ("client-timeout".into(), 0, Default::default(), None),
        Err(_) => ("conn-err".into(), 0, Default::default(), None),
    };
    let lat = t.elapsed();
    sh.inflight.lock().unwrap().remove(&id);
    let is_err = !(key.starts_with('2') || key.starts_with('3')) && key != "404" && key != "409";
    let is_timeout = key == "408" || key == "client-timeout";
    {
        let mut st = sh.stats.lock().unwrap();
        let e = st.entry(op).or_default();
        e.lat_us.push(lat.as_micros() as u64);
        *e.status.entry(key.clone()).or_default() += 1;
    }
    let ms = lat.as_millis() as u64;
    sh.bucket(|b| {
        b.done += 1;
        b.errors += is_err as u64;
        b.timeouts += is_timeout as u64;
        b.max_ms = b.max_ms.max(ms);
        if op == "canary_health" {
            b.health_max_ms = b.health_max_ms.max(ms);
        }
        if op == "canary_write" {
            b.write_canary_max_ms = b.write_canary_max_ms.max(ms);
        }
    });
    if is_timeout {
        sh.event(format!("{op} {path} -> {key} after {ms}ms"));
        if sh.dumps.load(Ordering::SeqCst) == 0 {
            sh.dump(&format!("{op} {path} -> {key}"));
        }
    }
    (status, headers, bytes)
}

// ---------------------------------------------------------------------------
// Workload
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const MIX: &[(&str, u32)] = &[
    ("list_entries", 20),
    ("list_feeds", 6),
    ("feed_entries", 10),
    ("get_entry", 8),
    ("batch_entries", 5),
    ("search_fts", 6),
    ("search_regex", 3),
    ("search_tags", 3),
    ("mark_read", 12),
    ("mark_unread", 5),
    ("set_entry_tags", 5),
    ("mark_feed_read", 2),
    ("refresh_feed", 4),
    ("refresh_all", 1),
    ("update_feed", 2),
    ("set_feed_tags", 2),
    ("feed_churn", 1),
    ("export_opml", 1),
    ("cleanup", 1),
    ("list_tags", 2),
    ("tag_entries", 2),
    ("entry_assets", 2),
    ("feed_favicon", 1),
];

async fn worker(sh: Arc<Shared>, client: Client, seed: u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    while !sh.stop.load(Ordering::Relaxed) {
        let op = pick_op(MIX, &mut rng);
        let r = rng.next();
        let f = Shared::pick(&sh.feeds, r).unwrap_or(1);
        let e = Shared::pick(&sh.entries, r >> 8).unwrap_or(1);
        let (t, tname) = Shared::pick(&sh.tags, r >> 16).unwrap_or((1, "x".into()));
        let c = &client;
        use Method as M;
        match op {
            "list_entries" => {
                call(
                    &sh,
                    c,
                    op,
                    M::GET,
                    &format!("/v1/entries?limit=50&offset={}", r % 400),
                    None,
                )
                .await;
            }
            "list_feeds" => {
                call(&sh, c, op, M::GET, "/v1/feeds", None).await;
            }
            "feed_entries" => {
                call(
                    &sh,
                    c,
                    op,
                    M::GET,
                    &format!("/v1/feeds/id/{f}/entries?limit=50"),
                    None,
                )
                .await;
            }
            "get_entry" => {
                call(&sh, c, op, M::GET, &format!("/v1/entries/id/{e}"), None).await;
            }
            "batch_entries" => {
                let ids: Vec<i64> = (0..50)
                    .filter_map(|i| Shared::pick(&sh.entries, r.wrapping_add(i * 7919)))
                    .collect();
                call(
                    &sh,
                    c,
                    op,
                    M::POST,
                    "/v1/entries/batch",
                    Some(json!({ "ids": ids })),
                )
                .await;
            }
            "search_fts" => {
                let q = ["lorem", "hello", "consectetur", "feed"][(r % 4) as usize];
                call(
                    &sh,
                    c,
                    op,
                    M::POST,
                    "/v1/entries/search",
                    Some(json!({ "query": q, "limit": 50 })),
                )
                .await;
            }
            "search_regex" => {
                call(
                    &sh,
                    c,
                    op,
                    M::POST,
                    "/v1/entries/search",
                    Some(json!({ "title_regex": format!("Item [0-9]*{}$", r % 10), "limit": 50 })),
                )
                .await;
            }
            "search_tags" => {
                call(&sh, c, op, M::POST, "/v1/entries/search",
                    Some(json!({ "tags": { "or": [tname, "stress-tag-1"] }, "content_glob": "*ipsum*", "limit": 50 }))).await;
            }
            "mark_read" => {
                call(
                    &sh,
                    c,
                    op,
                    M::PUT,
                    &format!("/v1/entries/id/{e}/system-tags/read"),
                    None,
                )
                .await;
            }
            "mark_unread" => {
                call(
                    &sh,
                    c,
                    op,
                    M::DELETE,
                    &format!("/v1/entries/id/{e}/system-tags/read"),
                    None,
                )
                .await;
            }
            "set_entry_tags" => {
                call(
                    &sh,
                    c,
                    op,
                    M::PUT,
                    &format!("/v1/entries/id/{e}/tags"),
                    Some(json!({ "tag_ids": [t] })),
                )
                .await;
            }
            "mark_feed_read" => {
                let rt = sh.read_tag.load(Ordering::Relaxed);
                call(
                    &sh,
                    c,
                    op,
                    M::POST,
                    &format!("/v1/tags/id/{rt}/entries"),
                    Some(json!({ "feed_id": f })),
                )
                .await;
            }
            "refresh_feed" => {
                call(&sh, c, op, M::POST, &format!("/v1/feeds/refresh/{f}"), None).await;
            }
            "refresh_all" => {
                call(&sh, c, op, M::POST, "/v1/feeds/refresh", None).await;
            }
            "update_feed" => {
                call(
                    &sh,
                    c,
                    op,
                    M::PUT,
                    &format!("/v1/feeds/id/{f}"),
                    Some(json!({ "title": format!("Renamed {f} {}", r % 100) })),
                )
                .await;
            }
            "set_feed_tags" => {
                call(
                    &sh,
                    c,
                    op,
                    M::PUT,
                    &format!("/v1/feeds/id/{f}/tags"),
                    Some(json!({ "tag_ids": [t] })),
                )
                .await;
            }
            "feed_churn" => {
                let url = format!(
                    "http://{}/feed/{}.xml",
                    sh.feed_base,
                    100_000 + r % 1000 * 20 + 3
                );
                let created = call(
                    &sh,
                    c,
                    "feed_create",
                    M::POST,
                    "/v1/feeds/create",
                    Some(json!({ "title": "churn", "url": url })),
                )
                .await;
                if let Some(id) = created.and_then(|v| v["id"].as_i64()) {
                    call(
                        &sh,
                        c,
                        "feed_refresh_new",
                        M::POST,
                        &format!("/v1/feeds/refresh/{id}"),
                        None,
                    )
                    .await;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    call(
                        &sh,
                        c,
                        "feed_delete",
                        M::DELETE,
                        &format!("/v1/feeds/id/{id}"),
                        None,
                    )
                    .await;
                }
            }
            "export_opml" => {
                call(&sh, c, op, M::GET, "/v1/feeds/export", None).await;
            }
            "cleanup" => {
                call(&sh, c, op, M::POST, "/v1/entries/cleanup", None).await;
            }
            "list_tags" => {
                call(&sh, c, op, M::GET, "/v1/tags", None).await;
            }
            "tag_entries" => {
                call(
                    &sh,
                    c,
                    op,
                    M::GET,
                    &format!("/v1/tags/id/{t}/entries?limit=50"),
                    None,
                )
                .await;
            }
            "entry_assets" => {
                call(
                    &sh,
                    c,
                    op,
                    M::GET,
                    &format!("/v1/entries/id/{e}/assets"),
                    None,
                )
                .await;
            }
            "feed_favicon" => {
                call(
                    &sh,
                    c,
                    op,
                    M::GET,
                    &format!("/v1/feeds/id/{f}/favicon"),
                    None,
                )
                .await;
            }
            _ => unreachable!(),
        }
    }
}

/// The web UI workload: what one person reading in a browser does, from
/// most to least often. Listing pages and entry pages also load the
/// images on them, as `web_asset`.
const WEB_MIX: &[(&str, u32)] = &[
    ("web_index", 30),
    ("web_index_read", 3),
    ("web_feed", 10),
    ("web_tag", 6),
    ("web_entry", 15),
    ("web_swipe_read", 15),
    ("web_swipe_undo", 2),
    ("web_save", 4),
    ("web_search", 5),
    ("web_feeds", 3),
    ("web_tags", 2),
    ("web_mark_feed_read", 1),
    ("web_plugins", 1),
];

fn pick_op(mix: &[(&'static str, u32)], rng: &mut Rng) -> &'static str {
    let total: u32 = mix.iter().map(|(_, w)| w).sum();
    let mut roll = (rng.next() % total as u64) as u32;
    for (op, w) in mix {
        if roll < *w {
            return op;
        }
        roll -= w;
    }
    mix[0].0
}

/// A listing's first page is opened far more often than later ones.
fn pick_page(r: u64) -> u64 {
    match r % 10 {
        0..=6 => 1,
        7 | 8 => 2,
        _ => 3 + r % 3,
    }
}

/// The distinct `/assets/{hash}` URLs an HTML page links to.
fn asset_links(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, _) in html.match_indices("/assets/") {
        let hash: String = html[i + 8..]
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        if hash.len() == 64 {
            let url = format!("/assets/{hash}");
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

/// Request `path` from the web UI, recording it under `op`, and return the
/// body if it succeeded. Changes carry `Sec-Fetch-Site: same-origin`, as
/// the UI's own script's requests do.
async fn web_call(
    sh: &Shared,
    client: &Client,
    op: &'static str,
    method: Method,
    path: &str,
) -> Option<String> {
    let mut rb = client.request(method.clone(), format!("{}{path}", sh.web_base));
    if method != Method::GET {
        rb = rb.header("sec-fetch-site", "same-origin");
    }
    let (status, _, body) = send(sh, op, path, rb).await;
    (status < 300).then(|| String::from_utf8_lossy(&body.unwrap_or_default()).into_owned())
}

/// Load the images on a page, as a browser with a warm cache does:
/// revalidating the ones it has an `ETag` for.
async fn web_assets(sh: &Shared, client: &Client, html: &str) {
    for path in asset_links(html) {
        let mut rb = client.get(format!("{}{path}", sh.web_base));
        let known = sh.etags.lock().unwrap().get(&path).cloned();
        if let Some(etag) = known {
            rb = rb.header("if-none-match", etag);
        }
        let (status, headers, _) = send(sh, "web_asset", &path, rb).await;
        if status == 200 {
            if let Some(etag) = headers.get("etag").and_then(|v| v.to_str().ok()) {
                sh.etags.lock().unwrap().insert(path, etag.to_owned());
            }
        }
    }
}

async fn web_worker(sh: Arc<Shared>, client: Client, seed: u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    while !sh.stop.load(Ordering::Relaxed) {
        let op = pick_op(WEB_MIX, &mut rng);
        let r = rng.next();
        let f = Shared::pick(&sh.feeds, r).unwrap_or(1);
        let e = Shared::pick(&sh.entries, r >> 8).unwrap_or(1);
        let t = Shared::pick(&sh.web_tags, r >> 16).unwrap_or(1);
        let page = pick_page(r >> 24);
        let c = &client;
        use Method as M;
        let html = match op {
            "web_index" => web_call(&sh, c, op, M::GET, &format!("/?page={page}")).await,
            "web_index_read" => {
                web_call(&sh, c, op, M::GET, &format!("/?show_read=true&page={page}")).await
            }
            "web_feed" => web_call(&sh, c, op, M::GET, &format!("/feeds/{f}?page={page}")).await,
            "web_tag" => web_call(&sh, c, op, M::GET, &format!("/tags/{t}?page={page}")).await,
            "web_entry" => web_call(&sh, c, op, M::GET, &format!("/entries/{e}?page=1")).await,
            "web_swipe_read" | "web_swipe_undo" => {
                let m = if op == "web_swipe_read" {
                    M::PUT
                } else {
                    M::DELETE
                };
                web_call(&sh, c, op, m, &format!("/entries/{e}/system-tags/read")).await;
                None
            }
            "web_save" => {
                let m = if r.is_multiple_of(2) {
                    M::PUT
                } else {
                    M::DELETE
                };
                web_call(&sh, c, op, m, &format!("/entries/{e}/system-tags/saved")).await;
                None
            }
            "web_search" => {
                let q = ["lorem", "hello", "consectetur", "feed 7"][(r % 4) as usize];
                let sort = if r.is_multiple_of(3) {
                    "&sort=newest"
                } else {
                    ""
                };
                let path = format!("/search?q={}{sort}&page={page}", q.replace(' ', "+"));
                web_call(&sh, c, op, M::GET, &path).await
            }
            "web_feeds" => web_call(&sh, c, op, M::GET, &format!("/feeds?page={page}")).await,
            "web_tags" => web_call(&sh, c, op, M::GET, "/tags").await,
            "web_mark_feed_read" => {
                web_call(&sh, c, op, M::POST, &format!("/entries/read?feed={f}")).await;
                None
            }
            "web_plugins" => web_call(&sh, c, op, M::GET, "/plugins").await,
            _ => unreachable!(),
        };
        if let Some(html) = html {
            web_assets(&sh, c, &html).await;
        }
    }
}

/// Keeps the IDs of the tags the web workers browse up to date, as the
/// `auto-tag` plugin creates them.
async fn tag_sampler(sh: Arc<Shared>, client: Client) {
    while !sh.stop.load(Ordering::Relaxed) {
        if let Some(v) = call(
            &sh,
            &client,
            "sample_tags",
            Method::GET,
            "/v1/tags?limit=1000",
            None,
        )
        .await
        {
            let ids: Vec<i64> = v["tags"]
                .as_array()
                .map(|a| a.iter().filter_map(|t| t["id"].as_i64()).collect())
                .unwrap_or_default();
            if !ids.is_empty() {
                *sh.web_tags.lock().unwrap() = ids;
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Keeps a sample of live entry IDs for the workers to act on.
async fn entry_sampler(sh: Arc<Shared>, client: Client) {
    while !sh.stop.load(Ordering::Relaxed) {
        if let Some(v) = call(
            &sh,
            &client,
            "sample_entries",
            Method::GET,
            "/v1/entries?limit=500",
            None,
        )
        .await
        {
            let ids: Vec<i64> = v["entries"]
                .as_array()
                .map(|a| a.iter().filter_map(|e| e["id"].as_i64()).collect())
                .unwrap_or_default();
            if !ids.is_empty() {
                *sh.entries.lock().unwrap() = ids;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Probes the read pool (health) and the writer (toggle one system tag)
/// at a fixed rate on independent loops, so a stall in one cannot hide
/// the other.
async fn canaries(sh: Arc<Shared>, client: Client) {
    let (sh2, c2) = (sh.clone(), client.clone());
    let health = tokio::spawn(async move {
        while !sh2.stop.load(Ordering::Relaxed) {
            call(&sh2, &c2, "canary_health", Method::GET, "/v1/health", None).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    let mut on = false;
    while !sh.stop.load(Ordering::Relaxed) {
        if let Some(e) = Shared::pick(&sh.entries, 0) {
            let m = if on { Method::DELETE } else { Method::PUT };
            on = !on;
            call(
                &sh,
                &client,
                "canary_write",
                m,
                &format!("/v1/entries/id/{e}/system-tags/saved"),
                None,
            )
            .await;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = health.await;
}

/// Watches in-flight requests; one older than the stall threshold means
/// something is wedged, so take a thread dump.
async fn watchdog(sh: Arc<Shared>, mut child: tokio::process::Child) {
    let mut worst_seen = 0u128;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(Some(status)) = child.try_wait() {
            sh.event(format!("KIKI EXITED: {status}"));
            sh.stop.store(true, Ordering::SeqCst);
            sh.kill.store(true, Ordering::SeqCst);
            return;
        }
        if sh.kill.load(Ordering::Relaxed) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return;
        }
        let oldest = {
            let inf = sh.inflight.lock().unwrap();
            inf.values()
                .map(|(op, t)| (t.elapsed().as_millis(), *op))
                .max()
        };
        if let Some((age, op)) = oldest {
            if age > sh.stall_ms && age > worst_seen + 2000 {
                worst_seen = age;
                let n = sh.inflight.lock().unwrap().len();
                sh.dump(&format!(
                    "{op} in flight for {age}ms ({n} requests in flight)"
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn pct(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i] as f64 / 1000.0
}

/// Rules for the `filter` plugin in web mode: hide one entry in ten, as
/// a filter on real feeds does. Any rules in `FILTER_TOML` are added.
const STRESS_FILTER: &str = r#"
[[exclude]]
fields = ["title"]
pattern = '^Item \d*7 of'
"#;

/// Rules for the `auto-tag` plugin in web mode: a few topics, each on a
/// share of the entries. Any rules in `AUTOTAG_TOML` are added.
const STRESS_AUTOTAG: &str = r#"
[[rules]]
tag = "topic-a"
fields = ["title"]
pattern = '^Item \d*[0-2] of'

[[rules]]
tag = "topic-b"
fields = ["title"]
pattern = '^Item \d*[3-5] of'

[[rules]]
tag = "topic-c"
fields = ["content"]
pattern = 'Feed \d*[0-4] says'
"#;

/// Set the `filter` and `auto-tag` plugins' rules with `kiki plugin config
/// set --replace`, the way a deployment of the web UI does before starting
/// it, so that the web UI's lists leave out hidden entries and its tag
/// pages have entries to show.
fn configure_plugins(cfg: &Config) -> Result<()> {
    for (plugin, rules, extra) in [
        ("filter", STRESS_FILTER, &cfg.filter_toml),
        ("auto-tag", STRESS_AUTOTAG, &cfg.autotag_toml),
    ] {
        let mut toml = String::new();
        if let Some(path) = extra {
            toml = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
            toml.push('\n');
        }
        toml.push_str(rules);
        let path = cfg.out.join(format!("{plugin}.toml"));
        std::fs::write(&path, toml)?;
        let status = std::process::Command::new(&cfg.kiki)
            .args(["plugin", "config", "set", "--replace", plugin])
            .arg(&path)
            .env("KIKI_HOME", &cfg.home)
            .status()
            .with_context(|| format!("configuring the {plugin} plugin"))?;
        if !status.success() {
            bail!("`kiki plugin config set {plugin}` failed: {status}");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env()?;
    std::fs::create_dir_all(&cfg.out)?;
    let feed_base = start_feed_server(cfg.slow_every).await?;
    let sock = cfg.home.join("kiki.sock");
    let _ = std::fs::remove_file(&sock);

    if cfg.web {
        configure_plugins(&cfg)?;
    }
    // The web UI's port: free now, and very likely still free once
    // `kiki web` binds it.
    let web_addr = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;

    let log = std::fs::File::create(cfg.out.join("kiki.log"))?;
    let mut cmd = tokio::process::Command::new(&cfg.kiki);
    if cfg.web {
        cmd.arg("web").arg("--listen").arg(web_addr.to_string());
    } else {
        cmd.arg("serve");
    }
    cmd.arg("--uds").arg(&sock).args(&cfg.extra_args);
    for k in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(k);
    }
    cmd.env("KIKI_HOME", &cfg.home)
        .env(
            "RUST_LOG",
            std::env::var("KIKI_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    let child = cmd.spawn().context("spawn kiki")?;
    let pid = child.id().context("kiki pid")?;

    let t0 = Instant::now();
    while std::os::unix::net::UnixStream::connect(&sock).is_err() {
        if t0.elapsed() > Duration::from_secs(15) {
            bail!("kiki did not start listening; see kiki.log");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Kiki's clippy.toml routes reqwest clients through its proxy-aware
    // builders; this one only ever talks to Kiki over its Unix socket.
    #[allow(clippy::disallowed_methods)]
    let client = Client::builder()
        .unix_socket(sock.clone())
        .timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(cfg.workers * 2)
        .build()?;
    // ...and this one only to the web UI on localhost.
    #[allow(clippy::disallowed_methods)]
    let web_client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(cfg.workers * 2)
        .build()?;

    let server_pid = if cfg.web {
        while std::net::TcpStream::connect(web_addr).is_err() {
            if t0.elapsed() > Duration::from_secs(15) {
                bail!("kiki web did not start listening; see kiki.log");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))?;
        children
            .split_whitespace()
            .next()
            .and_then(|p| p.parse().ok())
    } else {
        None
    };
    // For profilers attached from outside, e.g. `perf record -p`.
    std::fs::write(
        cfg.out.join("pids.txt"),
        match server_pid {
            Some(s) => format!("web={pid}\nserve={s}\n"),
            None => format!("serve={pid}\n"),
        },
    )?;

    let sh = Arc::new(Shared {
        start: Instant::now(),
        stop: AtomicBool::new(false),
        kill: AtomicBool::new(false),
        next_id: AtomicU64::new(0),
        stats: Default::default(),
        inflight: Default::default(),
        timeline: Default::default(),
        events: Default::default(),
        feeds: Default::default(),
        entries: Default::default(),
        tags: Default::default(),
        read_tag: AtomicU64::new(0),
        dumps: AtomicU64::new(0),
        pid,
        server_pid,
        web_base: format!("http://{web_addr}"),
        web_tags: Default::default(),
        etags: Default::default(),
        out: cfg.out.clone(),
        feed_base: feed_base.clone(),
        stall_ms: cfg.stall_ms,
    });
    let wd = tokio::spawn(watchdog(sh.clone(), child));

    // --- Setup: aggressive fetch settings, tags, feeds -------------------
    let cur = call(
        &sh,
        &client,
        "setup",
        Method::GET,
        "/v1/settings/feed-fetch",
        None,
    )
    .await
    .context("read feed-fetch settings")?;
    let mut s = cur.clone();
    s["min_polling_cadence_seconds"] = json!(1);
    s["default_fetch_interval_seconds"] = json!(10);
    s["force_refresh_after_seconds"] = json!(30);
    s["timeout_seconds"] = json!(5);
    if call(
        &sh,
        &client,
        "setup",
        Method::PUT,
        "/v1/settings/feed-fetch",
        Some(s.clone()),
    )
    .await
    .is_none()
    {
        sh.event(format!("could not apply feed-fetch settings {s}"));
    }
    for i in 0..10 {
        if let Some(v) = call(
            &sh,
            &client,
            "setup",
            Method::POST,
            "/v1/tags/create",
            Some(json!({ "name": format!("stress-tag-{i}") })),
        )
        .await
        {
            if let Some(id) = v["id"].as_i64() {
                sh.tags
                    .lock()
                    .unwrap()
                    .push((id, format!("stress-tag-{i}")));
            }
        }
    }
    if let Some(v) = call(&sh, &client, "setup", Method::GET, "/v1/tags", None).await {
        let arr = v["tags"]
            .as_array()
            .cloned()
            .or_else(|| v.as_array().cloned())
            .unwrap_or_default();
        for t in arr {
            if t["name"].as_str().is_some_and(|n| n.ends_with("read")) {
                sh.read_tag
                    .store(t["id"].as_i64().unwrap_or(0) as u64, Ordering::Relaxed);
            }
        }
    }
    sh.event(format!(
        "read tag id = {}",
        sh.read_tag.load(Ordering::Relaxed)
    ));

    let setup_t = Instant::now();
    let mut js = tokio::task::JoinSet::new();
    for chunk in (0..cfg.feeds)
        .collect::<Vec<_>>()
        .chunks(cfg.feeds.div_ceil(16).max(1))
    {
        let (sh, c, chunk, fb) = (
            sh.clone(),
            client.clone(),
            chunk.to_vec(),
            feed_base.clone(),
        );
        js.spawn(async move {
            for n in chunk {
                let body = json!({ "title": format!("Stress {n}"), "url": format!("http://{fb}/feed/{n}.xml") });
                if let Some(id) = call(&sh, &c, "feed_create", Method::POST, "/v1/feeds/create", Some(body))
                    .await
                    .and_then(|v| v["id"].as_i64())
                {
                    sh.feeds.lock().unwrap().push(id);
                }
            }
        });
    }
    while js.join_next().await.is_some() {}
    sh.event(format!(
        "created {} feeds in {:.1}s; refreshing all",
        sh.feeds.lock().unwrap().len(),
        setup_t.elapsed().as_secs_f64()
    ));
    call(
        &sh,
        &client,
        "refresh_all",
        Method::POST,
        "/v1/feeds/refresh",
        None,
    )
    .await;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if sh.entries.lock().unwrap().len() >= 200 {
            break;
        }
        if let Some(v) = call(
            &sh,
            &client,
            "sample_entries",
            Method::GET,
            "/v1/entries?limit=500",
            None,
        )
        .await
        {
            let ids: Vec<i64> = v["entries"]
                .as_array()
                .map(|a| a.iter().filter_map(|e| e["id"].as_i64()).collect())
                .unwrap_or_default();
            *sh.entries.lock().unwrap() = ids;
        }
    }
    sh.event(format!(
        "warm-up done; {} entries sampled; starting {} workers for {}s",
        sh.entries.lock().unwrap().len(),
        cfg.workers,
        cfg.secs
    ));
    // Setup latencies are not the load test's; start the stats over.
    sh.stats.lock().unwrap().clear();
    let load_start = sh.start.elapsed().as_secs() as usize;

    // --- Load --------------------------------------------------------------
    let mut tasks = Vec::new();
    for w in 0..cfg.workers {
        let seed = w as u64 + 1;
        tasks.push(if cfg.web {
            tokio::spawn(web_worker(sh.clone(), web_client.clone(), seed))
        } else {
            tokio::spawn(worker(sh.clone(), client.clone(), seed))
        });
    }
    if cfg.web {
        tasks.push(tokio::spawn(tag_sampler(sh.clone(), client.clone())));
    }
    tasks.push(tokio::spawn(entry_sampler(sh.clone(), client.clone())));
    tasks.push(tokio::spawn(canaries(sh.clone(), client.clone())));
    let deadline = Instant::now() + Duration::from_secs(cfg.secs);
    while Instant::now() < deadline && !sh.stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let crashed = sh.stop.swap(true, Ordering::SeqCst);
    let drain = Instant::now();
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(70), t).await;
    }
    let load_end = sh.start.elapsed().as_secs() as usize;

    // --- Recovery check and metrics ---------------------------------------
    let mut report = String::new();
    if !crashed {
        sh.stop.store(false, Ordering::SeqCst);
        let t = Instant::now();
        let ok = call(&sh, &client, "post_health", Method::GET, "/v1/health", None)
            .await
            .is_some();
        let _ = writeln!(
            report,
            "Post-load health: ok={ok} in {}ms (drain took {:.1}s)",
            t.elapsed().as_millis(),
            drain.elapsed().as_secs_f64()
        );
        let metrics = client.get(format!("{BASE}/metrics")).send().await;
        if let Ok(m) = metrics {
            let text = m.text().await.unwrap_or_default();
            std::fs::write(cfg.out.join("metrics.txt"), &text)?;
        }
        sh.stop.store(true, Ordering::SeqCst);
    }
    sh.kill.store(true, Ordering::SeqCst);
    let _ = wd.await;

    // --- Report -------------------------------------------------------------
    let elapsed = (load_end - load_start).max(1) as f64;
    let _ = writeln!(
        report,
        "\nPer-op latency (ms) over {elapsed:.0}s with {} workers, {} feeds",
        cfg.workers, cfg.feeds
    );
    let _ = writeln!(
        report,
        "{:<18} {:>7} {:>8} {:>8} {:>8} {:>8} {:>9}  statuses",
        "op", "count", "p50", "p95", "p99", "max", "req/s"
    );
    let mut total = 0usize;
    for (op, st) in sh.stats.lock().unwrap().iter_mut() {
        st.lat_us.sort_unstable();
        total += st.lat_us.len();
        let statuses: Vec<String> = st.status.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        let _ = writeln!(
            report,
            "{:<18} {:>7} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>9.1}  {}",
            op,
            st.lat_us.len(),
            pct(&st.lat_us, 0.5),
            pct(&st.lat_us, 0.95),
            pct(&st.lat_us, 0.99),
            pct(&st.lat_us, 1.0),
            st.lat_us.len() as f64 / elapsed,
            statuses.join(" ")
        );
    }
    let _ = writeln!(
        report,
        "total {total} requests, {:.0} req/s",
        total as f64 / elapsed
    );
    let _ = writeln!(
        report,
        "\nTimeline (per second): done errs 408s max_ms health_max_ms write_canary_max_ms"
    );
    for (i, b) in sh
        .timeline
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .skip(load_start)
    {
        let _ = writeln!(
            report,
            "t={i:4} {:6} {:5} {:5} {:7} {:7} {:7}",
            b.done, b.errors, b.timeouts, b.max_ms, b.health_max_ms, b.write_canary_max_ms
        );
    }
    let _ = writeln!(report, "\nEvents:");
    for e in sh.events.lock().unwrap().iter() {
        let _ = writeln!(report, "{e}");
    }
    std::fs::write(cfg.out.join("report.txt"), &report)?;
    println!("{report}");
    Ok(())
}
