use anyhow::{Context, Result};
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: &'static str = "1.0";

/// Run the `init` subcommand for Kiki.
pub fn init(directory: &PathBuf, check: bool, force: bool) -> Result<()> {
    let db_path = Path::new(directory).join("kiki.sqlite");

    if db_path.exists() {
        if check {
            // Kiki has already been configured
            return Ok(());
        }

        if !force {
            eprintln!("A database has already been set up at {:#?}", &db_path);
            eprintln!("Add --force to make Kiki overwrite existing files");
            return Ok(());
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

    #[derive(Debug)]
    struct SchemaInfo {
        version: String,
    }

    #[test]
    fn test_init_database() -> Result<()> {
        let conn = Connection::open_in_memory()
            .with_context(|| "Unable to open connection to database")?;

        init_database(&conn)?;

        // Check that schema information was set correctly
        let mut stmt = conn.prepare("SELECT version FROM schema_version")?;
        let schema_info = stmt
            .query_row([], |row| {
                Ok(SchemaInfo {
                    version: row.get(0)?,
                })
            })
            .with_context(|| "Could not retrieve version information from schema_version")?;

        assert_eq!(schema_info.version, SCHEMA_VERSION);

        Ok(())
    }
}
