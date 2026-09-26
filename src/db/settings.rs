/// Helpers for reading global settings stored in the `settings` table.
use anyhow::{Context, Result};
use rusqlite::{params, Connection};

const FEED_UPDATE_TIMEOUT_KEY: &str = "feed_update_timeout_seconds";
const MIN_POLLING_CADENCE_KEY: &str = "min_polling_cadence_seconds";
const MAX_FEED_BACKOFF_KEY: &str = "max_feed_backoff_seconds";
const FORCE_REFRESH_AFTER_KEY: &str = "force_refresh_after_secs";
const MAX_FEED_BYTES_KEY: &str = "max_feed_bytes";

/// Value seeded into `max_feed_bytes` by `init.sql`: 32 MiB.
///
/// Feeds are text and even very long archive feeds sit far below this.
pub const DEFAULT_MAX_FEED_BYTES: u64 = 32 * 1024 * 1024;

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

/// Returns how often (in seconds) the fetcher should bypass conditional
/// request headers and force a full `GET` on a feed.
///
/// Used to catch servers that keep returning the same `ETag`/`Last-Modified`
/// while the body has actually changed; pairing a forced 200 with the body
/// hash stored on the last fetch lets us detect the mismatch.
pub fn get_force_refresh_after_secs(conn: &Connection) -> Result<u64> {
    read_u64_setting(conn, FORCE_REFRESH_AFTER_KEY)
}

/// Returns the largest feed response body, in bytes, that the fetcher will
/// read into memory.
///
/// Responses exceeding this are abandoned mid-read and recorded as
/// [`crate::tasks::error::FetchError::BodyTooLarge`], so a server streaming
/// an unbounded body cannot exhaust memory. Read once per fetch, so a
/// change takes effect without restarting the server.
///
/// The default value is seeded by `init.sql`; a missing row here is treated
/// as a schema integrity error, as with the other settings in this module.
///
/// # Examples
///
/// ```no_run
/// # use rusqlite::Connection;
/// # use kiki_rss::db::settings::get_max_feed_bytes;
/// # fn run(conn: &Connection) -> anyhow::Result<()> {
/// let cap = get_max_feed_bytes(conn)?;
/// assert!(cap > 0);
/// # Ok(())
/// # }
/// ```
pub fn get_max_feed_bytes(conn: &Connection) -> Result<u64> {
    read_u64_setting(conn, MAX_FEED_BYTES_KEY)
}

/// Sets the maximum feed response body size in bytes.
///
/// # Errors
///
/// Returns an error if `bytes` is zero — a cap of zero would reject every
/// feed — or if the write fails.
pub fn set_max_feed_bytes(conn: &Connection, bytes: u64) -> Result<()> {
    if bytes == 0 {
        anyhow::bail!("max_feed_bytes must be greater than zero");
    }
    conn.execute(
        "INSERT INTO settings (key, value, type) VALUES (?1, ?2, 'integer')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![MAX_FEED_BYTES_KEY, bytes.to_string()],
    )
    .with_context(|| "failed to upsert max_feed_bytes")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;

    fn conn() -> Connection {
        ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()
            .expect("build in-memory database")
    }

    #[test]
    fn fresh_database_seeds_the_documented_default() {
        assert_eq!(
            get_max_feed_bytes(&conn()).unwrap(),
            DEFAULT_MAX_FEED_BYTES,
            "init.sql and DEFAULT_MAX_FEED_BYTES have drifted apart"
        );
    }

    #[test]
    fn max_feed_bytes_round_trips() {
        let conn = conn();
        set_max_feed_bytes(&conn, 4096).unwrap();
        assert_eq!(get_max_feed_bytes(&conn).unwrap(), 4096);

        // Upsert, not insert: a second write replaces the first.
        set_max_feed_bytes(&conn, 8192).unwrap();
        assert_eq!(get_max_feed_bytes(&conn).unwrap(), 8192);
    }

    /// A corrupt value is a real error, not something to paper over.
    #[test]
    fn unparseable_value_is_an_error() {
        let conn = conn();
        conn.execute(
            "UPDATE settings SET value = 'not-a-number' WHERE key = 'max_feed_bytes'",
            [],
        )
        .unwrap();
        assert!(get_max_feed_bytes(&conn).is_err());
    }

    #[test]
    fn zero_max_feed_bytes_is_rejected() {
        let conn = conn();
        assert!(
            set_max_feed_bytes(&conn, 0).is_err(),
            "a zero cap would reject every feed"
        );
        assert_eq!(get_max_feed_bytes(&conn).unwrap(), DEFAULT_MAX_FEED_BYTES);
    }
}
