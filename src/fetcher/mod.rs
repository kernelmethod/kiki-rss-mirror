//! Retrieving and parsing feeds, with no access to Kiki's state.
//!
//! Everything a feed refresh does with untrusted bytes — the HTTP
//! exchange, TLS, decompression, reading a capped body, and parsing the
//! result as Atom or RSS — lives here, as functions of their inputs
//! alone. Nothing in this module touches the database, the asset cache,
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

pub mod parse;
pub mod retrieve;

use crate::http::{FeedAuth, USER_AGENT};
use crate::scripting::FeedEntry;
use serde::{Deserialize, Serialize};
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
    },
    Rss {
        entries: Vec<RssEntry>,
    },
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
            ParsedFeed::Rss { entries } => entries.len(),
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

/// Where feed retrieval and parsing happen.
///
/// Callers get the same [`FetchReply`] either way; the variants differ
/// only in which process holds the network client and the parser.
#[derive(Clone)]
pub enum Fetcher {
    /// In this process, with the given client. Used by the library-level
    /// tests and on platforms without an isolated fetcher.
    InProcess(reqwest::Client),

    /// In the sandboxed feed fetcher process.
    #[cfg(unix)]
    Isolated(std::sync::Arc<crate::process::feed_fetcher::FeedFetcherHost>),
}

impl Fetcher {
    /// An in-process fetcher with the client configuration production
    /// uses: no automatic redirects (they are followed by [`retrieve()`], so
    /// credentials can be dropped cross-origin) and Kiki's user agent.
    ///
    /// # Errors
    ///
    /// Fails if the TLS backend cannot be initialised.
    pub fn in_process() -> reqwest::Result<Self> {
        Ok(Fetcher::InProcess(build_client()?))
    }

    /// Fetch, and on a `200 OK` parse, the feed described by `spec`.
    ///
    /// # Errors
    ///
    /// Only the isolated variant fails, and only when the fetcher process
    /// itself could not serve the request; see [`FetcherError`].
    pub async fn fetch(&self, spec: FetchSpec) -> Result<FetchReply, FetcherError> {
        match self {
            Fetcher::InProcess(client) => Ok(retrieve(client, &spec).await),
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
            Fetcher::InProcess(_) => Ok(parse_off_thread(feed_id, body).await),
            #[cfg(unix)]
            Fetcher::Isolated(host) => host.parse(feed_id, body).await,
        }
    }
}

/// Build the HTTP client used for feed fetches.
///
/// # Errors
///
/// Fails if the TLS backend cannot be initialised.
pub fn build_client() -> reqwest::Result<reqwest::Client> {
    client_builder().build()
}

/// The client configuration shared by every feed fetch, for callers that
/// need to adjust it further — the isolated fetcher swaps in its own DNS
/// resolver.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
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
mod tests {
    use super::*;

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
