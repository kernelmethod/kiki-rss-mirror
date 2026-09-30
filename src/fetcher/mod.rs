//! Retrieving and parsing feeds, and downloading their assets, with no
//! access to Kiki's state.
//!
//! Everything a feed refresh does with untrusted bytes — the HTTP
//! exchange, TLS, decompression, reading a capped body, and parsing the
//! result as Atom or RSS — lives here, as functions of their inputs
//! alone. So does everything asset caching does with them — downloading
//! images, enclosures and favicons, and parsing HTML to find them; see
//! [`assets`]. Nothing in this module touches the database, the asset cache,
//! metrics, or scripts: [`crate::tasks`] reads what a fetch needs out of
//! the database into a [`FetchSpec`], and writes what comes back in a
//! [`FetchReply`] into it.
//!
//! That split is what lets the work run in the sandboxed feed fetcher
//! process ([`crate::process::feed_fetcher`]), which holds no database
//! handle and no writable filesystem. [`Fetcher`] hides which of the two
//! places the work actually happens in, so callers do not change.
//!
//! Everything that crosses the process boundary is plain data, and all of
//! it is `Serialize` + `Deserialize` so it can be sent over the IPC channel.

pub mod assets;
pub mod parse;
pub mod retrieve;

use crate::config::ProxySettings;
use crate::http::{FeedAuth, USER_AGENT};
use crate::scripting::FeedEntry;
use assets::{AssetReply, AssetSpec, PageIcons, PageSpec};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use parse::parse_feed;
pub use retrieve::retrieve;

/// How many redirects a single fetch follows before giving up.
pub const MAX_REDIRECTS: u64 = 10;

/// The response headers the server's caching and scheduling logic reads.
///
/// Only these are sent back across the process boundary; nothing else in
/// a feed server's response influences what Kiki stores.
pub const FORWARDED_HEADERS: &[&str] = &[
    "age",
    "cache-control",
    "date",
    "etag",
    "expires",
    "last-modified",
    "pragma",
    "retry-after",
];

/// Everything needed to fetch one feed, read out of the database by the
/// caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchSpec {
    /// The feed being fetched. Used for logging and to stamp parsed
    /// entries; the server re-stamps them rather than trusting it.
    pub feed_id: i64,

    /// The feed's stored URL.
    pub url: String,

    /// Stored `ETag`, sent as `If-None-Match` when conditionals are on.
    pub etag: Option<String>,

    /// Stored `Last-Modified`, sent as `If-Modified-Since` when
    /// conditionals are on.
    pub last_modified: Option<String>,

    /// Whether conditional headers may be sent on the first request.
    /// Off inside an `immutable` window and on a forced full refresh.
    pub send_conditionals: bool,

    /// Credentials for the feed's own origin. Dropped on a cross-origin
    /// redirect.
    pub auth: FeedAuth,

    /// Per-request timeout, covering connect through the end of the body.
    pub timeout_secs: u64,

    /// Largest body that will be read before the fetch is abandoned.
    pub max_feed_bytes: u64,

    /// The proxy to fetch through, with environment overrides already
    /// applied by the server ([`crate::config::Settings::effective_proxy`]).
    #[serde(with = "proxy_wire")]
    pub proxy: ProxySettings,
}

/// [`ProxySettings`] as it crosses the process boundary. Its own serde
/// form skips unset keys, which suits TOML but not postcard: a
/// non-self-describing format cannot tell which fields were left out.
mod proxy_wire {
    use crate::config::ProxySettings;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(p: &ProxySettings, s: S) -> Result<S::Ok, S::Error> {
        (&p.url, &p.no_proxy).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ProxySettings, D::Error> {
        let (url, no_proxy) = Deserialize::deserialize(d)?;
        Ok(ProxySettings { url, no_proxy })
    }
}

/// Response headers from a feed server, restricted to
/// [`FORWARDED_HEADERS`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseHeaders(pub Vec<(String, String)>);

impl ResponseHeaders {
    /// Capture the forwarded subset of `headers`. Values that are not
    /// visible ASCII are dropped, matching how the server's header
    /// parsing already treated them.
    pub fn capture(headers: &reqwest::header::HeaderMap) -> Self {
        let mut out = Vec::new();
        for name in FORWARDED_HEADERS {
            for value in headers.get_all(*name) {
                if let Ok(v) = value.to_str() {
                    out.push(((*name).to_string(), v.to_string()));
                }
            }
        }
        ResponseHeaders(out)
    }

