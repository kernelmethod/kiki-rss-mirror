//! Utilities for testing Kiki.
#![allow(clippy::panic)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::indexing_slicing)]
use crate::db::ConnectionBuilder;
use crate::server::ServerBuilder;
use anyhow::{bail, Context, Result};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tempdir::TempDir;
use tokio_util::sync::CancellationToken;
use tracing::debug;

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
    /// If set, the server compresses response bodies using this encoding
    /// (e.g. `"gzip"` or `"deflate"`) and includes the `Content-Encoding` header.
    pub content_encoding: Option<String>,
    /// Total number of requests received by the server.
    pub request_count: usize,
    /// Number of 200 OK responses served.
    pub full_response_count: usize,
    /// Number of 304 Not Modified responses served.
    pub not_modified_count: usize,
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
        if let Some(handle) = self.feed_server_handle.take() {
            handle.abort();
        }
    }
}

impl TestConfig {
    pub fn new() -> Result<Self> {
        let config = TestConfig {
            td: TempDir::new("kiki_")?,
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

        let rss_content = std::fs::read(Self::test_data_path("example.xml"))
            .with_context(|| "failed to read test/example.xml")?;
        let atom_content = std::fs::read(Self::test_data_path("example_atom.xml"))
            .with_context(|| "failed to read test/example_atom.xml")?;

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

        let rss_content = Arc::new(
            std::fs::read(Self::test_data_path("example.xml"))
                .with_context(|| "failed to read test/example.xml")?,
        );

        async fn rss_handler(
            headers: HeaderMap,
            State(hs): State<HandlerState>,
        ) -> impl IntoResponse {
            let mut s = hs.config.lock().unwrap();
            s.request_count += 1;

            let if_none_match = headers.get("if-none-match").and_then(|v| v.to_str().ok());
            let if_modified_since = headers
                .get("if-modified-since")
                .and_then(|v| v.to_str().ok());

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
                return StatusCode::NOT_MODIFIED.into_response();
            }

            s.full_response_count += 1;

            // Capture header values before releasing the lock.
            let etag = s.etag.clone();
            let last_modified = s.last_modified.clone();
            let expires = s.expires.clone();
            let cache_control = s.cache_control.clone();
            let content_encoding = s.content_encoding.clone();
            drop(s);

            let body = hs.rss_content.as_ref().clone();
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
            if let Some(ref cc) = cache_control {
                response
                    .headers_mut()
                    .insert(axum::http::header::CACHE_CONTROL, cc.parse().unwrap());
            }
            if let Some(ref enc) = content_encoding {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_ENCODING, enc.parse().unwrap());
            }
            response
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
        let p = self.socket_path();

        // The server may take a little bit of time to start up.
        // We spin and wait until it's available.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if !p.exists() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }

            return Ok(reqwest::Client::builder().unix_socket(p).build()?);
        }

        bail!("HTTP server has not been started on {:?}", &p);
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

    pub fn config_dir(&self) -> &Path {
        self.td.path()
    }

    pub fn database_path(&self) -> PathBuf {
        PathBuf::from(self.config_dir()).join("kiki.db")
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
