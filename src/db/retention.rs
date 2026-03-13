/// Retention policy helpers for cleaning up old entries.
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;

const SETTING_KEY: &str = "retention_max_age_days";

/// Returns the configured `retention_max_age_days` value, or `None` if unset.
pub fn get_max_age_days(conn: &Connection) -> Result<Option<i64>> {
    let mut stmt = conn
        .prepare("SELECT value FROM settings WHERE key = ?1")
        .with_context(|| "failed to prepare settings query")?;

    let result: Option<String> = stmt.query_row([SETTING_KEY], |row| row.get(0)).ok();

    match result {
        Some(val) => {
            let days = val
                .parse::<i64>()
                .with_context(|| format!("invalid retention_max_age_days value: {}", val))?;
            Ok(Some(days))
        }
        None => Ok(None),
    }
}

/// Sets the `retention_max_age_days` value. Pass `None` to disable retention.
pub fn set_max_age_days(conn: &Connection, days: Option<i64>) -> Result<()> {
    match days {
        Some(d) => {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![SETTING_KEY, d.to_string()],
            )
            .with_context(|| "failed to upsert retention setting")?;
        }
        None => {
            conn.execute("DELETE FROM settings WHERE key = ?1", [SETTING_KEY])
                .with_context(|| "failed to delete retention setting")?;
        }
    }
    Ok(())
}

/// Delete entries older than the configured retention period across all feeds.
///
/// Returns the number of deleted entries. Returns `Ok(0)` if no retention
/// policy is configured.
pub fn cleanup_all(conn: &Connection) -> Result<usize> {
    let max_age_days = match get_max_age_days(conn)? {
        Some(d) => d,
        None => return Ok(0),
    };

    let cutoff = Utc::now().timestamp() - max_age_days * 86400;
    let deleted = conn
        .execute("DELETE FROM entries WHERE published_at < ?1", [cutoff])
        .with_context(|| "failed to delete old entries")?;

    Ok(deleted)
}

/// Delete entries older than the configured retention period for a single feed.
///
/// Returns the number of deleted entries. Returns `Ok(0)` if no retention
/// policy is configured.
pub fn cleanup_feed(conn: &Connection, feed_id: i64) -> Result<usize> {
    let max_age_days = match get_max_age_days(conn)? {
        Some(d) => d,
        None => return Ok(0),
    };

    let cutoff = Utc::now().timestamp() - max_age_days * 86400;
    let deleted = conn
        .execute(
            "DELETE FROM entries WHERE published_at < ?1 AND feed_id = ?2",
            rusqlite::params![cutoff, feed_id],
        )
        .with_context(|| format!("failed to delete old entries for feed {}", feed_id))?;

    Ok(deleted)
}