    /// Rebuild a [`reqwest::header::HeaderMap`] for the server's cache
    /// helpers. Names outside [`FORWARDED_HEADERS`], and anything that is
    /// not a valid header, are ignored.
    pub fn to_header_map(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderName, HeaderValue};
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in &self.0 {
            if !FORWARDED_HEADERS.contains(&name.as_str()) {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                map.append(n, v);
            }
        }
        map
    }
}

/// How a fetch ended.
///
/// Each variant corresponds to one of the outcomes the server records
/// against the feed; the server alone decides what to write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FetchReply {
    /// No response was received (connection failure, TLS error, timeout).
    Network {
        message: String,
        timeout: bool,
        redirects: u64,
    },

    /// More than [`MAX_REDIRECTS`] redirects.
    TooManyRedirects { redirects: u64 },

    /// `304 Not Modified`.
    NotModified {
        headers: ResponseHeaders,
        redirects: u64,
    },

    /// Any status other than 200 or 304.
    HttpStatus {
        status: u16,
        headers: ResponseHeaders,
        redirects: u64,
    },

    /// The body exceeded [`FetchSpec::max_feed_bytes`].
    BodyTooLarge {
        final_url: String,
        seen: u64,
        redirects: u64,
    },

    /// A `200 OK` whose body was read in full, and the result of parsing
    /// it.
    Body(Box<FetchedBody>),

    /// The exchange failed in a way that is not a feed-server error — a
    /// redirect with no usable `Location`, or a body read that failed
    /// part-way.
    Failed { message: String },
}

/// A successfully read `200 OK`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchedBody {
    /// The URL the body was finally served from, after redirects.
    pub final_url: String,

    /// Whether any hop was a 301 or 308.
    pub permanent_redirect: bool,

    pub redirects: u64,

    pub headers: ResponseHeaders,

    /// Size of the body in bytes.
    pub body_len: u64,

    /// BLAKE3 hex digest of the body, for validator-lie detection.
    pub body_hash: String,

    pub parsed: ParseOutcome,
}

/// The result of parsing a body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseOutcome {
    /// `None` when the body was neither Atom nor RSS.
    pub feed: Option<ParsedFeed>,

    /// Wall-clock time spent parsing, for the parse-duration metric.
    pub seconds: f64,
}

/// A parsed feed, reduced to exactly what Kiki stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParsedFeed {
    Atom {
        feed: Box<AtomFeedIngestData>,
        entries: Vec<AtomEntry>,
        hints: FeedHints,
        /// The feed's `rel="alternate"` link: the website it belongs to.
        site_url: Option<String>,
    },
    Rss {
        entries: Vec<RssEntry>,
        hints: FeedHints,
        /// The channel's `<link>`: the website it belongs to.
        site_url: Option<String>,
    },
}

/// Refresh hints a publisher declares in the feed document itself, as
/// opposed to in HTTP response headers.
///
/// Covers RSS 2.0 `<ttl>`, `<skipHours>` and `<skipDays>`, and the RSS 1.0
/// Syndication module (`sy:updatePeriod` / `sy:updateFrequency`), which
/// also turns up in RSS 2.0 and Atom feeds.
///
/// # Examples
///
/// ```
/// use kiki_rss::fetcher::{parse_feed, FeedHints};
///
/// let rss = br#"<rss version="2.0"><channel><title>t</title><link>http://x/</link>
///     <description>d</description><ttl>90</ttl>
///     <skipDays><day>Sunday</day></skipDays></channel></rss>"#;
/// let parsed = parse_feed(1, rss).expect("valid RSS");
/// let hints = parsed.hints();
/// assert_eq!(hints.ttl_secs, Some(90 * 60));
/// assert_eq!(hints.refresh_hint_secs(), Some(90 * 60));
/// assert_eq!(hints.skip_days, 1 << 6);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedHints {
    /// RSS `<ttl>`, converted from minutes to seconds. `None` when absent,
    /// unparseable, or zero.
    pub ttl_secs: Option<u64>,

    /// Seconds between updates according to `sy:updatePeriod` divided by
    /// `sy:updateFrequency`. `None` when the feed carries no Syndication
    /// module elements.
    pub update_interval_secs: Option<u64>,

    /// RSS `<skipHours>` as a bitmask: bit `h` set means the feed asks not
    /// to be read during hour `h` (0–23, UTC).
    pub skip_hours: u32,

    /// RSS `<skipDays>` as a bitmask: bit 0 is Monday through bit 6,
    /// Sunday (days are interpreted in UTC).
    pub skip_days: u8,
}

