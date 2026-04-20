/// Helpers for reading global settings stored in the `settings` table.
use anyhow::{Context, Result};
use rusqlite::Connection;

const FEED_UPDATE_TIMEOUT_KEY: &str = "feed_update_timeout_seconds";
const MIN_POLLING_CADENCE_KEY: &str = "min_polling_cadence_seconds";
const MAX_FEED_BACKOFF_KEY: &str = "max_feed_backoff_seconds";

fn read_u64_setting(conn: &Connection, key: &'static str) -> Result<u64> {
    let value: String = conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .with_context(|| format!("failed to read {} setting", key))?;

    value
        .parse::<u64>()
        .with_context(|| format!("invalid {} value: {}", key, value))
}

/// Returns the configured feed update HTTP request timeout in seconds.
///
/// The default value is seeded by `init.sql`; a missing row here is treated
/// as a schema integrity error.
pub fn get_feed_update_timeout_seconds(conn: &Connection) -> Result<u64> {
    read_u64_setting(conn, FEED_UPDATE_TIMEOUT_KEY)
}

/// Returns the minimum interval, in seconds, between polls of any feed.
///
/// Acts as an absolute floor on the fetch scheduler — a server advertising
/// a very short `max-age` or `Retry-After` cannot reduce the interval below
/// this value.
pub fn get_min_polling_cadence_seconds(conn: &Connection) -> Result<u64> {
    read_u64_setting(conn, MIN_POLLING_CADENCE_KEY)
}

/// Returns the cap on exponential backoff for transient errors, and the wait
/// applied to permanent errors, in seconds.
pub fn get_max_feed_backoff_seconds(conn: &Connection) -> Result<u64> {
    read_u64_setting(conn, MAX_FEED_BACKOFF_KEY)
}
