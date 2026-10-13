//! Full content: fetch the whole article for entries whose feed gives only a summary.
//!
//! Kiki's `full-content` plugin, built as WebAssembly by Kiki's `build.rs` (see
//! `plugins/Cargo.toml`) and installed as `plugins/full-content/plugin.wasm`.
//!
//! Many feeds give each entry's first paragraph, or nothing at all, and leave the rest to
//! their website. As each entry is stored (`entry.enrich`), this plugin measures its
//! content's text, leaving out markup and whitespace. An entry with less than `min_length`
//! characters of it is taken for a summary, a *stub*: the plugin fetches the page the
//! entry links to, with the host's `fetch-page`, and looks in it for the article, in the
//! content of the first element that one of `selectors` matches with at least
//! `min_length` characters of text (or, failing that, in the one with the most text, if it
//! has at least twice the stub's, and half of `min_length`: less may be a page's menus, or
//! a notice asking readers to subscribe). The article replaces the entry's content, with
//! its links and images made absolute, lazily loaded images given their real source, and
//! scripts, styles, frames, forms and event handlers removed. Every plugin's
//! `entry.transform` handler runs after every `entry.enrich` handler, so the `sanitize` and
//! `privacy` plugins then clean it like any other content, whatever order plugins load in.
//!
//! Kiki stores an entry again on every refresh of its feed, with the content the plugins
//! give it then. So that an article is fetched once, not on every refresh, the plugin
//! first looks for the entry already stored (`get-entry`): if its content is not a stub, it
//! is the article fetched before, and is kept.
//!
//! A page that cannot be fetched, or holds no article, is not tried again for
//! [`RETRY_AFTER`] seconds; the entry keeps its summary meanwhile. Fetching pages needs
//! network access for plugins, which is off by default (`allow_network` under `[plugins]`
//! in `kiki.toml`): while Kiki refuses the plugin's fetches, it stops asking for
//! [`REFUSED_FOR`] seconds at a time, and leaves entries as they are.
//!
//! Config:
//!
//! * `feeds`: the feeds whose entries to fill in, each given by its id or by the URL it
//!   is fetched from. Empty, every feed.
//! * `exclude`: feeds whose entries never to fill in, given the same way.
//! * `min_length`: the fewest characters of text, not counting whitespace, that an entry's
//!   content needs not to be taken for a stub.
//! * `selectors`: CSS selectors for the element holding the article, tried in order.

use kiki_plugin::html::{self, Edits, StripRules};
use kiki_plugin::{
    host, log, plugin, Entry, EventKind, FeedEvent, FetchSuccess, Level, Page, Plugin,
};
use serde_json::{Map, Value};
use std::collections::{hash_map, HashMap, HashSet};

/// The time, from WASI's wall clock, which Kiki gives plugins. The standard library has
/// no clock on `wasm32-unknown-unknown`, the target Kiki's own plugins are built for.
mod clock {
    wit_bindgen::generate!({
        inline: "
            package wasi:clocks@0.2.0;

            interface wall-clock {
                record datetime {
                    seconds: u64,
                    nanoseconds: u32,
                }
                now: func() -> datetime;
            }

            world full-content-clock {
                import wall-clock;
            }
        ",
        world: "full-content-clock",
    });

    /// The current Unix time, in seconds.
    pub fn now() -> u64 {
        wasi::clocks::wall_clock::now().seconds
    }
}

/// How long, in seconds, a page that could not be fetched, or held no article, is left
/// before it is tried again.
pub const RETRY_AFTER: u64 = 6 * 60 * 60;

/// How long, in seconds, the plugin stops fetching pages after Kiki refuses to fetch one
/// for it, as it does while network access for plugins is turned off.
pub const REFUSED_FOR: u64 = 60;

/// The most pages the plugin remembers it could not fetch. Past this, the oldest
/// failures are forgotten, and tried again sooner.
const MAX_FAILURES: usize = 4096;

/// Elements removed from an article, with their content: what runs, or asks for input,
/// rather than reads.
const DROP: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "frame", "frameset", "object", "embed",
    "applet", "form", "button", "input", "select", "textarea", "link", "meta", "base",
];

/// Elements whose text is not read, when an entry's text is measured.
const UNREAD: &[&str] = &["script", "style", "noscript", "template"];

/// A list of feeds from the config.
#[derive(Default)]
struct FeedSet {
    ids: HashSet<i64>,
    urls: HashSet<String>,
}

