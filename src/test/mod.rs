//! Utilities for testing Kiki.
#![allow(clippy::panic)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::indexing_slicing)]
use crate::db::ConnectionBuilder;
use crate::server::ServerBuilder;
use anyhow::{bail, Context, Result};
use std::sync::LazyLock;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tracing::debug;

static RSS_CONTENT: LazyLock<Vec<u8>> = LazyLock::new(|| {
    std::fs::read(TestConfig::test_data_path("example.xml"))
        .expect("failed to read test/example.xml")
});
static ATOM_CONTENT: LazyLock<Vec<u8>> = LazyLock::new(|| {
    std::fs::read(TestConfig::test_data_path("example_atom.xml"))
        .expect("failed to read test/example_atom.xml")
});

/// Configurable state for the test feed server, allowing tests to control
/// which HTTP cache headers are returned and to inspect request counts.
#[derive(Default)]
pub struct FeedServerState {
    /// If set, the server includes an `ETag` response header with this value.
    pub etag: Option<String>,
    /// If set, the server includes a `Last-Modified` response header with this value.
    pub last_modified: Option<String>,
    /// If set, the server includes an `Expires` response header with this value.
    pub expires: Option<String>,
    /// If set, the server includes a `Cache-Control` response header with this value.
    pub cache_control: Option<String>,
    /// Additional `Cache-Control` header instances appended after
    /// `cache_control`. Used to simulate servers that emit the header
    /// multiple times (RFC 9110 §5.3).
    pub cache_control_extra: Vec<String>,
    /// If set, included as an `Age` response header (RFC 9111 §5.1).
    pub age: Option<u64>,
    /// If set, included as a `Date` response header (RFC 9110 §6.6.1).
    pub date: Option<String>,
    /// If set, included as a `Pragma` response header (RFC 9111 §5.4).
    pub pragma: Option<String>,
    /// If set, the server compresses response bodies using this encoding
    /// (e.g. `"gzip"` or `"deflate"`) and includes the `Content-Encoding` header.
    pub content_encoding: Option<String>,
    /// Total number of requests received by the server.
    pub request_count: usize,
    /// Number of 200 OK responses served.
    pub full_response_count: usize,
    /// Number of 304 Not Modified responses served.
    pub not_modified_count: usize,
    /// Number of requests observed to carry an `If-None-Match` header.
    pub if_none_match_count: usize,
    /// Number of requests observed to carry an `If-Modified-Since` header.
    pub if_modified_since_count: usize,
    /// Force the next `fail_next` responses to return this HTTP status
    /// instead of the normal success/304 response. Decremented on each use.
    pub fail_next: usize,
    /// Status returned while `fail_next > 0`. Defaults to 503.
    pub fail_status: u16,
    /// If set, forced failures carry this `Retry-After` header value.
    pub fail_retry_after: Option<String>,
    /// If set, this value is returned as the response body instead of the
    /// default test RSS payload. Lets a test flip content mid-run while
    /// keeping validator headers (`etag`, `last_modified`) fixed.
    pub body_override: Option<Vec<u8>>,
    /// If set, requests that do not carry this exact `Authorization` header
    /// value are answered with `401 Unauthorized`. Used to exercise
    /// per-feed authentication.
    pub require_authorization: Option<String>,
    /// The most recent `Authorization` header observed by the server, if
    /// any. Tests inspect this to assert that credentials were attached.
    pub last_authorization: Option<String>,
    /// Number of `401 Unauthorized` responses served due to missing or
    /// mismatched `Authorization` headers.
    pub unauthorized_count: usize,
    /// If set, a `304 Not Modified` carries this `ETag`. Lets a test
    /// simulate a server that rotates its validator on revalidation.
    pub not_modified_etag: Option<String>,
    /// If set, a `304 Not Modified` carries this `Last-Modified`.
    pub not_modified_last_modified: Option<String>,
    /// The most recent `If-None-Match` header observed, if any.
    pub last_if_none_match: Option<String>,
    /// The most recent `If-Modified-Since` header observed, if any.
    pub last_if_modified_since: Option<String>,
}

/// Convenience alias for the shared, mutable feed-server state.
pub type SharedFeedServerState = Arc<Mutex<FeedServerState>>;