impl FeedHints {
    /// The feed's own statement of how long it can go between refreshes,
    /// in seconds.
    ///
    /// When both `<ttl>` and the Syndication module are present the longer
    /// of the two is used: each is a publisher saying nothing new is
    /// expected sooner, so the politer reading wins. Returns `None` when
    /// the feed declares neither.
    pub fn refresh_hint_secs(&self) -> Option<u64> {
        match (self.ttl_secs, self.update_interval_secs) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

impl ParsedFeed {
    /// The syndication format, as used in metric labels.
    pub fn format(&self) -> &'static str {
        match self {
            ParsedFeed::Atom { .. } => "atom",
            ParsedFeed::Rss { .. } => "rss",
        }
    }

    /// Number of entries in the feed.
    pub fn entry_count(&self) -> usize {
        match self {
            ParsedFeed::Atom { entries, .. } => entries.len(),
            ParsedFeed::Rss { entries, .. } => entries.len(),
        }
    }

    /// Refresh hints declared in the feed document.
    pub fn hints(&self) -> &FeedHints {
        match self {
            ParsedFeed::Atom { hints, .. } | ParsedFeed::Rss { hints, .. } => hints,
        }
    }

    /// The URL of the website the feed belongs to, exactly as the feed
    /// document gives it: it may be relative, and is not checked to be
    /// `http(s)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::fetcher::parse_feed;
    ///
    /// let rss = br#"<rss version="2.0"><channel><title>t</title>
    ///     <link>https://example.com/</link><description>d</description>
    ///     </channel></rss>"#;
    /// let parsed = parse_feed(1, rss).expect("valid RSS");
    /// assert_eq!(parsed.site_url(), Some("https://example.com/"));
    /// ```
    pub fn site_url(&self) -> Option<&str> {
        match self {
            ParsedFeed::Atom { site_url, .. } | ParsedFeed::Rss { site_url, .. } => {
                site_url.as_deref()
            }
        }
    }
}

/// One Atom entry: the common fields, plus the Atom-only ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomEntry {
    pub entry: FeedEntry,
    pub data: AtomEntryIngestData,
}

/// One RSS item: the common fields, plus the RSS-only ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssEntry {
    pub entry: FeedEntry,
    pub data: RssEntryIngestData,
}

/// Atom-specific feed-level data captured from a parsed feed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AtomFeedIngestData {
    pub atom_uri: Option<String>,
    pub atom_language_tag: Option<String>,
    pub rights: Option<String>,
    pub generator: Option<AtomGenerator>,
    pub logo: Option<String>,
    pub icon: Option<String>,
    pub authors: Vec<String>,
    pub contributors: Vec<String>,
    pub categories: Vec<AtomCategory>,
}

/// Atom-specific per-entry data captured from a parsed entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AtomEntryIngestData {
    pub rights: Option<String>,
    pub authors: Vec<String>,
    pub contributors: Vec<String>,
    pub categories: Vec<AtomCategory>,
}

/// An Atom `<generator>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AtomGenerator {
    pub value: String,
    pub uri: Option<String>,
    pub version: Option<String>,
}

/// An Atom `<category>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AtomCategory {
    pub term: String,
    pub scheme: Option<String>,
    pub label: Option<String>,
}

