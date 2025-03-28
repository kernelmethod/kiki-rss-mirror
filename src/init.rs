use crate::db::ConnectionBuilder;
use anyhow::{bail, Context, Result};
use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct InitArgs {
    /// The directory that Kiki's files should be set up in
    directory: PathBuf,

    /// Do nothing if Kiki has already been configured
    #[arg(short, long, conflicts_with = "force")]
    check: bool,

    /// Force Kiki to overwrite existing files. This option is destructive!
    #[arg(long, conflicts_with = "check")]
    force: bool,
}

impl InitArgs {
    /// Run the `init` subcommand
    pub fn run(&self) -> Result<()> {
        let db_path = Path::new(&self.directory).join("kiki.db");
        if db_path.exists() {
            if self.check {
                // Kiki has already been configured
                return Ok(());
            }

            if !self.force {
                bail!("A database has already been set up at {:#?}", &db_path);
            }

            fs::remove_file(&db_path)
                .with_context(|| format!("unable to delete database file at {:#?}", &db_path))?;
        }

        ConnectionBuilder::default()
            .at_path(&db_path)
            .create()
            .build()
            .with_context(|| format!("failed to create database in {:?}", &db_path))?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Result;
    use tempdir::TempDir;

    /// Test the `--check` flag for `kiki init`.
    #[test]
    fn test_check() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let path = PathBuf::from(td.path());
        assert!((InitArgs {
            directory: path.clone(),
            check: false,
            force: false
        })
        .run()
        .is_ok());
        assert!((InitArgs {
            directory: path.clone(),
            check: false,
            force: false
        })
        .run()
        .is_err());
        assert!((InitArgs {
            directory: path.clone(),
            check: true,
            force: false
        })
        .run()
        .is_ok());

        Ok(())
    }

    /// Test the `--force` flag for `kiki init`
    #[test]
    fn test_force() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let path = PathBuf::from(td.path());
        assert!((InitArgs {
            directory: path.clone(),
            check: false,
            force: false
        })
        .run()
        .is_ok());
        assert!((InitArgs {
            directory: path.clone(),
            check: false,
            force: false
        })
        .run()
        .is_err());
        assert!((InitArgs {
            directory: path.clone(),
            check: false,
            force: true
        })
        .run()
        .is_ok());

        Ok(())
    }
}