impl FeedSet {
    /// Reads config key `key`, a list of feed ids and URLs.
    fn from_config(config: &Map<String, Value>, key: &str) -> Result<Self, String> {
        let list = match config.get(key) {
            None | Some(Value::Null) => return Ok(FeedSet::default()),
            Some(Value::Array(list)) => list,
            Some(_) => {
                return Err(format!(
                    "full-content: '{key}' must be a list of feed ids or URLs"
                ))
            }
        };
        let mut set = FeedSet::default();
        for (i, feed) in list.iter().enumerate() {
            match (feed.as_str(), feed.as_i64()) {
                (Some(url), _) if !url.is_empty() => {
                    set.urls.insert(url.to_string());
                }
                (None, Some(id)) => {
                    set.ids.insert(id);
                }
                _ => {
                    return Err(format!(
                        "full-content: {key}[{}] must be a feed id or URL",
                        i + 1
                    ))
                }
            }
        }
        Ok(set)
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.urls.is_empty()
    }

    /// Whether feed `feed_id` is in the set, by id or by URL.
    fn contains(&self, feed_id: i64, urls: &mut FeedUrls) -> bool {
        self.ids.contains(&feed_id)
            || (!self.urls.is_empty() && urls.get(feed_id).is_some_and(|u| self.urls.contains(u)))
    }
}

/// The URL of each feed looked up so far, by feed id; `None` for a feed with no URL, or
/// that does not exist.
#[derive(Default)]
struct FeedUrls(HashMap<i64, Option<String>>);

impl FeedUrls {
    /// The URL of feed `feed_id`, looked up once per feed. A failed lookup is logged and
    /// not remembered, so that the next entry tries again.
    fn get(&mut self, feed_id: i64) -> Option<&str> {
        let url = match self.0.entry(feed_id) {
            hash_map::Entry::Occupied(url) => url.into_mut(),
            hash_map::Entry::Vacant(slot) => match host::get_feed(feed_id) {
                Ok(feed) => slot.insert(feed.and_then(|feed| feed.url)),
                Err(e) => {
                    log(
                        Level::Warn,
                        &format!("full-content: looking up feed {feed_id}: {e}"),
                    );
                    return None;
                }
            },
        };
        url.as_deref()
    }

    fn forget(&mut self, feed_id: i64) {
        self.0.remove(&feed_id);
    }
}

struct FullContent {
    only: FeedSet,
    exclude: FeedSet,
    feed_urls: FeedUrls,
    min_length: usize,
    selectors: Vec<String>,
    /// The pages that could not be fetched, or held no article, by URL, with when to try
    /// them again.
    failed: HashMap<String, u64>,
    /// Until when not to ask for pages, since Kiki refused the last one asked for.
    refused_until: u64,
}

impl Plugin for FullContent {
    fn new(config: &str) -> Result<Self, String> {
        let config: Map<String, Value> = serde_json::from_str(config)
            .map_err(|e| format!("full-content: invalid config: {e}"))?;
        let min_length = match config.get("min_length") {
            None | Some(Value::Null) => 500,
            Some(value) => value
                .as_u64()
                .filter(|n| *n > 0)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("full-content: 'min_length' must be a positive whole number")?,
        };
        let selectors = match config.get("selectors") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(list)) => list
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    s.as_str()
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string)
                        .ok_or(format!(
                            "full-content: selectors[{}] must be a CSS selector",
                            i + 1
                        ))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err("full-content: 'selectors' must be a list of CSS selectors".into())
            }
        };
        // An invalid selector fails now, not on every page.
        for selector in &selectors {
            html::inner("", selector).map_err(|e| format!("full-content: {e}"))?;
        }
        Ok(FullContent {
            only: FeedSet::from_config(&config, "feeds")?,
            exclude: FeedSet::from_config(&config, "exclude")?,
            feed_urls: FeedUrls::default(),
            min_length,
            selectors,
            failed: HashMap::new(),
            refused_until: 0,
        })
    }

    // A feed's URL changes when it is permanently redirected. Only watched for when feeds
    // are named by URL, since every handler costs each fetch a little.
    fn wants(&self, event: EventKind) -> bool {
        event != EventKind::FetchSuccess
            || !self.only.urls.is_empty()
            || !self.exclude.urls.is_empty()
    }
}

impl FullContent {
    /// Whether the plugin fills in the entries of feed `feed_id`.
    fn applies(&mut self, feed_id: i64) -> bool {
        if self.exclude.contains(feed_id, &mut self.feed_urls) {
            return false;
        }
        self.only.is_empty() || self.only.contains(feed_id, &mut self.feed_urls)
    }

