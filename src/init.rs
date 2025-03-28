use anyhow::{bail, Context, Result};
use clap::Args;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: &'static str = "1.0";

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
                .with_context(|| format!("Unable to delete database file at {:#?}", &db_path))?;
        }

        let conn = Connection::open(&db_path).with_context(|| {
            format!(
                "Unable to open connection to database at path {:#?}",
                &db_path
            )
        })?;

        init_database(&conn)?;
        Ok(())
    }
}

/// Initialize Kiki's database.
pub fn init_database(conn: &Connection) -> Result<()> {
    let _ = conn
        .execute_batch(include_str!("include/init.sql"))
        .with_context(|| "Failed to initialize database")?;

    let _ = conn
        .execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            (SCHEMA_VERSION,),
        )
        .with_context(|| "Unable to add schema version metadata to database")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use itertools::Itertools;
    use std::path::PathBuf;
    use tempdir::TempDir;

    #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct QueryStringResult {
        text: String,
    }

    #[test]
    fn test_init_database() -> Result<()> {
        let conn = Connection::open_in_memory()
            .with_context(|| "Unable to open connection to database")?;

        init_database(&conn)?;

        // Check that tables were all correctly constructed
        let mut stmt = conn.prepare("SELECT tbl_name FROM sqlite_master")?;
        let tables = stmt
            .query_map([], |row| Ok(QueryStringResult { text: row.get(0)? }))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .unique()
            .sorted()
            .map(|r| r.text)
            .collect::<Vec<_>>();

        let expected_tables = [
            "schema_version",
            "tags",
            "scripts",
            "feeds",
            "feed_tags",
            "feed_scripts",
            "entries",
            "entry_tags",
        ]
        .into_iter()
        .map(|s| String::from(s))
        .sorted()
        .collect::<Vec<_>>();

        assert_eq!(tables, expected_tables);

        // Check that schema information was set correctly
        let mut stmt = conn.prepare("SELECT version FROM schema_version")?;
        let schema_info = stmt
            .query_row([], |row| Ok(QueryStringResult { text: row.get(0)? }))
            .with_context(|| "Could not retrieve version information from schema_version")?;

        assert_eq!(schema_info.text, SCHEMA_VERSION);

        Ok(())
    }

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