/// RSS-specific per-entry data captured from a parsed item.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RssEntryIngestData {
    pub description: Option<String>,
    pub comments: Option<String>,
    pub author: Option<String>,
    pub enclosure_url: Option<String>,
    pub enclosure_length: Option<i64>,
    pub enclosure_mime_type: Option<String>,
    pub categories: Vec<RssCategory>,
}

/// An RSS `<category>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RssCategory {
    pub name: String,
    pub domain: Option<String>,
}

/// Why a [`Fetcher`] could not produce a reply at all.
///
/// Distinct from a [`FetchReply`] describing a failed fetch: this means
/// the machinery doing the fetching failed, not the feed server.
#[derive(Debug, thiserror::Error)]
pub enum FetcherError {
    /// The isolated fetcher process is gone, or its worker crashed while
    /// serving this request.
    #[error("feed fetcher unavailable: {0}")]
    Unavailable(String),

    /// The isolated fetcher did not answer within its deadline.
    #[error("feed fetcher did not answer within {0:?}")]
    Timeout(Duration),
}

/// Where feed retrieval and parsing, and asset downloads, happen.
///
/// Callers get the same replies either way; the variants differ only in
/// which process holds the network clients and the parsers.
#[derive(Clone)]
pub enum Fetcher {
    /// In this process, with the given clients. Used by the library-level
    /// tests and on platforms without an isolated fetcher.
    InProcess {
        /// For feeds; see [`client_builder`].
        feeds: ProxiedClient,
        /// For assets and favicons; see [`assets::asset_client_builder`].
        assets: ProxiedClient,
    },

    /// In the sandboxed feed fetcher process.
    #[cfg(unix)]
    Isolated(std::sync::Arc<crate::process::feed_fetcher::FeedFetcherHost>),
}

impl Fetcher {
    /// An in-process fetcher with the client configuration production
    /// uses: no automatic redirects (they are followed by [`retrieve()`], so
    /// credentials can be dropped cross-origin), Kiki's user agent, and
    /// [`assets::AssetTimeouts::DEFAULT`] for assets.
    ///
    /// # Errors
    ///
    /// Fails if the TLS backend cannot be initialised.
    pub fn in_process() -> reqwest::Result<Self> {
        Ok(Fetcher::InProcess {
            feeds: ProxiedClient::new(client_builder)?,
            assets: ProxiedClient::new(|| {
                assets::asset_client_builder(assets::AssetTimeouts::DEFAULT)
            })?,
        })
    }

    /// Fetch, and on a `200 OK` parse, the feed described by `spec`.
    ///
    /// # Errors
    ///
    /// Only the isolated variant fails, and only when the fetcher process
    /// itself could not serve the request; see [`FetcherError`].
    pub async fn fetch(&self, spec: FetchSpec) -> Result<FetchReply, FetcherError> {
        match self {
            Fetcher::InProcess { feeds, .. } => Ok(fetch_with(feeds, &spec).await),
            #[cfg(unix)]
            Fetcher::Isolated(host) => host.fetch(spec).await,
        }
    }

    /// Parse a body the caller already holds — a `file://` feed, which
    /// the server reads itself because the fetcher has no filesystem.
    ///
    /// # Errors
    ///
    /// As for [`Self::fetch`].
    pub async fn parse(&self, feed_id: i64, body: Vec<u8>) -> Result<ParseOutcome, FetcherError> {
        match self {
            Fetcher::InProcess { .. } => Ok(parse_off_thread(feed_id, body).await),
            #[cfg(unix)]
            Fetcher::Isolated(host) => host.parse(feed_id, body).await,
        }
    }

    /// Download the asset described by `spec`.
    ///
    /// # Errors
    ///
    /// As for [`Self::fetch`]; failures of the download itself are
    /// [`AssetReply`] variants.
    pub async fn fetch_asset(&self, spec: AssetSpec) -> Result<AssetReply, FetcherError> {
        match self {
            Fetcher::InProcess {
                assets: clients, ..
            } => Ok(assets::fetch_asset(clients, &spec).await),
            #[cfg(unix)]
            Fetcher::Isolated(host) => host.fetch_asset(spec).await,
        }
    }