/// Combined Axum handler state: the user-configurable [`FeedServerState`]
/// plus the static RSS content to serve on 200 responses.
#[derive(Clone)]
struct HandlerState {
    config: SharedFeedServerState,
    rss_content: Arc<Vec<u8>>,
}

#[derive(Default)]
pub struct TestBuilder {
    init_database: bool,
    init_server: bool,
}

impl TestBuilder {
    /// Create a test environment with all options enabled.
    pub fn all() -> Self {
        TestBuilder {
            init_database: true,
            init_server: true,
        }
    }

    /// Initialize the database when creating the test environment.
    pub fn init_database(mut self) -> Self {
        self.init_database = true;
        self
    }

    /// Start the server when creating the test environment.
    pub fn init_server(mut self) -> Self {
        self.init_server = true;
        self
    }

    /// Build the test environment.
    pub fn build(&self) -> Result<TestConfig> {
        let mut tc = TestConfig::new()?;

        if self.init_database {
            tc = tc.init_database()?;
        }

        if self.init_server {
            tc = tc.init_server()?;
        }

        Ok(tc)
    }
}

pub struct TestConfig {
    td: TempDir,
    pub server_handle: Option<thread::JoinHandle<Result<()>>>,
    pub server_token: Option<CancellationToken>,
    pub feed_server_handle: Option<tokio::task::JoinHandle<()>>,
    pub feed_server_addr: Option<SocketAddr>,
    pub feed_server_state: Option<SharedFeedServerState>,
}

impl Drop for TestConfig {
    fn drop(&mut self) {
        if let Some(token) = self.server_token.take() {
            token.cancel();
        }
        if let Some(handle) = self.feed_server_handle.take() {
            handle.abort();
        }
    }
}

impl TestConfig {
    pub fn new() -> Result<Self> {
        let config = TestConfig {
            td: TempDir::with_prefix("kiki_")?,
            server_handle: None,
            server_token: None,
            feed_server_handle: None,
            feed_server_addr: None,
            feed_server_state: None,
        };
        Ok(config)
    }

    pub fn init_database(self) -> Result<Self> {
        ConnectionBuilder::default()
            .at_path(&self.database_path())
            .create()
            .build()
            .with_context(|| "failed to initialize database")?;

        Ok(self)
    }

    pub fn init_server(mut self) -> Result<Self> {
        debug!("starting server at {:?}", &self.socket_path());
        if self.server_token.is_some() {
            bail!("server has already been started");
        }

        let server = ServerBuilder::new(&self.database_path())
            .socket_path(&self.socket_path())
            .single_threaded()
            .worker_count(1)
            .build();
        self.server_token = Some(server.cancel_token());
        let handle = thread::spawn(move || server.run());

        self.server_handle = Some(handle);

        Ok(self)
    }