    /// The stored content of `entry`, if it is stored and its content is not a stub.
    fn stored_article(&self, entry: &Entry) -> Option<String> {
        match host::get_entry(entry.feed_id, &entry.guid) {
            Ok(stored) => stored
                .and_then(|stored| stored.content)
                .filter(|content| text_length(content) >= self.min_length),
            Err(e) => {
                log(
                    Level::Warn,
                    &format!("full-content: looking up entry {:?}: {e}", entry.guid),
                );
                None
            }
        }
    }

    /// The fewest characters of text an element needs to be taken for the article when no
    /// element has `min_length`, for an entry whose content has `stub` characters of it.
    ///
    /// On a page that builds its article with a script, or keeps it from readers who
    /// haven't paid, what the selectors find may be the page's menus or a notice asking
    /// to subscribe, often a little longer than the summary it would replace. So the
    /// element needs at least twice the stub's text, more than the stub's in any case,
    /// and, for an entry with next to no text of its own, half of `min_length`.
    fn fallback(&self, stub: usize) -> usize {
        stub.saturating_mul(2)
            .max(self.min_length / 2)
            .max(stub.saturating_add(1))
    }

    /// The article on `page`: the content of the first element a selector finds with
    /// `min_length` characters of text, or else of the one with the most text, if it has
    /// at least [`Self::fallback`]`(stub)`. `None` if there is neither.
    fn article(&self, page: &Page, stub: usize) -> Option<String> {
        let fallback = self.fallback(stub);
        let mut best: Option<(usize, String)> = None;
        for selector in &self.selectors {
            let content = match html::inner(&page.body, selector) {
                Ok(Some(content)) => content,
                Ok(None) => continue,
                Err(e) => {
                    log(Level::Debug, &format!("full-content: {}: {e}", page.url));
                    continue;
                }
            };
            let length = text_length(&content);
            if length >= self.min_length {
                return Some(content);
            }
            if length >= fallback && best.as_ref().is_none_or(|(most, _)| length > *most) {
                best = Some((length, content));
            }
        }
        best.map(|(_, content)| content)
    }

    /// Note that the page at `url` could not be had, so as not to try it again soon.
    fn failed(&mut self, url: &str, now: u64) {
        if self.failed.len() >= MAX_FAILURES {
            self.failed.retain(|_, until| *until > now);
            if self.failed.len() >= MAX_FAILURES {
                self.failed.clear();
            }
        }
        self.failed.insert(url.to_string(), now + RETRY_AFTER);
    }

    /// The article `entry` links to, fetched, or `None`.
    fn fetch_article(&mut self, url: &str, stub: usize) -> Option<String> {
        let now = clock::now();
        if now < self.refused_until || self.failed.get(url).is_some_and(|until| now < *until) {
            return None;
        }
        let page = match host::fetch_page(url) {
            Ok(page) => page,
            Err(e) if is_refusal(&e) => {
                if self.refused_until == 0 {
                    log(
                        Level::Info,
                        &format!("full-content: not fetching articles: {e}"),
                    );
                }
                self.refused_until = now + REFUSED_FOR;
                return None;
            }
            Err(e) => {
                log(Level::Info, &format!("full-content: {url}: {e}"));
                self.failed(url, now);
                return None;
            }
        };
        self.refused_until = 0;
        let Some(article) = self.article(&page, stub) else {
            log(
                Level::Info,
                &format!("full-content: found no article in {}", page.url),
            );
            self.failed(url, now);
            return None;
        };
        match clean(&article, &page.url) {
            Ok(article) => Some(article),
            Err(e) => {
                log(Level::Info, &format!("full-content: {}: {e}", page.url));
                self.failed(url, now);
                None
            }
        }
    }
}

#[plugin]
impl FullContent {
    #[on(entry.enrich)]
    fn fill_in(&mut self, mut entry: Entry) -> Option<Entry> {
        let stub = entry.content.as_deref().map_or(0, text_length);
        if stub >= self.min_length || !self.applies(entry.feed_id) {
            return Some(entry);
        }
        let Some(url) = entry.url.clone().filter(|url| is_http(url)) else {
            return Some(entry);
        };
        if let Some(article) = self.stored_article(&entry) {
            entry.content = Some(article);
        } else if let Some(article) = self.fetch_article(&url, stub) {
            entry.content = Some(article);
        }
        Some(entry)
    }

    // A removed feed's id may be given to a new feed.
    #[on(feed.removed)]
    fn forget_feed(&mut self, feed: FeedEvent) {
        self.feed_urls.forget(feed.id);
    }

