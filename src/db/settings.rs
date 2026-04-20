/// Helpers for reading global settings stored in the `settings` table.
use anyhow::{Context, Result};
use rusqlite::Connection;

const FEED_UPDATE_TIMEOUT_KEY: &str = "feed_update_timeout_seconds";

/// Returns the configured feed update HTTP request timeout in seconds.
///
/// The default value is seeded by `init.sql`; a missing row here is treated
/// as a schema integrity error.
pub fn get_feed_update_timeout_seconds(conn: &Connection) -> Result<u64> {
    let value: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [FEED_UPDATE_TIMEOUT_KEY],
            |row| row.get(0),
        )
        .with_context(|| "failed to read feed_update_timeout_seconds setting")?;

    value
        .parse::<u64>()
        .with_context(|| format!("invalid feed_update_timeout_seconds value: {}", value))
}