    /// Fetch the web page in `spec` and find the icons it links to.
    ///
    /// # Errors
    ///
    /// As for [`Self::fetch`]; a page that cannot be read yields no icons.
    pub async fn find_page_icons(&self, spec: PageSpec) -> Result<PageIcons, FetcherError> {
        match self {
            Fetcher::InProcess {
                assets: clients, ..
            } => Ok(assets::find_page_icons(clients, &spec).await),
            #[cfg(unix)]
            Fetcher::Isolated(host) => host.find_page_icons(spec).await,
        }
    }

    /// Find the images an entry's HTML `content` shows, resolved against
    /// `base`; see [`assets::extract_asset_urls`].
    ///
    /// # Errors
    ///
    /// As for [`Self::fetch`].
    pub async fn extract_images(
        &self,
        content: String,
        base: &reqwest::Url,
    ) -> Result<Vec<String>, FetcherError> {
        match self {
            Fetcher::InProcess { .. } => Ok(assets::extract_asset_urls(&content, base)
                .into_iter()
                .map(String::from)
                .collect()),
            #[cfg(unix)]
            Fetcher::Isolated(host) => host.extract_images(content, base.to_string()).await,
        }
    }
}

/// The client configuration shared by every feed fetch, for callers that
/// need to adjust it further — the isolated fetcher swaps in its own DNS
/// resolver. Build it with [`ProxiedClient`], so the proxy settings apply.
#[allow(clippy::disallowed_methods, reason = "the sanctioned starting point")]
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
}

/// Builds the [`reqwest::ClientBuilder`] a [`ProxiedClient`] starts from.
type BuilderFn = dyn Fn() -> reqwest::ClientBuilder + Send + Sync;

/// An HTTP client that follows the proxy settings it is asked for.
///
/// A [`reqwest::Client`]'s proxy is fixed when it is built, while Kiki's
/// proxy settings can change at any time. This keeps one client for the
/// settings last asked for, and rebuilds it only when they change, so
/// connection pooling survives across requests that share a proxy.
/// Cloning is cheap and shares the cached client.
#[derive(Clone)]
pub struct ProxiedClient {
    builder: Arc<BuilderFn>,
    current: Arc<Mutex<(ProxySettings, reqwest::Client)>>,
}

impl ProxiedClient {
    /// A client built by `builder`, initially with no explicit proxy.
    ///
    /// # Errors
    ///
    /// Fails if the TLS backend cannot be initialised.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::ProxySettings;
    /// use kiki_rss::fetcher::{client_builder, ProxiedClient};
    ///
    /// let clients = ProxiedClient::new(client_builder).unwrap();
    /// let proxy = ProxySettings {
    ///     url: Some("http://proxy.example:3128".into()),
    ///     no_proxy: None,
    /// };
    /// let _client = clients.get(&proxy).unwrap();
    /// ```
    pub fn new(
        builder: impl Fn() -> reqwest::ClientBuilder + Send + Sync + 'static,
    ) -> reqwest::Result<Self> {
        #[allow(clippy::disallowed_methods, reason = "no proxy is asked for yet")]
        let client = builder().build()?;
        Ok(Self::with_client(client, builder))
    }

    /// Like [`ProxiedClient::new`], but using `client` as-is for as long as
    /// no proxy is asked for.
    pub fn with_client(
        client: reqwest::Client,
        builder: impl Fn() -> reqwest::ClientBuilder + Send + Sync + 'static,
    ) -> Self {
        ProxiedClient {
            builder: Arc::new(builder),
            current: Arc::new(Mutex::new((ProxySettings::default(), client))),
        }
    }

    /// Returns a client that sends requests through `proxy`.
    ///
    /// # Errors
    ///
    /// Fails if `proxy` is not a usable proxy URL, or if the TLS backend
    /// cannot be initialised.
    pub fn get(&self, proxy: &ProxySettings) -> reqwest::Result<reqwest::Client> {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.0 != *proxy {
            #[allow(clippy::disallowed_methods, reason = "the proxy is applied here")]
            let client = apply_proxy((self.builder)(), proxy)?.build()?;
            *current = (proxy.clone(), client);
        }
        Ok(current.1.clone())
    }
}

