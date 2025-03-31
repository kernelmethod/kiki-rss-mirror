use crate::db::ConnectionBuilder;
/// Utilities for testing Kiki.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tempdir::TempDir;

pub struct TestConfig {
    td: TempDir,
}

impl TestConfig {
    pub fn new() -> Result<Self> {
        let config = TestConfig {
            td: TempDir::new("kiki_")?,
        };
        Ok(config)
    }

    pub fn init(self) -> Result<Self> {
        ConnectionBuilder::default()
            .at_path(&self.database_path())
            .create()
            .build()
            .with_context(|| "failed to initialize database")?;

        Ok(self)
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
}
