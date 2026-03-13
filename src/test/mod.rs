/// Utilities for testing Kiki.
use crate::db::ConnectionBuilder;
use crate::server::ServerBuilder;
use anyhow::{bail, Context, Result};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use tempdir::TempDir;
use tokio_util::sync::CancellationToken;
use tracing::debug;

pub struct TestBuilder {
    init_database: bool,
    init_server: bool,
}

impl Default for TestBuilder {
    fn default() -> Self {
        TestBuilder {
            init_database: false,
            init_server: false,
        }
    }
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
        if let Some(_) = self.server_token {
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