/// Configures `builder` to use `proxy`. With no proxy URL, the builder is
/// left alone, so reqwest's default of following `HTTPS_PROXY` and friends
/// from the environment applies.
///
/// # Errors
///
/// Fails if the proxy URL cannot be parsed.
pub fn apply_proxy(
    builder: reqwest::ClientBuilder,
    proxy: &ProxySettings,
) -> reqwest::Result<reqwest::ClientBuilder> {
    let Some(url) = &proxy.url else {
        return Ok(builder);
    };
    let no_proxy = proxy
        .no_proxy
        .as_deref()
        .and_then(reqwest::NoProxy::from_string);
    Ok(builder.proxy(reqwest::Proxy::all(url.trim())?.no_proxy(no_proxy)))
}

/// Fetch `spec` with the client for its proxy settings.
pub async fn fetch_with(clients: &ProxiedClient, spec: &FetchSpec) -> FetchReply {
    match clients.get(&spec.proxy) {
        Ok(client) => retrieve(&client, spec).await,
        // Settings are validated before they reach a fetch, so this is
        // not expected; the error is not shown as it may contain the URL.
        Err(_) => FetchReply::Failed {
            message: "could not configure the HTTP client for the proxy".to_string(),
        },
    }
}

/// Run [`parse_feed`] on the blocking pool, timing it.
///
/// Parsing a large feed is CPU-bound, so it stays off the async workers.
/// A panic in the parser is reported as an unparseable body rather than
/// propagated.
pub async fn parse_off_thread(feed_id: i64, body: Vec<u8>) -> ParseOutcome {
    let start = std::time::Instant::now();
    let feed = tokio::task::spawn_blocking(move || parse_feed(feed_id, &body))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("Feed {}: parser failed: {}", feed_id, e);
            None
        });
    ParseOutcome {
        feed,
        seconds: start.elapsed().as_secs_f64(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;

    const RSS: &str = r#"<rss version="2.0"><channel><title>t</title><link>http://x/</link>
        <description>d</description><item><title>hi</title><guid>g1</guid></item>
        </channel></rss>"#;

    /// Start a stand-in HTTP proxy that serves [`RSS`] for any `/feed`
    /// request addressed to `feed.invalid`, a host that cannot resolve, so
    /// a fetch of it succeeds only if it went through the proxy.
    pub(crate) async fn start_proxy() -> std::net::SocketAddr {
        use axum::{http::Uri, routing::get, Router};
        let app = Router::new().route(
            "/feed",
            get(|uri: Uri| async move {
                assert_eq!(uri.host(), Some("feed.invalid"), "{uri}");
                RSS
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        addr
    }

    /// Start a stand-in SOCKS5 proxy that serves [`RSS`] over HTTP on any
    /// connection it is asked to make to `feed.invalid` by name, so a fetch
    /// of it succeeds only if it went through the proxy, and the proxy did
    /// the host name lookup.
    pub(crate) async fn start_socks_proxy() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn serve(mut s: tokio::net::TcpStream) -> std::io::Result<()> {
            // Greeting: version, and the authentication methods offered.
            let mut head = [0u8; 2];
            s.read_exact(&mut head).await?;
            let mut methods = vec![0u8; usize::from(head[1])];
            s.read_exact(&mut methods).await?;
            s.write_all(&[5, 0]).await?;

            // Request: only a CONNECT to a host name (address type 3).
            let mut req = [0u8; 5];
            s.read_exact(&mut req).await?;
            assert_eq!(req[..4], [5, 1, 0, 3], "not a CONNECT by host name");
            let mut name = vec![0u8; usize::from(req[4])];
            s.read_exact(&mut name).await?;
            let mut port = [0u8; 2];
            s.read_exact(&mut port).await?;
            assert_eq!(name, b"feed.invalid");
            s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;

            // The HTTP request, up to its blank line, then the feed.
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0u8; 1];
                s.read_exact(&mut byte).await?;
                request.extend_from_slice(&byte);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{RSS}",
                RSS.len()
            );
            s.write_all(response.as_bytes()).await
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream));
            }
        });
        addr
    }

    pub(crate) fn proxied_spec(proxy: ProxySettings) -> FetchSpec {
        FetchSpec {
            feed_id: 1,
            url: "http://feed.invalid/feed".to_string(),
            etag: None,
            last_modified: None,
            send_conditionals: false,
            auth: FeedAuth::default(),
            timeout_secs: 5,
            max_feed_bytes: 1024 * 1024,
            proxy,
        }
    }

    #[tokio::test]
    async fn fetches_go_through_the_configured_proxy() {
        let addr = start_proxy().await;
        let clients = ProxiedClient::new(client_builder).unwrap();
        let proxy = ProxySettings {
            url: Some(format!("http://{addr}")),
            no_proxy: None,
        };
        let reply = fetch_with(&clients, &proxied_spec(proxy.clone())).await;
        assert!(matches!(reply, FetchReply::Body(_)), "got {reply:?}");

        // Hosts listed in `no_proxy` are fetched directly, and so fail.
        let bypassed = ProxySettings {
            no_proxy: Some("localhost, feed.invalid".into()),
            ..proxy
        };
        let reply = fetch_with(&clients, &proxied_spec(bypassed)).await;
        assert!(matches!(reply, FetchReply::Network { .. }), "got {reply:?}");
    }

    /// A `socks5h` proxy is handed host names to look up itself, so a
    /// host that does not resolve here is still reached; with `socks5`,
    /// the name is looked up locally, and fails.
    #[tokio::test]
    async fn fetches_go_through_a_socks5_proxy() {
        let addr = start_socks_proxy().await;
        let clients = ProxiedClient::new(client_builder).unwrap();
        let proxy = ProxySettings {
            url: Some(format!("socks5h://{addr}")),
            no_proxy: None,
        };
        let reply = fetch_with(&clients, &proxied_spec(proxy)).await;
        assert!(matches!(reply, FetchReply::Body(_)), "got {reply:?}");

        let local_dns = ProxySettings {
            url: Some(format!("socks5://{addr}")),
            no_proxy: None,
        };
        let reply = fetch_with(&clients, &proxied_spec(local_dns)).await;
        assert!(matches!(reply, FetchReply::Network { .. }), "got {reply:?}");
    }

    #[test]
    fn clients_are_rebuilt_only_when_the_proxy_changes() {
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&built);
        let clients = ProxiedClient::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            client_builder()
        })
        .unwrap();
        let count = || built.load(std::sync::atomic::Ordering::SeqCst);
        let proxy = ProxySettings {
            url: Some("http://proxy.example:3128".into()),
            no_proxy: None,
        };

        clients.get(&ProxySettings::default()).unwrap();
        assert_eq!(count(), 1);
        clients.get(&proxy).unwrap();
        clients.get(&proxy).unwrap();
        assert_eq!(count(), 2);
        clients.get(&ProxySettings::default()).unwrap();
        assert_eq!(count(), 3);
    }

    #[test]
    fn only_forwarded_headers_are_captured() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("etag", "\"v1\"".parse().unwrap());
        headers.append("cache-control", "max-age=60".parse().unwrap());
        headers.append("cache-control", "immutable".parse().unwrap());
        headers.insert("set-cookie", "session=secret".parse().unwrap());

        let captured = ResponseHeaders::capture(&headers);
        let names: Vec<&str> = captured.0.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"set-cookie"));

        let rebuilt = captured.to_header_map();
        assert_eq!(rebuilt.get("etag").unwrap(), "\"v1\"");
        assert_eq!(rebuilt.get_all("cache-control").iter().count(), 2);
    }

    /// The server does not take the child's word for which headers it
    /// sent: anything outside the forwarded set is dropped on the way in.
    #[test]
    fn unexpected_headers_are_ignored_when_rebuilding() {
        let headers = ResponseHeaders(vec![
            ("set-cookie".to_string(), "x".to_string()),
            ("etag".to_string(), "\"ok\"".to_string()),
            ("expires".to_string(), "bad\nvalue".to_string()),
        ]);
        let map = headers.to_header_map();
        assert!(map.get("set-cookie").is_none());
        assert!(map.get("expires").is_none());
        assert_eq!(map.get("etag").unwrap(), "\"ok\"");
    }
}
