use anyhow::{Context, Result};
use rusqlite::Connection;

pub const SCHEMA_VERSION: &'static str = "1.0";

pub fn init() -> Result<()> {
    let conn =
        Connection::open_in_memory().with_context(|| "Unable to open connection to database")?;

    init_database(conn)?;
    Ok(())
}

/// Initialize Kiki's database.
pub fn init_database(conn: Connection) -> Result<()> {
    let _ = conn
        .execute(include_str!("include/init.sql"), ())
        .with_context(|| "Failed to initialize database")?;

    let _ = conn
        .execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            (SCHEMA_VERSION,),
        )
        .with_context(|| "Unable to add schema version metadata to database")?;

    Ok(())
}