    #[on(fetch.success)]
    fn forget_url(&mut self, fetch: FetchSuccess) {
        self.feed_urls.forget(fetch.feed_id);
    }
}

/// Whether `e`, an error from `fetch-page`, says Kiki will not fetch pages for the plugin
/// at all, rather than that this page could not be fetched.
fn is_refusal(e: &str) -> bool {
    e.contains("turned off") || e.contains("permission") || e.contains("not available")
}

fn is_http(url: &str) -> bool {
    let lower = url
        .trim_start()
        .get(..8)
        .unwrap_or(url)
        .to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// How many characters of text `content`, HTML, has, not counting whitespace, or what is
/// in scripts and styles. Character references count as what they stand for.
fn text_length(content: &str) -> usize {
    let rules = StripRules {
        drop: UNREAD.iter().map(|s| s.to_string()).collect(),
        keep: Vec::new(),
        drop_foreign: false,
        remove_comments: true,
    };
    let text = match html::strip(content, &rules) {
        Ok(stripped) => html::unescape(
            &stripped
                .pieces()
                .map(|p| match p {
                    html::Piece::Html(text) => text,
                    html::Piece::StartTag { .. } => "",
                })
                .collect::<String>(),
        )
        .unwrap_or_default(),
        // Too long to strip is not a stub.
        Err(_) => return usize::MAX,
    };
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// `article`, HTML from the page at `page_url`, made fit for an entry: the elements in
/// [`DROP`] and event handler attributes removed, lazily loaded images given their real
/// source, and the URLs in links and images made absolute.
fn clean(article: &str, page_url: &str) -> Result<String, String> {
    let elements = html::select(article, "*")?;
    let mut edits = Edits::new();
    for el in elements.iter() {
        let index = el.index();
        if DROP.contains(&el.tag_name()) {
            edits.remove(index);
            continue;
        }
        let mut src = None;
        let mut lazy = None;
        for (name, value) in el.attributes() {
            if name.starts_with("on") {
                edits.remove_attribute(index, name);
            } else if matches!(name, "href" | "src" | "poster" | "cite") {
                if name == "src" {
                    src = Some(value);
                }
                match absolute(value, page_url) {
                    Some(url) if url != value => {
                        edits.set_attribute(index, name, &url);
                    }
                    Some(_) => {}
                    None => {
                        edits.remove_attribute(index, name);
                    }
                }
            } else if matches!(name, "data-src" | "data-lazy-src" | "data-original") {
                lazy = lazy.or(Some(value));
            }
        }
        // A lazily loaded image's `src` is empty or a placeholder until a script swaps in
        // the real one, which won't run in a feed reader.
        if el.tag_name() == "img" {
            let placeholder = src.is_none_or(|s| s.trim().is_empty() || s.starts_with("data:"));
            if let (true, Some(real)) = (placeholder, lazy.and_then(|l| absolute(l, page_url))) {
                edits.set_attribute(index, "src", &real);
            }
        }
    }
    html::rewrite(article, "*", &edits).map(|html| html.trim().to_string())
}

/// `url`, a link on the page at `base`, as an absolute URL; or `None` if it runs script.
/// Links within the page, such as `#notes`, are kept as they are.
fn absolute(url: &str, base: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.starts_with('#') {
        return Some(url.to_string());
    }
    if let Some(scheme) = scheme(url) {
        let scheme = scheme.to_ascii_lowercase();
        return (scheme != "javascript" && scheme != "vbscript").then(|| url.to_string());
    }
    let (base_scheme, rest) = base.split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let origin = format!("{base_scheme}://{}", rest.get(..authority_end)?);
    if let Some(rest) = url.strip_prefix("//") {
        return Some(format!("{base_scheme}://{rest}"));
    }
    if url.starts_with('/') {
        return Some(format!("{origin}{url}"));
    }
    let path = rest.get(authority_end..)?;
    let path = path.split(['?', '#']).next().unwrap_or_default();
    if url.starts_with('?') {
        return Some(format!("{origin}{path}{url}"));
    }
    let dir = path
        .rfind('/')
        .map_or("/", |i| path.get(..=i).unwrap_or("/"));
    Some(format!("{origin}{dir}{url}"))
}

/// The scheme of `url`, if it has one: letters, digits, `+`, `-` and `.`, starting with
/// a letter, before a `:`.
fn scheme(url: &str) -> Option<&str> {
    let (scheme, _) = url.split_once(':')?;
    let mut chars = scheme.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    (starts_with_letter && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .then_some(scheme)
}