    /// Start a lightweight HTTP server that serves test RSS and Atom feeds.
    ///
    /// The server binds to `127.0.0.1:0` (OS-assigned port) and serves:
    /// - `/rss.xml` — the contents of `test/example.xml`
    /// - `/atom.xml` — the contents of `test/example_atom.xml`
    ///
    /// Use [`rss_feed_url`] and [`atom_feed_url`] to get the URLs for each
    /// endpoint.
    pub async fn init_feed_server(&mut self) -> Result<()> {
        use axum::{routing::get, Router};

        if self.feed_server_addr.is_some() {
            bail!("feed server has already been started");
        }

        let rss_content = RSS_CONTENT.clone();
        let atom_content = ATOM_CONTENT.clone();

        let app = Router::new()
            .route(
                "/rss.xml",
                get(move || async move {
                    ([("content-type", "application/rss+xml")], rss_content)
                }),
            )
            .route(
                "/atom.xml",
                get(move || async move {
                    ([("content-type", "application/atom+xml")], atom_content)
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        self.feed_server_addr = Some(addr);
        self.feed_server_handle = Some(handle);

        Ok(())
    }

    /// Start a lightweight HTTP server that serves test feeds with configurable
    /// cache headers and conditional-request handling.
    ///
    /// The server binds to `127.0.0.1:0` (OS-assigned port) and serves:
    /// - `/rss.xml` — the contents of `test/example.xml`, with cache headers
    ///   and 304 responses driven by the provided [`SharedFeedServerState`].
    ///
    /// Use [`rss_feed_url`] to get the URL for the RSS endpoint.
    pub async fn init_feed_server_with_state(
        &mut self,
        state: SharedFeedServerState,
    ) -> Result<()> {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
            Router,
        };

        if self.feed_server_addr.is_some() {
            bail!("feed server has already been started");
        }

        let rss_content = Arc::new(RSS_CONTENT.clone());

        async fn rss_handler(
            headers: HeaderMap,
            State(hs): State<HandlerState>,
        ) -> impl IntoResponse {
            let mut s = hs.config.lock().unwrap();
            s.request_count += 1;

            let authorization = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            s.last_authorization = authorization.clone();

            if let Some(expected) = s.require_authorization.clone() {
                if authorization.as_deref() != Some(expected.as_str()) {
                    s.unauthorized_count += 1;
                    drop(s);
                    return StatusCode::UNAUTHORIZED.into_response();
                }
            }

            let if_none_match = headers.get("if-none-match").and_then(|v| v.to_str().ok());
            let if_modified_since = headers
                .get("if-modified-since")
                .and_then(|v| v.to_str().ok());
            if if_none_match.is_some() {
                s.if_none_match_count += 1;
            }
            if if_modified_since.is_some() {
                s.if_modified_since_count += 1;
            }
            s.last_if_none_match = if_none_match.map(str::to_string);
            s.last_if_modified_since = if_modified_since.map(str::to_string);

            // Forced-failure branch: return the configured error status
            // without invoking the usual 200/304 logic.
            if s.fail_next > 0 {
                s.fail_next -= 1;
                let status_u16 = if s.fail_status == 0 {
                    503
                } else {
                    s.fail_status
                };
                let cache_control = s.cache_control.clone();
                let cache_control_extra = s.cache_control_extra.clone();
                let retry_after = s.fail_retry_after.clone();
                drop(s);
                let status = StatusCode::from_u16(status_u16).unwrap_or(StatusCode::BAD_GATEWAY);
                let mut response = status.into_response();
                append_cache_control_headers(&mut response, &cache_control, &cache_control_extra);
                if let Some(ref retry_after) = retry_after {
                    response.headers_mut().insert(
                        axum::http::header::RETRY_AFTER,
                        retry_after.parse().unwrap(),
                    );
                }
                return response;
            }

            let etag_match = s
                .etag
                .as_deref()
                .zip(if_none_match)
                .is_some_and(|(a, b)| a == b);
            let lm_match = s
                .last_modified
                .as_deref()
                .zip(if_modified_since)
                .is_some_and(|(a, b)| a == b);

            if etag_match || lm_match {
                s.not_modified_count += 1;
                let cache_control = s.cache_control.clone();
                let cache_control_extra = s.cache_control_extra.clone();
                let etag = s.not_modified_etag.clone();
                let last_modified = s.not_modified_last_modified.clone();
                drop(s);
                let mut response = StatusCode::NOT_MODIFIED.into_response();
                append_cache_control_headers(&mut response, &cache_control, &cache_control_extra);
                if let Some(ref etag) = etag {
                    response
                        .headers_mut()
                        .insert(axum::http::header::ETAG, etag.parse().unwrap());
                }
                if let Some(ref lm) = last_modified {
                    response
                        .headers_mut()
                        .insert(axum::http::header::LAST_MODIFIED, lm.parse().unwrap());
                }
                return response;
            }

            s.full_response_count += 1;

            // Capture header values before releasing the lock.
            let etag = s.etag.clone();
            let last_modified = s.last_modified.clone();
            let expires = s.expires.clone();
            let cache_control = s.cache_control.clone();
            let cache_control_extra = s.cache_control_extra.clone();
            let age = s.age;
            let date = s.date.clone();
            let pragma = s.pragma.clone();
            let content_encoding = s.content_encoding.clone();
            let body_override = s.body_override.clone();
            drop(s);

            let body = body_override.unwrap_or_else(|| hs.rss_content.as_ref().clone());
            let body = if let Some(ref encoding) = content_encoding {
                compress_body(&body, encoding)
            } else {
                body
            };
            let mut response = body.into_response();
            response.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                "application/rss+xml".parse().unwrap(),
            );
            if let Some(ref etag) = etag {
                response
                    .headers_mut()
                    .insert(axum::http::header::ETAG, etag.parse().unwrap());
            }
            if let Some(ref lm) = last_modified {
                response
                    .headers_mut()
                    .insert(axum::http::header::LAST_MODIFIED, lm.parse().unwrap());
            }
            if let Some(ref exp) = expires {
                response
                    .headers_mut()
                    .insert(axum::http::header::EXPIRES, exp.parse().unwrap());
            }
            append_cache_control_headers(&mut response, &cache_control, &cache_control_extra);
            if let Some(age) = age {
                response
                    .headers_mut()
                    .insert(axum::http::header::AGE, age.to_string().parse().unwrap());
            }
            if let Some(ref d) = date {
                response
                    .headers_mut()
                    .insert(axum::http::header::DATE, d.parse().unwrap());
            }
            if let Some(ref p) = pragma {
                response
                    .headers_mut()
                    .insert(axum::http::header::PRAGMA, p.parse().unwrap());
            }
            if let Some(ref enc) = content_encoding {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_ENCODING, enc.parse().unwrap());
            }
            response
        }

        /// Append `Cache-Control` values to a response. Emits one header
        /// line per value using `append`, so multiple values surface as
        /// distinct header fields (RFC 9110 §5.3).
        fn append_cache_control_headers(
            response: &mut axum::response::Response,
            primary: &Option<String>,
            extras: &[String],
        ) {
            if let Some(ref cc) = primary {
                response
                    .headers_mut()
                    .append(axum::http::header::CACHE_CONTROL, cc.parse().unwrap());
            }
            for extra in extras {
                response
                    .headers_mut()
                    .append(axum::http::header::CACHE_CONTROL, extra.parse().unwrap());
            }
        }

        let handler_state = HandlerState {
            config: state.clone(),
            rss_content,
        };

        let app = Router::new()
            .route("/rss.xml", get(rss_handler))
            .with_state(handler_state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        self.feed_server_addr = Some(addr);
        self.feed_server_handle = Some(handle);
        self.feed_server_state = Some(state);

        Ok(())
    }

    /// Return the URL to the RSS feed served by the test feed server.
    pub fn rss_feed_url(&self) -> String {
        let addr = self
            .feed_server_addr
            .expect("feed server not initialized; call init_feed_server() first");
        format!("http://{}/rss.xml", addr)
    }

    /// Return the URL to the Atom feed served by the test feed server.
    pub fn atom_feed_url(&self) -> String {
        let addr = self
            .feed_server_addr
            .expect("feed server not initialized; call init_feed_server() first");
        format!("http://{}/atom.xml", addr)
    }

    /// Create an HTTP client to connect to the test server being run
    /// in the background.
    pub fn client(&self) -> Result<reqwest::Client> {
        Ok(self.client_builder()?.build()?)
    }

    /// Wait for the test server to accept connections, then return a client
    /// builder pointed at its socket, for tests that need to customize the
    /// client.
    pub fn client_builder(&self) -> Result<reqwest::ClientBuilder> {
        let p = self.socket_path();

        // The server may take a little bit of time to start up, so spin until
        // it accepts a connection. The socket file alone isn't enough: it
        // appears at bind(2), and connecting before the listen(2) that
        // follows fails with ECONNREFUSED.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self.server_handle.as_ref().is_some_and(|h| h.is_finished()) {
                bail!("HTTP server exited before listening on {:?}", p);
            }
            if std::os::unix::net::UnixStream::connect(&p).is_ok() {
                return Ok(reqwest::Client::builder().unix_socket(p));
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        bail!("HTTP server has not been started on {:?}", p);
    }

    /// Create an API token named `name` with `scopes` (as `kiki token
    /// create --scopes` takes them), and return the token.
    pub fn create_token(&self, name: &str, scopes: &str) -> Result<String> {
        let conn = self.database_conn()?;
        let (_, token) = crate::db::tokens::create(&conn, name, scopes.parse()?, None)?;
        Ok(token)
    }

    /// Return the path to a file in the `test/` data directory.
    pub fn test_data_path(filename: &str) -> PathBuf {
        let mut p = std::env::current_dir().unwrap();
        p.push("test");
        p.push(filename);
        p
    }

    pub fn example_feed_url(&self) -> String {
        let p = Self::test_data_path("example.xml");
        format!("file://{}", p.into_os_string().into_string().unwrap())
    }

    pub fn rich_rss_feed_url(&self) -> String {
        let p = Self::test_data_path("rich_rss.xml");
        format!("file://{}", p.into_os_string().into_string().unwrap())
    }

    pub fn rich_atom_feed_url(&self) -> String {
        let p = Self::test_data_path("rich_atom.xml");
        format!("file://{}", p.into_os_string().into_string().unwrap())
    }

    /// POST /v1/feeds/create with the given title+url, assert 201, wait for
    /// the background worker to ingest, then return the new feed's id.
    ///
    /// `url` must be a `file://` URL: the feed is parsed here too, to learn
    /// how many entries ingestion will store.
    ///
    /// Shared by the entry- and feed-route test modules.
    pub async fn add_feed_from_url(&self, title: &str, url: String) -> Result<i64> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            title: &'a str,
            url: String,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            id: i64,
        }

