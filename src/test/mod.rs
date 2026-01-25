/// Utilities for testing Kiki.
use crate::db::ConnectionBuilder;
use crate::server::ServerBuilder;
use anyhow::{bail, Context, Result};
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use tempdir::TempDir;
use tokio_util::sync::CancellationToken;

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
}

impl TestConfig {
    pub fn new() -> Result<Self> {
        let config = TestConfig {
            td: TempDir::new("kiki_")?,
            server_handle: None,
            server_token: None,
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
        println!("starting server at {:?}", &self.socket_path());
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

    pub fn example_feed_url(&self) -> String {
        let mut cwd = std::env::current_dir().unwrap();
        cwd.push("test");
        cwd.push("example.xml");
        format!("file://{}", cwd.into_os_string().into_string().unwrap())
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
