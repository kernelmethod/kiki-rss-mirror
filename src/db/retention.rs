/// Retention policy helpers for cleaning up old entries.
///
/// The policy itself is [`crate::config::RetentionSettings`]; callers pass
/// its `max_age_days` in.
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;

/// Delete entries older than `max_age_days` across all feeds.
///
/// Returns the number of deleted entries. Returns `Ok(0)` if `max_age_days`
/// is `None`, i.e. no retention policy is configured.
pub fn cleanup_all(conn: &Connection, max_age_days: Option<i64>) -> Result<usize> {
    let Some(max_age_days) = max_age_days else {
        return Ok(0);
    };

    let cutoff = cutoff_timestamp(max_age_days);
    let deleted = conn
        .execute("DELETE FROM entries WHERE published_at < ?1", [cutoff])
        .with_context(|| "failed to delete old entries")?;

    Ok(deleted)
}

/// Delete entries older than `max_age_days` for a single feed.
///
/// Returns the number of deleted entries. Returns `Ok(0)` if `max_age_days`
/// is `None`, i.e. no retention policy is configured.
pub fn cleanup_feed(conn: &Connection, feed_id: i64, max_age_days: Option<i64>) -> Result<usize> {
    let Some(max_age_days) = max_age_days else {
        return Ok(0);
    };

    let cutoff = cutoff_timestamp(max_age_days);
    let deleted = conn
        .execute(
            "DELETE FROM entries WHERE published_at < ?1 AND feed_id = ?2",
            rusqlite::params![cutoff, feed_id],
        )
        .with_context(|| format!("failed to delete old entries for feed {}", feed_id))?;

    Ok(deleted)
}

/// Unix timestamp `max_age_days` before now. Saturates rather than
/// overflowing, so an absurdly large value deletes nothing instead of
/// wrapping to an arbitrary cutoff; config validation keeps values far
/// below that anyway.
fn cutoff_timestamp(max_age_days: i64) -> i64 {
    Utc::now()
        .timestamp()
        .saturating_sub(max_age_days.saturating_mul(86400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_does_not_overflow() {
        // Far enough in the past to match no entry.
        assert!(cutoff_timestamp(i64::MAX) < -(1 << 62));
        let now = Utc::now().timestamp();
        let c = cutoff_timestamp(1);
        assert!((now - 86400 - 5..=now - 86400 + 5).contains(&c));
    }
}