        let path = url
            .strip_prefix("file://")
            .with_context(|| format!("not a file:// URL: {url}"))?;
        let expected = crate::fetcher::Parsers::InProcess
            .feed(0, std::fs::read(path)?)
            .await?
            .feed
            .with_context(|| format!("failed to parse test feed {path}"))?
            .entry_count();

        let client = self.client()?;
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&Req {
                title,
                url: url.clone(),
            })
            .send()
            .await?;
        if resp.status() != reqwest::StatusCode::CREATED {
            bail!(
                "POST /v1/feeds/create returned unexpected status {:?}",
                resp.status()
            );
        }
        let id = resp.json::<Resp>().await?.id;

        // Entries are committed one at a time, so wait until all of them
        // are in rather than until the first one appears.
        let conn = self.database_conn()?;
        let start = Instant::now();
        loop {
            let stored: usize = conn.query_row(
                "SELECT count(*) FROM entries WHERE feed_id = ?1",
                [id],
                |row| row.get(0),
            )?;
            if stored >= expected {
                return Ok(id);
            }
            if start.elapsed() > Duration::from_secs(5) {
                bail!("feed {id} stored {stored} of {expected} entries from {url}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Read `name` from the test server's `/metrics` endpoint, summed over
    /// all of its label sets. `name` can also be a single series, labels
    /// included, as the exporter prints it. A metric that has not been
    /// recorded yet reads as 0.
    #[cfg(feature = "metrics")]
    pub async fn metric(&self, name: &str) -> Result<f64> {
        let body = self
            .client()?
            .get("http://localhost/metrics")
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let labelled = format!("{name}{{");
        let mut total = 0.0;
        for line in body.lines().filter(|l| !l.starts_with('#')) {
            let Some((series, value)) = line.rsplit_once(' ') else {
                continue;
            };
            if series == name || series.starts_with(&labelled) {
                total += value
                    .parse::<f64>()
                    .with_context(|| format!("bad metric line: {line}"))?;
            }
        }
        Ok(total)
    }

    /// Poll [`Self::metric`] until `done` accepts its value, and return it.
    ///
    /// # Errors
    ///
    /// Returns an error if `done` hasn't accepted a value within 5 seconds.
    #[cfg(feature = "metrics")]
    pub async fn wait_for_metric(&self, name: &str, done: impl Fn(f64) -> bool) -> Result<f64> {
        let start = Instant::now();
        loop {
            let value = self.metric(name).await?;
            if done(value) {
                return Ok(value);
            }
            if start.elapsed() > Duration::from_secs(5) {
                bail!("timed out waiting on {name} (last value: {value})");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Wait until the server has finished `n` feed fetches in total,
    /// whatever their outcome.
    #[cfg(feature = "metrics")]
    pub async fn wait_for_fetches(&self, n: u32) -> Result<()> {
        self.wait_for_metric("kiki_feed_fetch_total", |v| v >= f64::from(n))
            .await?;
        Ok(())
    }

    pub fn config_dir(&self) -> &Path {
        self.td.path()
    }

    pub fn database_path(&self) -> PathBuf {
        PathBuf::from(self.config_dir()).join("kiki.db")
    }

    /// The directory the test server discovers plugins in.
    pub fn plugins_dir(&self) -> PathBuf {
        crate::plugins::plugins_dir(self.config_dir())
    }

    /// The user plugins directory inside [`Self::plugins_dir`].
    pub fn user_plugins_dir(&self) -> PathBuf {
        crate::plugins::PluginSource::User.dir(&self.plugins_dir())
    }

    /// Install a Lua plugin named `name` whose entrypoint is `text`, with
    /// config `config`, into [`Self::user_plugins_dir`].
    pub fn install_lua_plugin(
        &self,
        name: &str,
        text: &str,
        config: serde_json::Value,
    ) -> Result<PathBuf> {
        let serde_json::Value::Object(config) = config else {
            bail!("plugin config must be a JSON object");
        };
        let manifest = crate::plugins::PluginManifest {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            engine: crate::plugins::PluginEngine::Lua,
            entrypoint: None,
            description: None,
            authors: vec![],
            license: None,
            homepage: None,
            enabled: true,
            time_budget_ms: None,
            config,
            settings: vec![],
        };
        crate::plugins::install(&self.user_plugins_dir(), &manifest, text)
    }

    pub fn socket_path(&self) -> PathBuf {
        PathBuf::from(self.config_dir()).join("kiki.sock")
    }

    pub fn database_conn(&self) -> Result<rusqlite::Connection> {
        ConnectionBuilder::default()
            .at_path(&self.database_path())
            .read_write()
            .build()
            .with_context(|| "failed to connect to database")
    }

    /// Assert database referential integrity: no FK violations, no orphaned
    /// join-table rows, and no duplicate entries (same feed_id + guid).
    ///
    /// Panics with a descriptive message on the first violation found.
    pub fn assert_db_integrity(&self) {
        let conn = self
            .database_conn()
            .expect("failed to open DB for integrity check");

        // 1. PRAGMA foreign_key_check — returns one row per violation.
        let fk_violations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check()",
                [],
                |row| row.get(0),
            )
            .expect("foreign_key_check query failed");
        assert_eq!(
            fk_violations, 0,
            "foreign key violations found: {fk_violations}"
        );

        // 2. Orphaned entry_tags (entry_id not in entries).
        let orphaned_entry_tags: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entry_tags WHERE entry_id NOT IN (SELECT id FROM entries)",
                [],
                |row| row.get(0),
            )
            .expect("orphaned entry_tags query failed");
        assert_eq!(
            orphaned_entry_tags, 0,
            "orphaned entry_tags found: {orphaned_entry_tags}"
        );

        // 3. Orphaned feed_tags (feed_id not in feeds).
        let orphaned_feed_tags: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM feed_tags WHERE feed_id NOT IN (SELECT id FROM feeds)",
                [],
                |row| row.get(0),
            )
            .expect("orphaned feed_tags query failed");
        assert_eq!(
            orphaned_feed_tags, 0,
            "orphaned feed_tags found: {orphaned_feed_tags}"
        );

        // 4. Duplicate entries (same feed_id + guid).
        let duplicate_entries: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (
                    SELECT feed_id, guid, COUNT(*) AS c
                    FROM entries GROUP BY feed_id, guid HAVING c > 1
                )",
                [],
                |row| row.get(0),
            )
            .expect("duplicate entries query failed");
        assert_eq!(
            duplicate_entries, 0,
            "duplicate entries (feed_id, guid) found: {duplicate_entries}"
        );
    }
}

/// Compress `data` using the given encoding (`"gzip"` or `"deflate"`).
///
/// Panics on unsupported encodings — this is intentional since it is only
/// used in test helpers.
fn compress_body(data: &[u8], encoding: &str) -> Vec<u8> {
    use flate2::write::{GzEncoder, ZlibEncoder};
    use flate2::Compression;
    use std::io::Write;

    match encoding {
        "gzip" => {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        }
        "deflate" => {
            // Content-Encoding: deflate uses zlib-wrapped deflate (RFC 1950),
            // not raw deflate (RFC 1951).
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        }
        other => panic!("unsupported test content encoding: {other}"),
    }
}
